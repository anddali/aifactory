//! Singleton duties guarded by a Postgres advisory lock: whichever replica
//! holds the lock runs them; when it dies, its session ends, the lock frees
//! and another replica takes over. No dedicated leader.

use crate::jobs::Job;
use crate::runs::{self, Run};
use crate::{AppState, engine, events, humans};
use anyhow::Result;
use chrono::Utc;
use proto::{RunState, TurnOutcome};
use serde_json::{Value, json};
use sqlx::Connection;
use std::time::Duration;

const LOCK_ID: i64 = 7_700_100;

pub async fn run(state: AppState) {
    loop {
        let tick = crate::settings::get(&state.pool).await.map(|s| s.tick_ms).unwrap_or(15000).max(100);
        match sqlx::PgConnection::connect(&state.cfg.database_url).await {
            Ok(mut conn) => {
                let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                    .bind(LOCK_ID)
                    .fetch_one(&mut conn)
                    .await
                    .unwrap_or(false);
                if got {
                    tracing::info!(replica = %state.cfg.replica_id, "holding singleton duties");
                    loop {
                        if let Err(e) = tick_once(&state).await {
                            tracing::warn!("singleton tick failed: {e:#}");
                        }
                        let tick = crate::settings::get(&state.pool).await.map(|s| s.tick_ms).unwrap_or(15000).max(100);
                        tokio::time::sleep(Duration::from_millis(tick)).await;
                        // Our lock lives as long as this session does.
                        if sqlx::query("SELECT 1").execute(&mut conn).await.is_err() {
                            tracing::warn!("lost singleton session");
                            break;
                        }
                    }
                } else {
                    let _ = conn.close().await;
                }
            }
            Err(e) => tracing::warn!("singleton connect failed: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(tick)).await;
    }
}

pub async fn tick_once(state: &AppState) -> Result<()> {
    let s = crate::settings::get(&state.pool).await?;
    let mut errs = vec![];
    if let Err(e) = liveness(state, &s).await {
        errs.push(format!("liveness: {e:#}"));
    }
    if let Err(e) = idle_lifecycle(state).await {
        errs.push(format!("idle: {e:#}"));
    }
    if let Err(e) = turn_limits(state).await {
        errs.push(format!("turn limits: {e:#}"));
    }
    if let Err(e) = poll_connectors(state, &s).await {
        errs.push(format!("polling: {e:#}"));
    }
    if let Err(e) = housekeeping(state, &s).await {
        errs.push(format!("housekeeping: {e:#}"));
    }
    if let Err(e) = gauges(state).await {
        errs.push(format!("gauges: {e:#}"));
    }
    if !errs.is_empty() {
        anyhow::bail!(errs.join("; "));
    }
    Ok(())
}

async fn active_runs(state: &AppState, filter: &'static str) -> Result<Vec<Run>> {
    let sql = format!("SELECT * FROM runs WHERE state NOT IN ('finished','failed','cancelled','expired') AND {filter}");
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql)).fetch_all(&state.pool).await?)
}

