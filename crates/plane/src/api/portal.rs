//! Portal back end. Contract in docs/API.md.

use super::auth::User;
use super::{ApiError, ApiResult, bad};
use crate::engine::{self, CreateError, NewRun};
use crate::runs::{self, Run};
use crate::{AppState, events, humans};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/runs", get(list_runs).post(create_run))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/events", get(run_events))
        .route("/api/runs/{id}/stream", get(stream))
        .route("/api/runs/{id}/steer", post(steer))
        .route("/api/runs/{id}/pause", post(pause))
        .route("/api/runs/{id}/resume", post(resume))
        .route("/api/runs/{id}/cancel", post(cancel))
        .route("/api/runs/{id}/revive", post(revive))
        .route("/api/runs/{id}/teardown", post(teardown))
        .route("/api/runs/{id}/cap", post(cap))
        .route("/api/runs/{id}/rebuild", get(rebuild))
        .route("/api/questions/{id}/answer", post(answer))
        .route("/api/connectors", get(list_connectors).post(create_connector))
        .route("/api/connectors/{id}", put(update_connector).delete(delete_connector))
        .route("/api/connectors/{id}/secrets", put(put_secrets))
        .route("/api/connectors/{id}/test", post(test_connector))
        .route("/api/connectors/{id}/repos", get(connector_repos))
        .route("/api/connector-kinds", get(connector_kinds))
        .route("/api/projects", get(list_projects).post(create_project))
        .route("/api/projects/{id}", get(get_project).put(update_project))
        .route("/api/projects/{id}/repos", get(project_repos).post(onboard_repo))
        .route("/api/repos/{id}", put(update_repo).delete(delete_repo))
        .route("/api/templates", get(templates))
        .route("/api/projects/{id}/templates/{workflow}", put(put_template))
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/{id}/identities", put(put_identities))
        .route("/api/users/{id}/grants", put(put_grants))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/settings/rotate-key", post(rotate_secrets))
        .route("/api/audit", get(audit))
        .route("/api/metrics/outcomes", get(outcomes))
        .route("/api/_tick", post(force_tick))
        .route("/api/_purge", post(force_purge))
        .route("/api/_unused", delete(|| async { "" }))
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

async fn run_summary(state: &AppState, run: &Run, viewer: Option<&User>) -> ApiResult<Value> {
    let pkey: String = sqlx::query_scalar("SELECT key FROM projects WHERE id=$1").bind(run.project_id).fetch_one(&state.pool).await?;
    let repo: Option<String> = match run.repo_id {
        Some(id) => sqlx::query_scalar("SELECT full_name FROM repositories WHERE id=$1").bind(id).fetch_optional(&state.pool).await?,
        None => None,
    };
    let assignee: Option<(Uuid, String, String)> = match run.assignee_user_id {
        Some(id) => sqlx::query_as("SELECT id, email, display_name FROM users WHERE id=$1").bind(id).fetch_optional(&state.pool).await?,
        None => None,
    };
    let needs_human = run.state == "waiting" && matches!(run.waiting_for.as_deref(), Some("human_answer" | "approval" | "cap_decision")) || run.state == "paused" && run.pause_reason.as_deref() != Some("operator") || run.state == "expired";
    let needs_you = needs_human
        && match viewer {
            Some(v) => run.assignee_user_id == Some(v.id) || run.assignee_user_id.is_none() || v.is_global_admin,
            None => true,
        };
    Ok(json!({
        "id": run.id, "title": run.title, "project_id": run.project_id, "project_key": pkey, "workflow": run.workflow,
        "state": run.state, "waiting_for": run.waiting_for, "pause_reason": run.pause_reason,
        "spent_usd": run.spent_usd, "budget_usd": run.budget_usd, "turns_used": run.turns_used, "max_turns": run.max_turns,
        "needs_you": needs_you, "ticket_key": run.ticket_key, "repo": repo, "pr_number": run.pr_number, "pr_url": run.pr_url,
        "branch": run.branch, "assignee": assignee.map(|(id, e, n)| json!({"id": id, "email": e, "display_name": n})),
        "model": run.model, "harness": run.harness, "created_at": run.created_at, "updated_at": run.updated_at,
        "last_activity_at": run.last_activity_at, "outcome": run.outcome, "review_rounds": run.review_rounds,
        "tokens_in": run.tokens_in, "tokens_out": run.tokens_out, "expired_at": run.expired_at, "finished_at": run.finished_at,
    }))
}

#[derive(Deserialize)]
struct ListQ {
    state: Option<String>,
    project_id: Option<Uuid>,
    limit: Option<i64>,
}

async fn visible_projects(state: &AppState, user: &User) -> ApiResult<Option<Vec<Uuid>>> {
    if user.is_global_admin {
        return Ok(None);
    }
    let global: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM role_grants WHERE user_id=$1 AND project_id IS NULL)").bind(user.id).fetch_one(&state.pool).await?;
    if global {
        return Ok(None);
    }
    Ok(Some(sqlx::query_scalar("SELECT DISTINCT project_id FROM role_grants WHERE user_id=$1 AND project_id IS NOT NULL").bind(user.id).fetch_all(&state.pool).await?))
}

