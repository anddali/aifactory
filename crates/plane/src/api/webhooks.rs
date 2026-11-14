//! Webhook receivers (Jira, GitHub, Slack). Filled in during phase 2.

use crate::AppState;
use axum::Router;

pub fn routes() -> Router<AppState> {
    Router::new()
}