/// Missed heartbeats and runners that never said hello.
async fn liveness(state: &AppState, s: &crate::settings::Settings) -> Result<()> {
    let hb = s.heartbeat_secs.max(1) as i64;
    let now = Utc::now();
    for run in active_runs(state, "runner_state IN ('alive','launching')").await? {
        match run.runner_state.as_str() {
            "alive" => {
                let last = run.last_heartbeat_at.unwrap_or(run.updated_at);
                if (now - last).num_seconds() > hb * 3 {
                    tracing::warn!(run = %run.id, "runner missed heartbeats");
                    engine::runner_lost(state, &run).await?;
                    if run.runner_busy {
                        alert_stuck(state, &run, "missed_heartbeats", "the runner stopped sending heartbeats mid-turn; it was relaunched from the last checkpoint", false).await?;
                    }
                }
            }
            "launching" => {
                let since = run.runner_launched_at.unwrap_or(run.updated_at);
                if (now - since).num_seconds() > s.launch_timeout_secs as i64 {
                    tracing::warn!(run = %run.id, "runner never said hello");
                    engine::runner_lost(state, &run).await?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Reminder at 24h idle, warning at 48h, expiry at 72h (per project).
async fn idle_lifecycle(state: &AppState) -> Result<()> {
    let now = Utc::now();
    for run in active_runs(state, "state IN ('waiting','paused')").await? {
        let pc = engine::project_cfg(state, run.project_id).await?;
        let idle = (now - run.last_activity_at).num_seconds();
        let waiting_for = describe_wait(&run);
        if idle >= pc.idle_expiry_secs {
            engine::expire(state, &run).await?;
            continue;
        }
        if idle >= pc.idle_warning_secs && run.warning_sent_at.is_none() {
            let at = run.last_activity_at + chrono::Duration::seconds(pc.idle_expiry_secs);
            let n = sqlx::query("UPDATE runs SET warning_sent_at=now() WHERE id=$1 AND warning_sent_at IS NULL").bind(run.id).execute(&state.pool).await?;
            if n.rows_affected() > 0 {
                events::append(&state.pool, &*state.blobs, run.id, "warning_sent", None, json!({"expires_at": at})).await?;
                humans::notify(state, &run, &format!("warning:{}:{}", run.id, run.last_activity_at.timestamp()), &format!(
                    "This run is still waiting for {waiting_for}. It will expire at {} unless there is activity.", at.format("%Y-%m-%d %H:%M UTC")
                )).await?;
            }
            continue;
        }
        if idle >= pc.idle_reminder_secs && run.reminder_sent_at.is_none() {
            let n = sqlx::query("UPDATE runs SET reminder_sent_at=now() WHERE id=$1 AND reminder_sent_at IS NULL").bind(run.id).execute(&state.pool).await?;
            if n.rows_affected() > 0 {
                events::append(&state.pool, &*state.blobs, run.id, "reminder_sent", None, json!({"waiting_for": waiting_for})).await?;
                humans::notify(state, &run, &format!("reminder:{}:{}", run.id, run.last_activity_at.timestamp()), &format!("Reminder: this run is waiting for {waiting_for}.")).await?;
            }
        }
    }
    Ok(())
}

fn describe_wait(run: &Run) -> String {
    match (run.state.as_str(), run.waiting_for.as_deref()) {
        ("paused", _) => format!("an operator to resume it (paused: {})", run.pause_reason.clone().unwrap_or_default()),
        (_, Some("human_answer")) => "your answer to its question".into(),
        (_, Some("approval")) => "your approval".into(),
        (_, Some("review")) => "a review of its pull request".into(),
        (_, Some("ci")) => "CI results".into(),
        (_, Some("cap_decision")) => "a decision on its cap (raise it or end the run)".into(),
        _ => "input".into(),
    }
}

/// A turn running far past its wall-clock limit (supervisor failed to stop it).
async fn turn_limits(state: &AppState) -> Result<()> {
    let now = Utc::now();
    for run in active_runs(state, "runner_busy AND turn_started_at IS NOT NULL").await? {
        let started = run.turn_started_at.unwrap();
        if (now - started).num_seconds() > run.turn_timeout_secs + 120 {
            alert_stuck(state, &run, "turn_over_limit", "a turn exceeded its wall-clock limit", true).await?;
        }
    }
    Ok(())
}

pub async fn alert_stuck(state: &AppState, run: &Run, kind: &str, text: &str, pause: bool) -> Result<()> {
    let key = format!("stuck:{}:{kind}:{}", run.id, run.turns_used);
    let first = sqlx::query("INSERT INTO side_effects(key, status) VALUES ($1,'done') ON CONFLICT DO NOTHING")
        .bind(&key)
        .execute(&state.pool)
        .await?
        .rows_affected()
        > 0;
    if !first {
        return Ok(());
    }
    events::append(&state.pool, &*state.blobs, run.id, "stuck_detected", None, json!({"kind": kind, "detail": text, "paused": pause})).await?;
    if pause {
        engine::pause(state, run, kind, "stuck-detector", None).await?;
    }
    humans::notify(state, run, &format!("{key}:assignee"), &format!("Run {} detected as {kind}: {text}.{}", run.title, if pause { " It is paused." } else { "" })).await?;
    humans::notify_ops(state, run, &format!("{key}:ops"), &format!("[ops] run {} ({}) {kind}: {text}", run.id, run.title)).await?;
    Ok(())
}

/// Checks after each turn: a turn that hit its limit, repeated identical tool
/// calls, and repeated turns with no progress.
pub async fn detect_after_turn(state: &AppState, run: &Run, tc: &proto::TurnComplete) -> Result<()> {
    let s = crate::settings::get(&state.pool).await?;
    if tc.outcome == TurnOutcome::TimedOut {
        return alert_stuck(state, run, "turn_over_limit", "a turn exceeded its wall-clock limit", true).await;
    }
    // Repeated identical tool calls in the turn just ended.
    let calls: Vec<Value> = sqlx::query_scalar(
        "SELECT payload FROM events WHERE run_id=$1 AND kind IN ('harness.tool_call') AND seq > COALESCE((SELECT max(seq) FROM events WHERE run_id=$1 AND kind='turn_started'),0)",
    )
    .bind(run.id)
    .fetch_all(&state.pool)
    .await?;
    let mut counts = std::collections::HashMap::new();
    for c in &calls {
        let k = format!("{}|{}", c.get("name").and_then(|v| v.as_str()).unwrap_or(""), client::canonical_json(c.get("args").unwrap_or(&Value::Null)));
        *counts.entry(k).or_insert(0u32) += 1;
    }
    if counts.values().any(|n| *n >= s.stuck_repeat_threshold.max(2)) {
        return alert_stuck(state, run, "looping", "the agent repeated the same tool call many times in one turn", true).await;
    }
    // No progress: the last N turns made no side-effecting primitive call.
    let n = s.stuck_noprogress_turns.max(2) as i64;
    if run.turns_used as i64 >= n {
        let turn_starts: Vec<i64> = sqlx::query_scalar("SELECT seq FROM events WHERE run_id=$1 AND kind='turn_started' ORDER BY seq DESC LIMIT $2")
            .bind(run.id)
            .bind(n)
            .fetch_all(&state.pool)
            .await?;
        if let Some(from) = turn_starts.last() {
            let progress: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM events WHERE run_id=$1 AND seq >= $2 AND kind IN ('primitive_call','git_push') AND (payload->>'side_effect')::bool IS TRUE AND (payload->>'ok')::bool IS TRUE",
            )
            .bind(run.id)
            .bind(from)
            .fetch_one(&state.pool)
            .await?;
            if progress == 0 && turn_starts.len() as i64 == n {
                return alert_stuck(state, run, "no_progress", &format!("{n} turns in a row made no progress"), true).await;
            }
        }
    }
    Ok(())
}

/// Reconciliation polling for ticket sources and repo hosts.
async fn poll_connectors(state: &AppState, s: &crate::settings::Settings) -> Result<()> {
    let due: bool = {
        let last: Option<Value> = sqlx::query_scalar("SELECT value FROM poll_state WHERE key='last_poll'")
            .fetch_optional(&state.pool)
            .await?;
        let last_ms = last.and_then(|v| v.as_i64()).unwrap_or(0);
        let now_ms = Utc::now().timestamp_millis();
        let interval = (s.poll_secs.max(1) * 1000) as i64;
        now_ms - last_ms >= interval
    };
    if !due {
        return Ok(());
    }
    sqlx::query("INSERT INTO poll_state(key,value) VALUES ('last_poll',$1) ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value, updated_at=now()")
        .bind(json!(Utc::now().timestamp_millis()))
        .execute(&state.pool)
        .await?;
    let tickets: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT id FROM connectors WHERE role='ticket'").fetch_all(&state.pool).await?;
    for cid in tickets {
        let key = format!("ticket:{cid}");
        let cursor: Option<Value> = sqlx::query_scalar("SELECT value FROM poll_state WHERE key=$1").bind(&key).fetch_optional(&state.pool).await?;
        let cursor = cursor.and_then(|v| v.as_str().map(String::from));
        let src = match crate::connectors::ticket_source(state, cid).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        match src.poll(cursor.as_deref()).await {
            Ok((evs, next)) => {
                for ev in evs {
                    state.jobs.push(Job::TicketEvent { connector_id: cid, event: ev.clone() }, Some(format!("tev:{cid}:{}", ev.id))).await?;
                }
                if let Some(n) = next {
                    sqlx::query("INSERT INTO poll_state(key,value) VALUES ($1,$2) ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value, updated_at=now()")
                        .bind(&key)
                        .bind(json!(n))
                        .execute(&state.pool)
                        .await?;
                }
            }
            Err(e) => tracing::warn!("polling ticket connector {cid}: {e:#}"),
        }
    }
    let repos: Vec<(uuid::Uuid, String)> = sqlx::query_as("SELECT DISTINCT connector_id, full_name FROM repositories").fetch_all(&state.pool).await?;
    for (cid, name) in repos {
        let key = format!("repo:{cid}:{name}");
        let cursor: Option<Value> = sqlx::query_scalar("SELECT value FROM poll_state WHERE key=$1").bind(&key).fetch_optional(&state.pool).await?;
        let cursor = cursor.and_then(|v| v.as_str().map(String::from));
        let host = match crate::connectors::repo_host(state, cid).await {
            Ok(h) => h,
            Err(_) => continue,
        };
        match host.poll(&name, cursor.as_deref()).await {
            Ok((evs, next)) => {
                for ev in evs {
                    state.jobs.push(Job::RepoEvent { connector_id: cid, event: ev.clone() }, Some(format!("rev:{cid}:{}", ev.id))).await?;
                }
                if let Some(n) = next {
                    sqlx::query("INSERT INTO poll_state(key,value) VALUES ($1,$2) ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value, updated_at=now()")
                        .bind(&key)
                        .bind(json!(n))
                        .execute(&state.pool)
                        .await?;
                }
            }
            Err(e) => tracing::warn!("polling repo {name}: {e:#}"),
        }
    }
    Ok(())
}

/// Partitions, retention and checkpoint purge (hourly).
async fn housekeeping(state: &AppState, s: &crate::settings::Settings) -> Result<()> {
    let last: Option<Value> = sqlx::query_scalar("SELECT value FROM poll_state WHERE key='housekeeping'").fetch_optional(&state.pool).await?;
    let now = Utc::now().timestamp();
    if let Some(l) = last.and_then(|v| v.as_i64()) {
        if now - l < 3600 {
            return Ok(());
        }
    }
    sqlx::query("INSERT INTO poll_state(key,value) VALUES ('housekeeping',$1) ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value")
        .bind(json!(now))
        .execute(&state.pool)
        .await?;
    crate::events::maintain_partitions(&state.pool, s.event_retention_days).await?;
    purge_checkpoints(state).await?;
    sqlx::query("DELETE FROM sessions WHERE expires_at < now()").execute(&state.pool).await?;
    Ok(())
}

/// Delete checkpoints of expired runs once their retention has passed.
pub async fn purge_checkpoints(state: &AppState) -> Result<u64> {
    let runs: Vec<Run> = sqlx::query_as("SELECT * FROM runs WHERE state='expired' AND session_blob IS NOT NULL").fetch_all(&state.pool).await?;
    let mut n = 0;
    for run in runs {
        let pc = engine::project_cfg(state, run.project_id).await?;
        let expired_at = run.expired_at.unwrap_or(run.updated_at);
        if Utc::now() - expired_at > chrono::Duration::days(pc.checkpoint_retention_days) {
            if let Some(h) = &run.session_blob {
                let others: i64 = sqlx::query_scalar("SELECT count(*) FROM runs WHERE session_blob=$1 AND id<>$2").bind(h).bind(run.id).fetch_one(&state.pool).await?;
                if others == 0 {
                    state.blobs.delete(h).await?;
                }
            }
            let mut data = run.data.clone();
            data["checkpoint_purged"] = json!(true);
            sqlx::query("UPDATE runs SET session_blob=NULL, data=$2 WHERE id=$1").bind(run.id).bind(&data).execute(&state.pool).await?;
            events::append(&state.pool, &*state.blobs, run.id, "checkpoint_purged", None, json!({})).await?;
            n += 1;
        }
    }
    Ok(n)
}

async fn gauges(state: &AppState) -> Result<()> {
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT state, count(*) FROM runs GROUP BY state").fetch_all(&state.pool).await?;
    for st in ["queued", "starting", "running", "waiting", "paused", "finished", "failed", "cancelled", "expired"] {
        let n = rows.iter().find(|(s, _)| s == st).map(|(_, n)| *n).unwrap_or(0);
        crate::metrics::RUNS_BY_STATE.with_label_values(&[st]).set(n);
    }
    let _ = RunState::Waiting;
    let _ = runs::rebuild_state;
    Ok(())
}