async fn list_runs(State(state): State<AppState>, user: User, Query(q): Query<ListQ>) -> ApiResult<Json<Value>> {
    let filter = match q.state.as_deref() {
        Some("active") => "state NOT IN ('finished','failed','cancelled','expired')",
        Some("done") => "state IN ('finished','failed','cancelled','expired')",
        _ => "true",
    };
    let sql = format!("SELECT * FROM runs WHERE {filter} AND ($1::uuid IS NULL OR project_id=$1) ORDER BY created_at DESC LIMIT $2");
    let rows: Vec<Run> = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(q.project_id).bind(q.limit.unwrap_or(100).clamp(1, 1000)).fetch_all(&state.pool).await?;
    let visible = visible_projects(&state, &user).await?;
    let mut out = vec![];
    for r in rows.iter().filter(|r| visible.as_ref().map(|v| v.contains(&r.project_id)).unwrap_or(true)) {
        out.push(run_summary(&state, r, Some(&user)).await?);
    }
    Ok(Json(json!({"runs": out})))
}

async fn create_run(State(state): State<AppState>, user: User, Json(req): Json<NewRun>) -> ApiResult<Json<Value>> {
    user.require(&state, Some(req.project_id), "operate").await?;
    let mut req = req;
    if req.assignee_email.is_none() {
        req.assignee_email = Some(user.email.clone());
    }
    match engine::create_run(&state, req, &user.email, Some(user.id)).await {
        Ok(run) => Ok(Json(run_summary(&state, &run, Some(&user)).await?)),
        Err(CreateError::AlreadyActive(id)) => Err(ApiError(StatusCode::CONFLICT, format!("work item already has an active run {id}"))),
        Err(CreateError::Refused(m)) => Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, m)),
        Err(CreateError::Other(e)) => Err(e.into()),
    }
}

async fn load_run(state: &AppState, user: &User, id: Uuid, role: &str) -> ApiResult<Run> {
    let run = runs::find(state, id).await?.ok_or(ApiError(StatusCode::NOT_FOUND, "run not found".into()))?;
    user.require(state, Some(run.project_id), role).await?;
    Ok(run)
}

