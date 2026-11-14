//! Webhook receivers: the fast path for Jira, GitHub, Azure DevOps and Slack
//! events. Each verifies the request's signature with the connector's
//! webhook secret, parses it into the role's event type and enqueues the same
//! job the polling reconciliation uses; de-duplication happens there.

use super::{ApiError, ApiResult};
use crate::AppState;
use crate::jobs::Job;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/webhooks/jira/{cid}", post(jira))
        .route("/webhooks/github/{cid}", post(github))
        .route("/webhooks/azure-devops/{cid}", post(ado))
        .route("/webhooks/slack/{cid}", post(slack))
}

async fn secret(state: &AppState, cid: Uuid, kind: &str, name: &str) -> ApiResult<String> {
    let l = crate::connectors::load(state, cid).await.map_err(|_| ApiError(StatusCode::NOT_FOUND, "unknown connector".into()))?;
    if l.row.kind != kind {
        return Err(ApiError(StatusCode::NOT_FOUND, "unknown connector".into()));
    }
    l.secrets.get(name).cloned().ok_or(ApiError(StatusCode::UNAUTHORIZED, "webhook secret not configured".into()))
}

fn refused(source: &str) -> ApiError {
    crate::metrics::POLICY_DENIALS.with_label_values(&["webhook_signature"]).inc();
    tracing::warn!(source, "webhook with a bad signature refused");
    ApiError(StatusCode::UNAUTHORIZED, "bad signature".into())
}

async fn jira(State(state): State<AppState>, Path(cid): Path<Uuid>, headers: HeaderMap, body: Bytes) -> ApiResult<Json<serde_json::Value>> {
    let s = secret(&state, cid, "jira", "webhook_secret").await?;
    if !crate::connectors::jira::verify_signature(&s, &headers, &body) {
        return Err(refused("jira"));
    }
    let evs = crate::connectors::jira::parse_webhook(&headers, &body).map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let n = evs.len();
    for ev in evs {
        state.jobs.push(Job::TicketEvent { connector_id: cid, event: ev.clone() }, Some(format!("tev:{cid}:{}", ev.id))).await?;
    }
    Ok(Json(json!({"accepted": n})))
}

async fn github(State(state): State<AppState>, Path(cid): Path<Uuid>, headers: HeaderMap, body: Bytes) -> ApiResult<Json<serde_json::Value>> {
    let s = secret(&state, cid, "github", "webhook_secret").await?;
    if !crate::connectors::github::verify_signature(&s, &headers, &body) {
        return Err(refused("github"));
    }
    let evs = crate::connectors::github::parse_webhook(&headers, &body).map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let n = evs.len();
    for ev in evs {
        state.jobs.push(Job::RepoEvent { connector_id: cid, event: ev.clone() }, Some(format!("rev:{cid}:{}", ev.id))).await?;
    }
    Ok(Json(json!({"accepted": n})))
}

async fn ado(State(state): State<AppState>, Path(cid): Path<Uuid>, headers: HeaderMap, body: Bytes) -> ApiResult<Json<serde_json::Value>> {
    let s = secret(&state, cid, "azure_devops", "webhook_secret").await.or(secret(&state, cid, "azure_devops", "pat").await)?;
    if !crate::connectors::ado::verify_signature(&s, &headers, &body) {
        return Err(refused("azure_devops"));
    }
    let evs = crate::connectors::ado::parse_webhook(&headers, &body).map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let n = evs.len();
    for ev in evs {
        state.jobs.push(Job::RepoEvent { connector_id: cid, event: ev.clone() }, Some(format!("rev:{cid}:{}", ev.id))).await?;
    }
    Ok(Json(json!({"accepted": n})))
}

async fn slack(State(state): State<AppState>, Path(cid): Path<Uuid>, headers: HeaderMap, body: Bytes) -> Response {
    let s = match secret(&state, cid, "slack", "signing_secret").await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !crate::connectors::slack::verify_signature(&s, &headers, &body) {
        return refused("slack").into_response();
    }
    match crate::connectors::slack::parse_webhook(&headers, &body) {
        Ok(crate::connectors::slack::SlackInbound::UrlVerification(c)) => Json(json!({"challenge": c})).into_response(),
        Ok(crate::connectors::slack::SlackInbound::Events(evs)) => {
            for ev in evs {
                if let Err(e) = state.jobs.push(Job::ChatEvent { connector_id: cid, event: ev.clone() }, Some(format!("cev:{cid}:{}", ev.id))).await {
                    return ApiError::from(e).into_response();
                }
            }
            // Slack wants a fast 200.
            StatusCode::OK.into_response()
        }
        Err(e) => ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}
