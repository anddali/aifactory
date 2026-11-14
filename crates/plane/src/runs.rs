//! Run rows and the run state machine.

use crate::AppState;
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use proto::RunState;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct Run {
    pub id: Uuid,
    pub project_id: Uuid,
    pub workflow: String,
    pub state: String,
    pub waiting_for: Option<String>,
    pub pause_reason: Option<String>,
    pub title: String,
    pub work_item_key: Option<String>,
    pub ticket_key: Option<String>,
    pub repo_id: Option<Uuid>,
    pub pr_number: Option<i64>,
    pub pr_url: Option<String>,
    pub branch: Option<String>,
    pub base_branch: Option<String>,
    pub assignee_user_id: Option<Uuid>,
    pub harness: String,
    pub model: String,
    pub budget_usd: f64,
    pub max_turns: i32,
    pub turn_timeout_secs: i64,
    pub spent_usd: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub turns_used: i32,
    pub review_rounds: i32,
    pub event_seq: i64,
    pub runner_generation: i64,
    pub runner_state: String,
    pub runner_handle: Option<String>,
    pub runner_image: Option<String>,
    pub runner_launched_at: Option<DateTime<Utc>>,
    pub launch_attempts: i32,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub runner_busy: bool,
    pub turn_started_at: Option<DateTime<Utc>>,
    pub current_instruction: Option<i64>,
    pub token_salt: String,
    pub last_activity_at: DateTime<Utc>,
    pub reminder_sent_at: Option<DateTime<Utc>>,
    pub warning_sent_at: Option<DateTime<Utc>>,
    pub expired_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub chat_thread: Option<Value>,
    pub wip_commit: Option<String>,
    pub session_blob: Option<String>,
    pub cap_notified: bool,
    pub finish_declared: bool,
    pub blocked_reason: Option<String>,
    pub outcome: Option<String>,
    pub template_version: Option<String>,
    pub policy_version: Option<String>,
    pub config_version: Option<String>,
    pub brief: Option<String>,
    pub scope: Option<String>,
    pub data: Value,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Run {
    pub fn state(&self) -> RunState {
        RunState::parse(&self.state).unwrap_or(RunState::Failed)
    }
    pub fn workflow(&self) -> proto::WorkflowKind {
        proto::WorkflowKind::parse(&self.workflow).unwrap_or(proto::WorkflowKind::AdHoc)
    }
    pub fn short(&self) -> String {
        proto::short_id(&self.id)
    }
    /// The run's own branch namespace, e.g. `factory/ab12cd34`.
    pub fn namespace(&self) -> String {
        format!("factory/{}", self.short())
    }
    /// The run-owned work-in-progress branch that checkpoints are pushed to.
    pub fn wip_branch(&self) -> String {
        format!("factory-wip/{}", self.short())
    }
    pub fn wip_ref(&self) -> String {
        format!("refs/heads/{}", self.wip_branch())
    }
}

pub async fn get(state: &AppState, id: Uuid) -> Result<Run> {
    sqlx::query_as("SELECT * FROM runs WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| anyhow!("run {id} not found"))
}

pub async fn find(state: &AppState, id: Uuid) -> Result<Option<Run>> {
    Ok(sqlx::query_as("SELECT * FROM runs WHERE id=$1").bind(id).fetch_optional(&state.pool).await?)
}

/// States a run may move to from each state.
pub fn allowed(from: RunState, to: RunState) -> bool {
    use RunState::*;
    match from {
        Queued => matches!(to, Starting | Paused | Cancelled | Failed),
        Starting => matches!(to, Running | Waiting | Paused | Finished | Failed | Cancelled | Expired),
        Running => matches!(to, Running | Waiting | Paused | Finished | Failed | Cancelled),
        Waiting => matches!(to, Running | Waiting | Paused | Finished | Failed | Cancelled | Expired),
        Paused => matches!(to, Waiting | Running | Paused | Finished | Failed | Cancelled | Expired),
        Expired => matches!(to, Waiting),
        Finished | Failed | Cancelled => false,
    }
}

/// Move a run to a new state if the transition is allowed. Returns the
/// previous state if a change happened, `None` if refused or a no-op.
pub async fn transition(
    state: &AppState,
    run_id: Uuid,
    to: RunState,
    waiting_for: Option<&str>,
    reason: &str,
    actor: Option<&str>,
) -> Result<Option<RunState>> {
    let froms: Vec<&str> = [
        RunState::Queued,
        RunState::Starting,
        RunState::Running,
        RunState::Waiting,
        RunState::Paused,
        RunState::Finished,
        RunState::Failed,
        RunState::Cancelled,
        RunState::Expired,
    ]
    .into_iter()
    .filter(|f| allowed(*f, to))
    .map(|f| f.as_str())
    .collect();
    let mut tx = state.pool.begin().await?;
    let cur: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT state, waiting_for FROM runs WHERE id=$1 FOR UPDATE")
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((cur_state, cur_wait)) = cur else { return Ok(None) };
    if !froms.contains(&cur_state.as_str()) {
        return Ok(None);
    }
    let wait = if to == RunState::Waiting { waiting_for } else { None };
    if cur_state == to.as_str() && cur_wait.as_deref() == wait {
        return Ok(None);
    }
    let terminal = to.is_terminal();
    sqlx::query(
        "UPDATE runs SET state=$2, waiting_for=$3,
           pause_reason = CASE WHEN $2 = 'paused' THEN $4 ELSE NULL END,
           finished_at = CASE WHEN $5 THEN now() ELSE finished_at END,
           expired_at = CASE WHEN $2 = 'expired' THEN now() WHEN $2 = 'waiting' THEN NULL ELSE expired_at END,
           updated_at = now()
         WHERE id=$1",
    )
    .bind(run_id)
    .bind(to.as_str())
    .bind(wait)
    .bind(reason)
    .bind(terminal)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    crate::events::append(
        &state.pool,
        &*state.blobs,
        run_id,
        "state_changed",
        actor,
        json!({"from": cur_state, "to": to.as_str(), "waiting_for": wait, "reason": reason}),
    )
    .await?;
    crate::notify::notify(&state.pool, crate::notify::CH_RUN, &run_id.to_string()).await;
    Ok(RunState::parse(&cur_state))
}

/// Rebuild a run's state from its event log alone.
pub fn rebuild_state(events: &[crate::events::Event]) -> (Option<String>, Option<String>) {
    let mut st = None;
    let mut wait = None;
    for e in events {
        if e.kind == "run_created" {
            st = Some("queued".to_string());
        }
        if e.kind == "state_changed" {
            st = e.payload.get("to").and_then(|v| v.as_str()).map(String::from);
            wait = e.payload.get("waiting_for").and_then(|v| v.as_str()).map(String::from);
        }
    }
    (st, wait)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_states_are_final() {
        for to in [RunState::Waiting, RunState::Running, RunState::Queued] {
            assert!(!allowed(RunState::Finished, to));
            assert!(!allowed(RunState::Cancelled, to));
        }
        assert!(allowed(RunState::Expired, RunState::Waiting));
        assert!(!allowed(RunState::Expired, RunState::Running));
        assert!(allowed(RunState::Waiting, RunState::Expired));
        assert!(!allowed(RunState::Queued, RunState::Running));
    }
}