async fn get_run(State(state): State<AppState>, user: User, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let run = load_run(&state, &user, id, "view").await?;
    let mut v = run_summary(&state, &run, Some(&user)).await?;
    let artifacts: Vec<(Uuid, String, String, Option<String>, Option<String>, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT id, kind, title, url, content, created_at FROM artifacts WHERE run_id=$1 ORDER BY created_at").bind(id).fetch_all(&state.pool).await?;
    let questions: Vec<humans::Question> = sqlx::query_as("SELECT * FROM questions WHERE run_id=$1 ORDER BY created_at").bind(id).fetch_all(&state.pool).await?;
    let mut qs = vec![];
    for q in questions {
        let mut qv = serde_json::to_value(&q).unwrap_or_default();
        if let Some(u) = q.answered_by {
            qv["answered_by"] = json!(humans::user_label(&state, u).await);
        }
        if let Value::Object(m) = &mut qv {
            m.remove("chat_ref");
        }
        qs.push(qv);
    }
    let cost: Vec<(String, i64, i64, f64)> = sqlx::query_as("SELECT model, sum(input_tokens)::int8, sum(output_tokens)::int8, sum(cost_usd)::float8 FROM usage WHERE run_id=$1 GROUP BY model").bind(id).fetch_all(&state.pool).await?;
    let m = v.as_object_mut().unwrap();
    m.insert("artifacts".into(), json!(artifacts.into_iter().map(|(id, k, t, u, c, at)| json!({"id": id, "kind": k, "title": t, "url": u, "content": c, "created_at": at})).collect::<Vec<_>>()));
    m.insert("questions".into(), json!(qs));
    m.insert("cost_by_model".into(), json!(cost.into_iter().map(|(model, i, o, c)| json!({"model": model, "input_tokens": i, "output_tokens": o, "cost_usd": c})).collect::<Vec<_>>()));
    m.insert("event_count".into(), json!(run.event_seq));
    m.insert("chat_thread".into(), json!(run.chat_thread));
    m.insert("template_version".into(), json!(run.template_version));
    m.insert("policy_version".into(), json!(run.policy_version));
    m.insert("config_version".into(), json!(run.config_version));
    m.insert("brief".into(), json!(run.brief));
    m.insert("scope".into(), json!(run.scope));
    m.insert("runner".into(), json!({"state": run.runner_state, "generation": run.runner_generation, "handle": run.runner_handle, "last_heartbeat_at": run.last_heartbeat_at, "image": run.runner_image, "busy": run.runner_busy, "wip_commit": run.wip_commit, "has_session": run.session_blob.is_some()}));
    Ok(Json(v))
}

#[derive(Deserialize)]
struct EvQ {
    after: Option<i64>,
    before: Option<i64>,
    limit: Option<i64>,
}

async fn run_events(State(state): State<AppState>, user: User, Path(id): Path<Uuid>, Query(q): Query<EvQ>) -> ApiResult<Json<Value>> {
    let run = load_run(&state, &user, id, "view").await?;
    let evs = events::list(&state.pool, id, q.after, q.before, q.limit.unwrap_or(200)).await?;
    Ok(Json(json!({"events": evs, "total": run.event_seq, "max_seq": run.event_seq})))
}

async fn stream(State(state): State<AppState>, user: User, Path(id): Path<Uuid>) -> ApiResult<Sse<impl futures::Stream<Item = Result<SseEvent, Infallible>>>> {
    let run = load_run(&state, &user, id, "view").await?;
    let mut rx = state.hub.subscribe();
    let st = state.clone();
    let mut last = run.event_seq;
    let s = async_stream::stream! {
        loop {
            let note = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await;
            let mut changed = false;
            match note {
                Ok(Ok(crate::notify::Note::Event { run_id, .. })) if run_id == id => {}
                Ok(Ok(crate::notify::Note::Run { run_id })) if run_id == id => changed = true,
                Ok(Ok(_)) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                _ => {}
            }
            if let Ok(evs) = events::list(&st.pool, id, Some(last), None, 500).await {
                for e in evs {
                    last = e.seq;
                    if e.kind == "state_changed" { changed = true; }
                    yield Ok(SseEvent::default().event("event").data(serde_json::to_string(&e).unwrap_or_default()));
                }
            }
            if changed {
                if let Ok(r) = runs::get(&st, id).await {
                    if let Ok(v) = run_summary(&st, &r, None).await {
                        yield Ok(SseEvent::default().event("state").data(v.to_string()));
                    }
                }
            }
        }
    };
    Ok(Sse::new(s).keep_alive(KeepAlive::default()))
}

#[derive(Deserialize)]
struct SteerReq {
    text: String,
    #[serde(default)]
    interrupt: bool,
}

async fn steer(State(state): State<AppState>, user: User, Path(id): Path<Uuid>, Json(req): Json<SteerReq>) -> ApiResult<Json<Value>> {
    let run = load_run(&state, &user, id, "operate").await?;
    events::append(&state.pool, &*state.blobs, run.id, "human_message", Some(&user.email), json!({"text": req.text, "direction": "from_human", "via": "portal"})).await?;
    engine::steer(&state, &run, &req.text, req.interrupt, &user.email, Some(user.id)).await?;
    Ok(Json(json!({})))
}

async fn pause(State(state): State<AppState>, user: User, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let run = load_run(&state, &user, id, "operate").await?;
    engine::pause(&state, &run, "operator", &user.email, Some(user.id)).await?;
    Ok(Json(json!({})))
}

async fn resume(State(state): State<AppState>, user: User, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let run = load_run(&state, &user, id, "operate").await?;
    engine::resume(&state, &run, &user.email, Some(user.id)).await?;
    Ok(Json(json!({})))
}

async fn cancel(State(state): State<AppState>, user: User, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let run = load_run(&state, &user, id, "operate").await?;
    engine::cancel(&state, &run, "cancelled by operator", &user.email, Some(user.id)).await?;
    Ok(Json(json!({})))
}

async fn revive(State(state): State<AppState>, user: User, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let run = load_run(&state, &user, id, "operate").await?;
    engine::revive(&state, &run, &user.email, Some(user.id)).await?;
    Ok(Json(json!({})))
}

async fn teardown(State(state): State<AppState>, user: User, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let run = load_run(&state, &user, id, "operate").await?;
    engine::operator_teardown(&state, &run, &user.email, Some(user.id)).await?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct CapReq {
    action: String,
    max_turns: Option<i32>,
    budget_usd: Option<f64>,
    review_rounds: Option<i32>,
}

async fn cap(State(state): State<AppState>, user: User, Path(id): Path<Uuid>, Json(req): Json<CapReq>) -> ApiResult<Json<Value>> {
    let run = load_run(&state, &user, id, "operate").await?;
    engine::cap_decision(&state, &run, &req.action, req.max_turns, req.budget_usd, req.review_rounds, &user.email, Some(user.id)).await?;
    Ok(Json(json!({})))
}

/// The run's state rebuilt from its event log alone (for audit and tests).
async fn rebuild(State(state): State<AppState>, user: User, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let run = load_run(&state, &user, id, "view").await?;
    let evs = events::all(&state.pool, id).await?;
    let (st, wait) = runs::rebuild_state(&evs);
    Ok(Json(json!({"state": st, "waiting_for": wait, "stored_state": run.state, "stored_waiting_for": run.waiting_for})))
}

#[derive(Deserialize)]
struct AnswerReq {
    answer: Option<String>,
    decision: Option<String>,
}

async fn answer(State(state): State<AppState>, user: User, Path(id): Path<Uuid>, Json(req): Json<AnswerReq>) -> ApiResult<Json<Value>> {
    let q = humans::answer(&state, id, user.id, "portal", req.answer.as_deref(), req.decision.as_deref())
        .await
        .map_err(|e| ApiError(StatusCode::CONFLICT, format!("{e:#}")))?;
    let mut v = serde_json::to_value(&q).unwrap_or_default();
    v["answered_by"] = json!(user.email);
    if let Value::Object(m) = &mut v {
        m.remove("chat_ref");
    }
    Ok(Json(v))
}

// ---------------------------------------------------------------------------
// Connectors
// ---------------------------------------------------------------------------

async fn connector_value(state: &AppState, row: &crate::connectors::ConnectorRow) -> ApiResult<Value> {
    let secrets: Vec<(String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as("SELECT name, hint, updated_at FROM secrets WHERE connector_id=$1 ORDER BY name").bind(row.id).fetch_all(&state.pool).await?;
    let mut v = crate::connectors::connector_json(row, &secrets);
    v["created_at"] = json!(sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>("SELECT created_at FROM connectors WHERE id=$1").bind(row.id).fetch_one(&state.pool).await?);
    Ok(v)
}

async fn list_connectors(State(state): State<AppState>, user: User) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    let rows: Vec<crate::connectors::ConnectorRow> = sqlx::query_as("SELECT id, kind, role, name, config, service_identity FROM connectors ORDER BY created_at").fetch_all(&state.pool).await?;
    let mut out = vec![];
    for r in &rows {
        out.push(connector_value(&state, r).await?);
    }
    Ok(Json(json!({"connectors": out})))
}

#[derive(Deserialize)]
struct ConnReq {
    kind: Option<String>,
    name: Option<String>,
    config: Option<Value>,
    service_identity: Option<String>,
}

async fn create_connector(State(state): State<AppState>, user: User, Json(req): Json<ConnReq>) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    let kind = req.kind.ok_or_else(|| bad("kind is required"))?;
    let info = crate::connectors::kind_info(&kind).ok_or_else(|| bad(format!("unknown connector kind {kind}")))?;
    let id = Uuid::new_v4();
    let identity = req.service_identity.unwrap_or_else(|| "factory-bot".into());
    sqlx::query("INSERT INTO connectors(id, kind, role, name, config, service_identity) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(id)
        .bind(&kind)
        .bind(info.role)
        .bind(req.name.unwrap_or_else(|| kind.clone()))
        .bind(req.config.unwrap_or(json!({})))
        .bind(&identity)
        .execute(&state.pool)
        .await?;
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.connector.create", None, None, json!({"connector": id, "kind": kind})).await;
    let row: crate::connectors::ConnectorRow = sqlx::query_as("SELECT id, kind, role, name, config, service_identity FROM connectors WHERE id=$1").bind(id).fetch_one(&state.pool).await?;
    Ok(Json(connector_value(&state, &row).await?))
}

async fn update_connector(State(state): State<AppState>, user: User, Path(id): Path<Uuid>, Json(req): Json<ConnReq>) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    sqlx::query("UPDATE connectors SET name=COALESCE($2,name), config=COALESCE($3,config), service_identity=COALESCE($4, service_identity) WHERE id=$1")
        .bind(id)
        .bind(req.name)
        .bind(req.config)
        .bind(req.service_identity)
        .execute(&state.pool)
        .await?;
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.connector.update", None, None, json!({"connector": id})).await;
    let row: crate::connectors::ConnectorRow = sqlx::query_as("SELECT id, kind, role, name, config, service_identity FROM connectors WHERE id=$1").bind(id).fetch_one(&state.pool).await?;
    Ok(Json(connector_value(&state, &row).await?))
}

async fn delete_connector(State(state): State<AppState>, user: User, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    sqlx::query("DELETE FROM connectors WHERE id=$1").bind(id).execute(&state.pool).await.map_err(|_| bad("connector is in use"))?;
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.connector.delete", None, None, json!({"connector": id})).await;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct SecretsReq {
    secrets: std::collections::HashMap<String, String>,
}

pub async fn store_secret(state: &AppState, connector: Uuid, name: &str, value: &str) -> anyhow::Result<()> {
    let (kid, nonce, ct) = state.crypto.encrypt(value.as_bytes())?;
    sqlx::query(
        "INSERT INTO secrets(connector_id, name, key_id, nonce, ciphertext, hint) VALUES ($1,$2,$3,$4,$5,$6)
         ON CONFLICT (connector_id, name) DO UPDATE SET key_id=EXCLUDED.key_id, nonce=EXCLUDED.nonce, ciphertext=EXCLUDED.ciphertext, hint=EXCLUDED.hint, updated_at=now()",
    )
    .bind(connector)
    .bind(name)
    .bind(kid)
    .bind(nonce)
    .bind(ct)
    .bind(crate::crypto::hint(value))
    .execute(&state.pool)
    .await?;
    Ok(())
}

async fn put_secrets(State(state): State<AppState>, user: User, Path(id): Path<Uuid>, Json(req): Json<SecretsReq>) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    for (k, v) in &req.secrets {
        if v.is_empty() {
            sqlx::query("DELETE FROM secrets WHERE connector_id=$1 AND name=$2").bind(id).bind(k).execute(&state.pool).await?;
        } else {
            store_secret(&state, id, k, v).await?;
        }
    }
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.secret.set", None, None, json!({"connector": id, "names": req.secrets.keys().collect::<Vec<_>>()})).await;
    Ok(Json(json!({})))
}

