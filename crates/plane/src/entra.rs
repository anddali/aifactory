//! Entra ID sign-in. Implemented in phase 3.

use crate::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

pub async fn login(State(_state): State<AppState>) -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "Entra sign-in is not configured")
}

pub async fn callback(State(_state): State<AppState>) -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "Entra sign-in is not configured")
}
