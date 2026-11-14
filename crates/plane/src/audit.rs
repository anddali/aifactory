//! Audit trail of human actions and of connector and secret use.

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

pub async fn record(
    pool: &PgPool,
    actor: &str,
    actor_user_id: Option<Uuid>,
    action: &str,
    project_id: Option<Uuid>,
    run_id: Option<Uuid>,
    details: Value,
) {
    let mut details = details;
    crate::redact::redact_json(&mut details);
    if let Err(e) = sqlx::query(
        "INSERT INTO audit(actor, actor_user_id, action, project_id, run_id, details) VALUES ($1,$2,$3,$4,$5,$6)",
    )
    .bind(actor)
    .bind(actor_user_id)
    .bind(action)
    .bind(project_id)
    .bind(run_id)
    .bind(details)
    .execute(pool)
    .await
    {
        tracing::error!("audit write failed: {e}");
    }
}