/// Re-encrypt every secret with the active key (after adding a new key in front).
async fn rotate_secrets(State(state): State<AppState>, user: User) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    let rows: Vec<(Uuid, String, String, Vec<u8>, Vec<u8>)> = sqlx::query_as("SELECT connector_id, name, key_id, nonce, ciphertext FROM secrets").fetch_all(&state.pool).await?;
    let mut n = 0;
    for (cid, name, kid, nonce, ct) in rows {
        if kid == state.crypto.active_key_id() {
            continue;
        }
        let pt = state.crypto.decrypt(&kid, &nonce, &ct)?;
        store_secret(&state, cid, &name, &String::from_utf8_lossy(&pt)).await?;
        n += 1;
    }
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.secret.rotate", None, None, json!({"reencrypted": n})).await;
    Ok(Json(json!({"reencrypted": n, "active_key": state.crypto.active_key_id()})))
}

async fn test_connector(State(state): State<AppState>, user: User, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    let r = crate::connectors::test(&state, id).await;
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.connector.test", None, None, json!({"connector": id, "ok": r.is_ok()})).await;
    Ok(Json(match r {
        Ok(m) => json!({"ok": true, "message": m}),
        Err(e) => json!({"ok": false, "message": format!("{e:#}")}),
    }))
}

async fn connector_repos(State(state): State<AppState>, user: User, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    let host = crate::connectors::repo_host(&state, id).await?;
    Ok(Json(json!({"repos": host.list_repos().await?})))
}

