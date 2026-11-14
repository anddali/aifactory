//! The plane's OpenAI-compatible model endpoint. Implemented in phase 2.

use crate::AppState;
use axum::Router;

pub fn routes() -> Router<AppState> {
    Router::new()
}
