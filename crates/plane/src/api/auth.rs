//! Portal sign-in: Entra ID (phase 3) and a development sign-in that exists
//! only when `FACTORY_DEV_LOGIN` is set.

use super::{ApiError, ApiResult, forbidden, unauthorized};
use crate::AppState;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub const COOKIE: &str = "factory_session";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/config", get(config))
        .route("/auth/dev-login", post(dev_login))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/auth/entra/login", get(crate::entra::login))
        .route("/auth/entra/callback", get(crate::entra::callback))
}

#[derive(Clone, Debug)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub is_global_admin: bool,
}

impl User {
    pub async fn require(&self, state: &AppState, project: Option<Uuid>, role: &str) -> ApiResult<()> {
        if crate::humans::has_role(state, self.id, project, role).await? {
            Ok(())
        } else {
            Err(forbidden(format!("you need the {role} role")))
        }
    }
    pub async fn require_admin(&self, state: &AppState) -> ApiResult<()> {
        self.require(state, None, "administer").await
    }
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    if let Some(h) = headers.get("x-factory-session").and_then(|h| h.to_str().ok()) {
        return Some(h.to_string());
    }
    let cookies = headers.get("cookie")?.to_str().ok()?;
    cookies.split(';').find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == COOKIE).then(|| v.to_string())
    })
}

impl FromRequestParts<AppState> for User {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let tok = session_token(&parts.headers).ok_or_else(unauthorized)?;
        let hash = crate::crypto::sha256_hex(tok.as_bytes());
        let row: Option<(Uuid, String, bool)> = sqlx::query_as(
            "SELECT u.id, u.email, u.is_global_admin FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.id_hash=$1 AND s.expires_at > now()",
        )
        .bind(hash)
        .fetch_optional(&state.pool)
        .await?;
        let (id, email, is_global_admin) = row.ok_or_else(unauthorized)?;
        Ok(User { id, email, is_global_admin })
    }
}

pub async fn user_json(state: &AppState, id: Uuid) -> ApiResult<Value> {
    let (email, name, admin): (String, String, bool) = sqlx::query_as("SELECT email, display_name, is_global_admin FROM users WHERE id=$1")
        .bind(id)
        .fetch_one(&state.pool)
        .await?;
    let grants: Vec<(Option<Uuid>, String)> = sqlx::query_as("SELECT project_id, role FROM role_grants WHERE user_id=$1").bind(id).fetch_all(&state.pool).await?;
    Ok(json!({
        "id": id, "email": email, "display_name": name, "is_global_admin": admin,
        "grants": grants.into_iter().map(|(p, r)| json!({"project_id": p, "role": r})).collect::<Vec<_>>(),
    }))
}

/// Create a session for a user and return the cookie response.
pub async fn start_session(state: &AppState, user: Uuid) -> ApiResult<Response> {
    let token = crate::crypto::new_token("fss");
    sqlx::query("INSERT INTO sessions(id_hash, user_id, expires_at) VALUES ($1,$2, now() + interval '12 hours')")
        .bind(crate::crypto::sha256_hex(token.as_bytes()))
        .bind(user)
        .execute(&state.pool)
        .await?;
    let body = user_json(state, user).await?;
    let mut resp = (StatusCode::OK, Json(body)).into_response();
    resp.headers_mut().insert(
        "set-cookie",
        format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=43200").parse().unwrap(),
    );
    resp.headers_mut().insert("x-factory-session", token.parse().unwrap());
    Ok(resp)
}

async fn config(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"dev_login": state.cfg.dev_login, "entra": state.cfg.entra.is_some()}))
}

#[derive(Deserialize)]
struct DevLogin {
    email: String,
    #[serde(default)]
    name: Option<String>,
}

async fn dev_login(State(state): State<AppState>, Json(req): Json<DevLogin>) -> ApiResult<Response> {
    if !state.cfg.dev_login {
        return Err(ApiError(StatusCode::NOT_FOUND, "development sign-in is disabled".into()));
    }
    let email = req.email.trim().to_lowercase();
    if !email.contains('@') {
        return Err(super::bad("enter an email address"));
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO users(id, email, display_name) VALUES ($1,$2,$3) ON CONFLICT (email) DO UPDATE SET email=EXCLUDED.email RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(&email)
    .bind(req.name.unwrap_or_else(|| email.split('@').next().unwrap_or("").to_string()))
    .fetch_one(&state.pool)
    .await?;
    // Link the email as an identity where possible.
    sqlx::query("INSERT INTO identities(user_id, provider, external_id) VALUES ($1,'email',$2) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(&email)
        .execute(&state.pool)
        .await?;
    crate::audit::record(&state.pool, &email, Some(id), "auth.dev_login", None, None, json!({})).await;
    start_session(&state, id).await
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(t) = session_token(&headers) {
        sqlx::query("DELETE FROM sessions WHERE id_hash=$1").bind(crate::crypto::sha256_hex(t.as_bytes())).execute(&state.pool).await?;
    }
    let mut resp = Json(json!({})).into_response();
    resp.headers_mut().insert("set-cookie", format!("{COOKIE}=; Path=/; Max-Age=0").parse().unwrap());
    Ok(resp)
}

async fn me(State(state): State<AppState>, user: User) -> ApiResult<Json<Value>> {
    Ok(Json(user_json(&state, user.id).await?))
}