async fn connector_kinds(State(_state): State<AppState>, _user: User) -> Json<Value> {
    Json(json!({"kinds": crate::connectors::KINDS.iter().map(|k| json!({"kind": k.kind, "role": k.role, "config_fields": k.config_fields, "secret_fields": k.secret_fields})).collect::<Vec<_>>()}))
}

// ---------------------------------------------------------------------------
// Projects and repositories
// ---------------------------------------------------------------------------

async fn project_value(state: &AppState, id: Uuid) -> ApiResult<Value> {
    let (key, name, config, templates, created): (String, String, Value, Value, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT key, name, config, templates, created_at FROM projects WHERE id=$1").bind(id).fetch_optional(&state.pool).await?.ok_or(ApiError(StatusCode::NOT_FOUND, "project not found".into()))?;
    Ok(json!({"id": id, "key": key, "name": name, "config": crate::settings::project_config_json(&config), "templates": templates, "created_at": created}))
}

async fn list_projects(State(state): State<AppState>, user: User) -> ApiResult<Json<Value>> {
    let visible = visible_projects(&state, &user).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM projects ORDER BY key").fetch_all(&state.pool).await?;
    let mut out = vec![];
    for id in ids.into_iter().filter(|i| visible.as_ref().map(|v| v.contains(i)).unwrap_or(true)) {
        out.push(project_value(&state, id).await?);
    }
    Ok(Json(json!({"projects": out})))
}

#[derive(Deserialize)]
struct ProjReq {
    key: Option<String>,
    name: Option<String>,
    config: Option<Value>,
}

async fn create_project(State(state): State<AppState>, user: User, Json(req): Json<ProjReq>) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    let key = req.key.ok_or_else(|| bad("key is required"))?;
    let id = Uuid::new_v4();
    let mut cfg = serde_json::to_value(crate::settings::ProjectConfig::default()).unwrap();
    if let Some(c) = &req.config {
        crate::settings::merge_json(&mut cfg, c);
    }
    let _: crate::settings::ProjectConfig = serde_json::from_value(cfg.clone()).map_err(|e| bad(format!("bad config: {e}")))?;
    sqlx::query("INSERT INTO projects(id, key, name, config) VALUES ($1,$2,$3,$4)")
        .bind(id)
        .bind(&key)
        .bind(req.name.unwrap_or_else(|| key.clone()))
        .bind(&cfg)
        .execute(&state.pool)
        .await
        .map_err(|_| bad("a project with that key exists"))?;
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.project.create", Some(id), None, json!({"key": key})).await;
    Ok(Json(project_value(&state, id).await?))
}

