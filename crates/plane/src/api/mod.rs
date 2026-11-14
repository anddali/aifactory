//! HTTP API: runner protocol, primitives, git endpoint, portal API, auth,
//! webhooks, fake-system controls, health and metrics.

pub mod auth;
pub mod fake;
pub mod portal;
pub mod runner;
pub mod webhooks;

use crate::AppState;
use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;

/// API error: a status and a message, rendered as `{"error": ...}`.
#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, axum::Json(json!({"error": self.1}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        if let Some(d) = e.downcast_ref::<crate::primitives::Denied>() {
            return ApiError(StatusCode::FORBIDDEN, d.to_string());
        }
        let msg = format!("{e:#}");
        let status = if msg.contains("not found") { StatusCode::NOT_FOUND } else { StatusCode::BAD_REQUEST };
        ApiError(status, msg)
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!("database error: {e}");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, "database error".into())
    }
}

pub fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}
pub fn forbidden(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::FORBIDDEN, msg.into())
}
pub fn unauthorized() -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, "sign in required".into())
}

pub type ApiResult<T> = Result<T, ApiError>;

pub fn router(state: AppState) -> Router {
    let mut r = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(|| async { ([("content-type", "text/plain; version=0.0.4")], crate::metrics::render()) }))
        .merge(runner::routes())
        .merge(auth::routes())
        .merge(portal::routes())
        .merge(webhooks::routes())
        .merge(crate::model_endpoint::routes());
    if state.cfg.dev_login {
        r = r.merge(fake::routes());
    }
    let r = r.with_state(state.clone());
    let r = match &state.cfg.portal_dir {
        Some(dir) if dir.exists() => {
            let index = dir.join("index.html");
            r.fallback_service(tower_http::services::ServeDir::new(dir).fallback(tower_http::services::ServeFile::new(index)))
        }
        _ => r,
    };
    r.layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024))
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

async fn readyz(axum::extract::State(state): axum::extract::State<AppState>) -> impl IntoResponse {
    if crate::db::ready(&state.pool).await {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}
