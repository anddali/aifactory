//! A fake LiteLLM proxy: virtual keys with budgets, model allowlists and
//! spend, and an OpenAI-compatible chat endpoint that charges a fixed amount
//! per call and refuses calls once a key's budget is spent. State lives in
//! `fake_store` so all replicas agree.

use crate::AppState;
use crate::connectors::fake::{store_get, store_put};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MASTER_KEY: &str = "sk-fake-master";
pub const COST_PER_CALL: f64 = 0.01;
const NS: &str = "fake_litellm";

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct VKey {
    pub key: String,
    pub alias: Option<String>,
    pub max_budget: Option<f64>,
    pub spend: f64,
    pub models: Vec<String>,
    pub metadata: Value,
    pub deleted: bool,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/fake-litellm/health/liveliness", get(|| async { "I'm alive!" }))
        .route("/fake-litellm/key/generate", post(generate))
        .route("/fake-litellm/key/update", post(update))
        .route("/fake-litellm/key/delete", post(delete))
        .route("/fake-litellm/key/info", get(info))
        .route("/fake-litellm/v1/chat/completions", post(chat))
}

fn bearer(h: &HeaderMap) -> String {
    h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("").to_string()
}

fn master(h: &HeaderMap) -> Result<(), Response> {
    if bearer(h) == MASTER_KEY {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, Json(json!({"error": {"message": "invalid master key"}}))).into_response())
    }
}

async fn generate(State(state): State<AppState>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    if let Err(r) = master(&h) {
        return r;
    }
    let key = format!("sk-{}", uuid::Uuid::new_v4().simple());
    let vk = VKey {
        key: key.clone(),
        alias: b["key_alias"].as_str().map(String::from),
        max_budget: b["max_budget"].as_f64(),
        spend: 0.0,
        models: b["models"].as_array().map(|a| a.iter().filter_map(|m| m.as_str().map(String::from)).collect()).unwrap_or_default(),
        metadata: b["metadata"].clone(),
        deleted: false,
    };
    if store_put(&state.pool, NS, &key, &vk).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Json(json!({"key": key, "key_alias": vk.alias, "max_budget": vk.max_budget, "models": vk.models})).into_response()
}

async fn update(State(state): State<AppState>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    if let Err(r) = master(&h) {
        return r;
    }
    let k = b["key"].as_str().unwrap_or("");
    let Ok(Some(mut vk)) = store_get::<VKey>(&state.pool, NS, k).await else { return StatusCode::NOT_FOUND.into_response() };
    if let Some(m) = b["max_budget"].as_f64() {
        vk.max_budget = Some(m);
    }
    let _ = store_put(&state.pool, NS, k, &vk).await;
    Json(json!({"key": k, "max_budget": vk.max_budget})).into_response()
}

async fn delete(State(state): State<AppState>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    if let Err(r) = master(&h) {
        return r;
    }
    let mut deleted = vec![];
    for k in b["keys"].as_array().cloned().unwrap_or_default() {
        let k = k.as_str().unwrap_or("").to_string();
        if let Ok(Some(mut vk)) = store_get::<VKey>(&state.pool, NS, &k).await {
            vk.deleted = true;
            let _ = store_put(&state.pool, NS, &k, &vk).await;
            deleted.push(k);
        }
    }
    Json(json!({"deleted_keys": deleted})).into_response()
}

#[derive(Deserialize)]
struct KeyQ {
    key: String,
}

async fn info(State(state): State<AppState>, h: HeaderMap, Query(q): Query<KeyQ>) -> Response {
    if let Err(r) = master(&h) {
        return r;
    }
    match store_get::<VKey>(&state.pool, NS, &q.key).await {
        Ok(Some(vk)) if !vk.deleted => Json(json!({"key": q.key, "info": {"spend": vk.spend, "max_budget": vk.max_budget, "models": vk.models, "key_alias": vk.alias, "metadata": vk.metadata}})).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn chat(State(state): State<AppState>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    let k = bearer(&h);
    let Ok(Some(mut vk)) = store_get::<VKey>(&state.pool, NS, &k).await else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": {"message": "invalid key"}}))).into_response();
    };
    if vk.deleted {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": {"message": "key deleted"}}))).into_response();
    }
    let model = b["model"].as_str().unwrap_or("").to_string();
    if !vk.models.is_empty() && !vk.models.contains(&model) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": {"message": format!("key not allowed to access model {model}")}}))).into_response();
    }
    if let Some(m) = vk.max_budget {
        if vk.spend >= m {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": {"message": "Budget has been exceeded", "type": "budget_exceeded"}}))).into_response();
        }
    }
    vk.spend += COST_PER_CALL;
    let _ = store_put(&state.pool, NS, &k, &vk).await;
    Json(json!({"id": "chatcmpl-litellm-fake", "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok (fake litellm)"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 50, "completion_tokens": 10, "total_tokens": 60}}))
    .into_response()
}