async fn get_project(State(state): State<AppState>, user: User, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    user.require(&state, Some(id), "view").await?;
    let mut v = project_value(&state, id).await?;
    v["repos"] = json!(engine::project_repos(&state, id).await?);
    Ok(Json(v))
}

async fn update_project(State(state): State<AppState>, user: User, Path(id): Path<Uuid>, Json(req): Json<ProjReq>) -> ApiResult<Json<Value>> {
    user.require(&state, Some(id), "administer").await?;
    let mut tx = state.pool.begin().await?;
    let cur: Value = sqlx::query_scalar("SELECT config FROM projects WHERE id=$1 FOR UPDATE").bind(id).fetch_one(&mut *tx).await?;
    let mut cfg = crate::settings::project_config_json(&cur);
    if let Some(c) = &req.config {
        crate::settings::merge_json(&mut cfg, c);
    }
    let _: crate::settings::ProjectConfig = serde_json::from_value(cfg.clone()).map_err(|e| bad(format!("bad config: {e}")))?;
    sqlx::query("UPDATE projects SET name=COALESCE($2,name), config=$3 WHERE id=$1").bind(id).bind(&req.name).bind(&cfg).execute(&mut *tx).await?;
    tx.commit().await?;
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.project.update", Some(id), None, json!({"config": req.config, "name": req.name})).await;
    Ok(Json(project_value(&state, id).await?))
}

async fn project_repos(State(state): State<AppState>, user: User, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    user.require(&state, Some(id), "view").await?;
    Ok(Json(json!({"repos": engine::project_repos(&state, id).await?})))
}

#[derive(Deserialize)]
struct OnboardReq {
    connector_id: Uuid,
    full_name: String,
}

/// Onboard a repository: record its default branch, protected branches and
/// detected stack with no further input.
pub async fn onboard(state: &AppState, project: Uuid, connector: Uuid, full_name: &str) -> anyhow::Result<engine::RepoRow> {
    let host = crate::connectors::repo_host(state, connector).await?;
    let info = host.repo_info(full_name).await?;
    let files = host.list_files(full_name, &info.default_branch).await.unwrap_or_default();
    let (stack, profile) = crate::stack::detect(&files);
    let runner_cfg = match host.read_file(full_name, &info.default_branch, ".factory/runner.yml").await {
        Ok(Some(y)) => serde_norway::from_str::<proto::RunnerConfig>(&y).ok().map(|c| serde_json::to_value(c).unwrap()),
        _ => None,
    };
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO repositories(id, project_id, connector_id, full_name, default_branch, protected_branches, stack, stack_profile, runner_config)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
         ON CONFLICT (project_id, full_name) DO UPDATE SET default_branch=EXCLUDED.default_branch, protected_branches=EXCLUDED.protected_branches,
            stack=EXCLUDED.stack, stack_profile=EXCLUDED.stack_profile, runner_config=EXCLUDED.runner_config",
    )
    .bind(id)
    .bind(project)
    .bind(connector)
    .bind(full_name)
    .bind(&info.default_branch)
    .bind(&info.protected_branches)
    .bind(&stack)
    .bind(&profile)
    .bind(&runner_cfg)
    .execute(&state.pool)
    .await?;
    Ok(engine::repo_by_name(state, project, full_name).await?.unwrap())
}

async fn onboard_repo(State(state): State<AppState>, user: User, Path(id): Path<Uuid>, Json(req): Json<OnboardReq>) -> ApiResult<Json<Value>> {
    user.require(&state, Some(id), "administer").await?;
    let r = onboard(&state, id, req.connector_id, &req.full_name).await?;
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.repo.onboard", Some(id), None, json!({"repo": req.full_name, "stack": r.stack})).await;
    Ok(Json(serde_json::to_value(r).unwrap()))
}

#[derive(Deserialize)]
struct RepoUpd {
    #[serde(default, with = "double_option")]
    image_override: Option<Option<String>>,
    monitor_prs: Option<bool>,
}

mod double_option {
    use serde::{Deserialize, Deserializer};
    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Ok(Some(Option::deserialize(d)?))
    }
}

async fn update_repo(State(state): State<AppState>, user: User, Path(id): Path<Uuid>, Json(req): Json<RepoUpd>) -> ApiResult<Json<Value>> {
    let r = engine::repo(&state, id).await?;
    user.require(&state, Some(r.project_id), "administer").await?;
    if let Some(img) = &req.image_override {
        if let Some(i) = img {
            if !crate::stack::STACKS.contains(&i.as_str()) {
                return Err(bad(format!("unknown image {i}")));
            }
        }
        sqlx::query("UPDATE repositories SET image_override=$2 WHERE id=$1").bind(id).bind(img.clone()).execute(&state.pool).await?;
    }
    if let Some(m) = req.monitor_prs {
        sqlx::query("UPDATE repositories SET monitor_prs=$2 WHERE id=$1").bind(id).bind(m).execute(&state.pool).await?;
    }
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.repo.update", Some(r.project_id), None, json!({"repo": r.full_name, "image_override": req.image_override, "monitor_prs": req.monitor_prs})).await;
    Ok(Json(serde_json::to_value(engine::repo(&state, id).await?).unwrap()))
}

async fn delete_repo(State(state): State<AppState>, user: User, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let r = engine::repo(&state, id).await?;
    user.require(&state, Some(r.project_id), "administer").await?;
    sqlx::query("DELETE FROM repositories WHERE id=$1").bind(id).execute(&state.pool).await.map_err(|_| bad("repository has runs"))?;
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.repo.delete", Some(r.project_id), None, json!({"repo": r.full_name})).await;
    Ok(Json(json!({})))
}

async fn templates(State(_state): State<AppState>, _user: User) -> Json<Value> {
    Json(json!({"templates": proto::WorkflowKind::ALL.iter().map(|w| {
        let b = crate::workflows::builtin(*w);
        json!({"workflow": w.as_str(), "version": crate::workflows::version_of(b), "body": b})
    }).collect::<Vec<_>>()}))
}

#[derive(Deserialize)]
struct TplReq {
    body: String,
}

async fn put_template(State(state): State<AppState>, user: User, Path((id, workflow)): Path<(Uuid, String)>, Json(req): Json<TplReq>) -> ApiResult<Json<Value>> {
    user.require(&state, Some(id), "administer").await?;
    if proto::WorkflowKind::parse(&workflow).is_none() {
        return Err(bad("unknown workflow"));
    }
    if req.body.trim().is_empty() {
        sqlx::query("UPDATE projects SET templates = templates - $2 WHERE id=$1").bind(id).bind(&workflow).execute(&state.pool).await?;
    } else {
        sqlx::query("UPDATE projects SET templates = jsonb_set(templates, ARRAY[$2], to_jsonb($3::text)) WHERE id=$1").bind(id).bind(&workflow).bind(&req.body).execute(&state.pool).await?;
    }
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.template.update", Some(id), None, json!({"workflow": workflow, "version": crate::workflows::version_of(&req.body)})).await;
    Ok(Json(project_value(&state, id).await?))
}

// ---------------------------------------------------------------------------
// Users, settings, audit, metrics
// ---------------------------------------------------------------------------

async fn list_users(State(state): State<AppState>, user: User) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM users ORDER BY email").fetch_all(&state.pool).await?;
    let mut out = vec![];
    for id in ids {
        let mut u = super::auth::user_json(&state, id).await?;
        let ids: Vec<(String, String)> = sqlx::query_as("SELECT provider, external_id FROM identities WHERE user_id=$1 ORDER BY provider").bind(id).fetch_all(&state.pool).await?;
        u["identities"] = json!(ids.into_iter().map(|(p, e)| json!({"provider": p, "external_id": e})).collect::<Vec<_>>());
        out.push(u);
    }
    Ok(Json(json!({"users": out})))
}

#[derive(Deserialize)]
struct UserReq {
    email: String,
    display_name: Option<String>,
    is_global_admin: Option<bool>,
}

async fn create_user(State(state): State<AppState>, user: User, Json(req): Json<UserReq>) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    let email = req.email.trim().to_lowercase();
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO users(id, email, display_name, is_global_admin) VALUES ($1,$2,$3,$4)
         ON CONFLICT (email) DO UPDATE SET display_name=COALESCE(NULLIF(EXCLUDED.display_name,''), users.display_name), is_global_admin=EXCLUDED.is_global_admin RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(&email)
    .bind(req.display_name.unwrap_or_default())
    .bind(req.is_global_admin.unwrap_or(false))
    .fetch_one(&state.pool)
    .await?;
    sqlx::query("INSERT INTO identities(user_id, provider, external_id) VALUES ($1,'email',$2) ON CONFLICT DO NOTHING").bind(id).bind(&email).execute(&state.pool).await?;
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.user.create", None, None, json!({"email": email})).await;
    Ok(Json(super::auth::user_json(&state, id).await?))
}

#[derive(Deserialize)]
struct Ident {
    provider: String,
    external_id: String,
}
#[derive(Deserialize)]
struct IdentReq {
    identities: Vec<Ident>,
}

async fn put_identities(State(state): State<AppState>, user: User, Path(id): Path<Uuid>, Json(req): Json<IdentReq>) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("DELETE FROM identities WHERE user_id=$1 AND provider <> 'email'").bind(id).execute(&mut *tx).await?;
    for i in &req.identities {
        sqlx::query("INSERT INTO identities(user_id, provider, external_id) VALUES ($1,$2,$3) ON CONFLICT (provider, external_id) DO UPDATE SET user_id=EXCLUDED.user_id")
            .bind(id)
            .bind(&i.provider)
            .bind(&i.external_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.user.identities", None, None, json!({"user": id, "count": req.identities.len()})).await;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct Grant {
    project_id: Option<Uuid>,
    role: String,
}
#[derive(Deserialize)]
struct GrantReq {
    grants: Vec<Grant>,
}

async fn put_grants(State(state): State<AppState>, user: User, Path(id): Path<Uuid>, Json(req): Json<GrantReq>) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("DELETE FROM role_grants WHERE user_id=$1 AND source='manual'").bind(id).execute(&mut *tx).await?;
    for g in &req.grants {
        if humans::role_rank(&g.role) < 0 {
            return Err(bad(format!("unknown role {}", g.role)));
        }
        sqlx::query("INSERT INTO role_grants(user_id, project_id, role) VALUES ($1,$2,$3)").bind(id).bind(g.project_id).bind(&g.role).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.user.grants", None, None, json!({"user": id, "grants": req.grants.iter().map(|g| json!({"project_id": g.project_id, "role": g.role})).collect::<Vec<_>>()})).await;
    Ok(Json(json!({})))
}

async fn get_settings(State(state): State<AppState>, user: User) -> ApiResult<Json<Value>> {
    user.require(&state, None, "view").await.or(user.require_admin(&state).await)?;
    Ok(Json(serde_json::to_value(crate::settings::get(&state.pool).await?).unwrap()))
}

async fn put_settings(State(state): State<AppState>, user: User, Json(patch): Json<Value>) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    let before = crate::settings::get(&state.pool).await?;
    let s = crate::settings::merge(&state.pool, &patch).await.map_err(|e| bad(format!("{e:#}")))?;
    crate::audit::record(&state.pool, &user.email, Some(user.id), "config.settings.update", None, None, patch.clone()).await;
    if s.kill_switch && !before.kill_switch {
        engine::apply_kill_switch(&state, &user.email, Some(user.id)).await?;
    }
    Ok(Json(serde_json::to_value(s).unwrap()))
}

#[derive(Deserialize)]
struct AuditQ {
    run_id: Option<Uuid>,
    limit: Option<i64>,
    action: Option<String>,
}

async fn audit(State(state): State<AppState>, user: User, Query(q): Query<AuditQ>) -> ApiResult<Json<Value>> {
    if let Some(r) = q.run_id {
        load_run(&state, &user, r, "view").await?;
    } else {
        user.require_admin(&state).await?;
    }
    let rows: Vec<(i64, chrono::DateTime<chrono::Utc>, String, String, Option<Uuid>, Option<Uuid>, Value)> = sqlx::query_as(
        "SELECT id, ts, actor, action, project_id, run_id, details FROM audit WHERE ($1::uuid IS NULL OR run_id=$1) AND ($3::text IS NULL OR action LIKE $3 || '%') ORDER BY id DESC LIMIT $2",
    )
    .bind(q.run_id)
    .bind(q.limit.unwrap_or(200).clamp(1, 5000))
    .bind(q.action)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({"entries": rows.into_iter().map(|(id, ts, a, ac, p, r, d)| json!({"id": id, "ts": ts, "actor": a, "action": ac, "project_id": p, "run_id": r, "details": d})).collect::<Vec<_>>()})))
}

/// Outcome metrics per project, model and harness.
async fn outcomes(State(state): State<AppState>, _user: User) -> ApiResult<Json<Value>> {
    let rows: Vec<(String, String, String, i64, i64, Option<f64>, Option<f64>, f64)> = sqlx::query_as(
        "SELECT p.key, r.model, r.harness, count(*)::int8,
                count(*) FILTER (WHERE r.outcome IN ('approved','merged'))::int8,
                avg(r.review_rounds) FILTER (WHERE r.pr_number IS NOT NULL)::float8,
                avg(EXTRACT(EPOCH FROM (r.finished_at - r.created_at)) / 3600.0) FILTER (WHERE r.outcome IN ('approved','merged'))::float8,
                COALESCE(sum(r.spent_usd),0)::float8
         FROM runs r JOIN projects p ON p.id = r.project_id
         WHERE r.workflow = 'ticket_build'
         GROUP BY p.key, r.model, r.harness ORDER BY p.key, r.model",
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({"rows": rows.into_iter().map(|(p, m, h, n, ok, rounds, hours, cost)| {
        let with_pr = n.max(1) as f64;
        json!({"project_key": p, "model": m, "harness": h, "runs": n, "approved_prs": ok,
               "approval_rate": ok as f64 / with_pr, "avg_review_rounds": rounds, "avg_hours_to_approval": hours,
               "cost_per_approved_pr": if ok > 0 { Some(cost / ok as f64) } else { None }})
    }).collect::<Vec<_>>()})))
}

/// Run the singleton duties now (admin; used by tests and operators).
async fn force_tick(State(state): State<AppState>, user: User) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    crate::singleton::tick_once(&state).await?;
    Ok(Json(json!({})))
}

async fn force_purge(State(state): State<AppState>, user: User) -> ApiResult<Json<Value>> {
    user.require_admin(&state).await?;
    let n = crate::singleton::purge_checkpoints(&state).await?;
    Ok(Json(json!({"purged": n})))
}
