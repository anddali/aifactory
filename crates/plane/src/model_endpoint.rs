//! The plane's OpenAI-compatible model endpoint (direct provider mode).
//!
//! The harness calls `/model/v1/...` with its run-scoped key. The plane
//! checks the key and the run's budget, forwards the request (streaming
//! included) to the configured provider with the real API key, meters token
//! usage from each response, prices it from the portal's price table, and
//! refuses further requests once the budget is spent.
//!
//! A fake OpenAI-compatible provider is served at `/fake-model/v1/...` for
//! development and tests.

use crate::runs::Run;
use crate::{AppState, events};
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/model/v1/models", get(models))
        .route("/model/v1/{*path}", post(proxy))
        .route("/fake-model/v1/models", get(fake_models))
        .route("/fake-model/v1/{*path}", post(fake))
}

fn oai_error(status: StatusCode, kind: &str, msg: &str) -> Response {
    (status, Json(json!({"error": {"message": msg, "type": kind, "code": kind}}))).into_response()
}

async fn run_for_key(state: &AppState, headers: &HeaderMap) -> Option<Run> {
    let h = headers.get("authorization")?.to_str().ok()?;
    let key = h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer "))?.trim();
    let key = headers.get("api-key").and_then(|v| v.to_str().ok()).unwrap_or(key);
    let hash = crate::crypto::sha256_hex(key.as_bytes());
    sqlx::query_as("SELECT * FROM runs WHERE model_key_hash=$1").bind(hash).fetch_optional(&state.pool).await.ok()?
}

pub struct Provider {
    pub base_url: String,
    pub api_key: Option<String>,
    pub kind: String,
}

pub async fn provider(state: &AppState) -> anyhow::Result<Option<Provider>> {
    let s = crate::settings::get(&state.pool).await?;
    let Some(id) = s.model_connector_id else { return Ok(None) };
    let l = crate::connectors::load(state, id).await?;
    let base = match l.row.kind.as_str() {
        "fake_model" => l.cfg("base_url").unwrap_or_else(|| format!("{}/fake-model/v1", state.cfg.public_urls[0])),
        _ => l.cfg("base_url").ok_or_else(|| anyhow::anyhow!("model connector has no base_url"))?,
    };
    Ok(Some(Provider { base_url: base.trim_end_matches('/').to_string(), api_key: l.secrets.get("api_key").cloned(), kind: l.row.kind.clone() }))
}

async fn models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(run) = run_for_key(&state, &headers).await else { return oai_error(StatusCode::UNAUTHORIZED, "invalid_api_key", "invalid run model key") };
    Json(json!({"object": "list", "data": [{"id": run.model, "object": "model", "owned_by": "factory"}]})).into_response()
}

/// Extract (input, output) tokens from a usage object of either API shape.
pub fn usage_of(v: &Value) -> Option<(i64, i64)> {
    let u = v.get("usage").filter(|u| u.is_object()).or_else(|| v.get("response").and_then(|r| r.get("usage")).filter(|u| u.is_object()))?;
    let i = u.get("prompt_tokens").or_else(|| u.get("input_tokens")).and_then(|x| x.as_i64()).unwrap_or(0);
    let o = u.get("completion_tokens").or_else(|| u.get("output_tokens")).and_then(|x| x.as_i64()).unwrap_or(0);
    Some((i, o))
}

pub fn price(s: &crate::settings::Settings, model: &str, input: i64, output: i64) -> f64 {
    let p = s.model_prices.get(model).or_else(|| s.model_prices.iter().find(|(k, _)| model.starts_with(k.as_str())).map(|(_, v)| v));
    match p {
        Some(p) => input as f64 * p.input_per_mtok / 1e6 + output as f64 * p.output_per_mtok / 1e6,
        None => 0.0,
    }
}

/// Record metered usage against the run, its model and its project.
pub async fn record_usage(state: &AppState, run: &Run, model: &str, input: i64, output: i64, source: &str) -> anyhow::Result<f64> {
    let s = crate::settings::get(&state.pool).await?;
    let cost = price(&s, model, input, output);
    sqlx::query("INSERT INTO usage(run_id, project_id, model, input_tokens, output_tokens, cost_usd, source) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(run.id)
        .bind(run.project_id)
        .bind(model)
        .bind(input)
        .bind(output)
        .bind(cost)
        .bind(source)
        .execute(&state.pool)
        .await?;
    sqlx::query("UPDATE runs SET spent_usd = spent_usd + $2, tokens_in = tokens_in + $3, tokens_out = tokens_out + $4 WHERE id=$1")
        .bind(run.id)
        .bind(cost)
        .bind(input)
        .bind(output)
        .execute(&state.pool)
        .await?;
    events::append(&state.pool, &*state.blobs, run.id, "cost", None, json!({"model": model, "input_tokens": input, "output_tokens": output, "cost_usd": cost, "source": source})).await?;
    crate::engine::after_spend(state, run.id).await?;
    Ok(cost)
}

async fn proxy(State(state): State<AppState>, Path(path): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(run) = run_for_key(&state, &headers).await else { return oai_error(StatusCode::UNAUTHORIZED, "invalid_api_key", "invalid run model key") };
    if run.state().is_terminal() {
        return oai_error(StatusCode::UNAUTHORIZED, "invalid_api_key", "the run has ended");
    }
    if run.spent_usd >= run.budget_usd {
        return oai_error(StatusCode::PAYMENT_REQUIRED, "insufficient_quota", "the run's budget is spent");
    }
    let prov = match provider(&state).await {
        Ok(Some(p)) => p,
        Ok(None) => return oai_error(StatusCode::SERVICE_UNAVAILABLE, "no_provider", "no model provider is configured"),
        Err(e) => return oai_error(StatusCode::BAD_GATEWAY, "provider_error", &format!("{e:#}")),
    };
    let mut req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return oai_error(StatusCode::BAD_REQUEST, "invalid_request", "body is not JSON"),
    };
    // The run's model is the only model allowed.
    let model = run.model.clone();
    req["model"] = json!(model);
    let streaming = req.get("stream").and_then(|v| v.as_bool()) == Some(true);
    if streaming && path.starts_with("chat/") {
        req["stream_options"] = json!({"include_usage": true});
    }
    let url = format!("{}/{path}", prov.base_url);
    let mut rb = state.http.post(&url).json(&req);
    if let Some(k) = &prov.api_key {
        rb = rb.bearer_auth(k);
    }
    let resp = match rb.send().await {
        Ok(r) => r,
        Err(e) => return oai_error(StatusCode::BAD_GATEWAY, "provider_unreachable", &format!("{e}")),
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !streaming || !status.is_success() {
        let bytes = resp.bytes().await.unwrap_or_default();
        if status.is_success() {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                if let Some((i, o)) = usage_of(&v) {
                    let _ = record_usage(&state, &run, &model, i, o, "plane_endpoint").await;
                }
            }
        }
        return (status, [("content-type", "application/json")], bytes).into_response();
    }
    // Stream through, watching for the usage chunk.
    let st = state.clone();
    let run2 = run.clone();
    let mut upstream = resp.bytes_stream();
    let s = async_stream::stream! {
        let mut buf = String::new();
        let mut recorded = false;
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(b) => {
                    buf.push_str(&String::from_utf8_lossy(&b));
                    while let Some(pos) = buf.find('\n') {
                        let line = buf[..pos].trim().to_string();
                        buf.drain(..=pos);
                        if let Some(data) = line.strip_prefix("data:") {
                            if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                                if let Some((i, o)) = usage_of(&v) {
                                    if !recorded && (i > 0 || o > 0) {
                                        recorded = true;
                                        let _ = record_usage(&st, &run2, &run2.model, i, o, "plane_endpoint").await;
                                    }
                                }
                            }
                        }
                    }
                    yield Ok::<Bytes, std::io::Error>(b);
                }
                Err(e) => {
                    yield Err(std::io::Error::other(e.to_string()));
                    break;
                }
            }
        }
    };
    Response::builder().status(status).header("content-type", "text/event-stream").header("cache-control", "no-cache").body(Body::from_stream(s)).unwrap()
}

// ---------------------------------------------------------------------------
// Fake OpenAI-compatible provider
// ---------------------------------------------------------------------------

async fn fake_models() -> Json<Value> {
    Json(json!({"object": "list", "data": [{"id": "fake-model", "object": "model", "owned_by": "fake"}]}))
}

/// Deterministic fake model. It requires the provider API key configured on
/// the `fake_model` connector (proving the plane, not the runner, adds it).
/// Behaviour: if the request offers tools and the last message is from the
/// user, it calls the first tool whose name contains `status_report` (if
/// any); otherwise it answers with text. Usage is always reported.
async fn fake(State(_state): State<AppState>, Path(path): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let auth = headers.get("authorization").and_then(|h| h.to_str().ok()).unwrap_or("");
    if !auth.starts_with("Bearer fake-provider-key") {
        return oai_error(StatusCode::UNAUTHORIZED, "invalid_api_key", "fake provider: wrong API key");
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let model = req["model"].as_str().unwrap_or("fake-model").to_string();
    let stream = req["stream"].as_bool() == Some(true);
    let msgs = req["messages"].as_array().cloned().unwrap_or_default();
    let last_role = msgs.last().and_then(|m| m["role"].as_str()).unwrap_or("user").to_string();
    let tool = req["tools"].as_array().and_then(|t| t.iter().find(|x| x["function"]["name"].as_str().unwrap_or("").contains("status_report")).cloned());
    let (pt, ct) = (100 + msgs.len() as i64 * 10, 20);
    if path.starts_with("responses") {
        let v = json!({"id": "resp_fake", "object": "response", "model": model, "status": "completed",
            "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Fake response."}]}],
            "usage": {"input_tokens": pt, "output_tokens": ct, "total_tokens": pt + ct}});
        return Json(v).into_response();
    }
    // Plan phase: after its progress report, submit a plan once per conversation.
    let plan_tool = req["tools"].as_array().and_then(|t| t.iter().find(|x| x["function"]["name"].as_str().unwrap_or("").contains("plan_submit")).cloned());
    let planned = msgs.iter().any(|m| {
        m["tool_calls"].as_array().is_some_and(|c| c.iter().any(|c| c["function"]["name"].as_str().unwrap_or("").contains("plan_submit")))
    });
    let message = if last_role == "user" && tool.is_some() {
        let name = tool.unwrap()["function"]["name"].as_str().unwrap().to_string();
        json!({"role": "assistant", "content": null, "tool_calls": [{"id": "call_fake_1", "type": "function", "function": {"name": name, "arguments": "{\"text\":\"fake model progress\"}"}}]})
    } else if last_role == "tool" && plan_tool.is_some() && !planned {
        let name = plan_tool.unwrap()["function"]["name"].as_str().unwrap().to_string();
        let args = json!({"plan": "## Approach\nFake plan from the fake model.\n", "summary": "Fake plan"}).to_string();
        json!({"role": "assistant", "content": null, "tool_calls": [{"id": "call_fake_plan", "type": "function", "function": {"name": name, "arguments": args}}]})
    } else {
        json!({"role": "assistant", "content": "Done. (fake model)"})
    };
    let finish = if message.get("tool_calls").is_some() { "tool_calls" } else { "stop" };
    if !stream {
        return Json(json!({"id": "chatcmpl-fake", "object": "chat.completion", "created": 0, "model": model,
            "choices": [{"index": 0, "message": message, "finish_reason": finish}],
            "usage": {"prompt_tokens": pt, "completion_tokens": ct, "total_tokens": pt + ct}}))
        .into_response();
    }
    let mut delta = message.clone();
    if let Some(tc) = delta.get_mut("tool_calls").and_then(|t| t.as_array_mut()) {
        for (i, c) in tc.iter_mut().enumerate() {
            c["index"] = json!(i);
        }
    }
    let chunks = [
        json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk", "created": 0, "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": null}]}),
        json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk", "created": 0, "model": model, "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]}),
        json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk", "created": 0, "model": model, "choices": [], "usage": {"prompt_tokens": pt, "completion_tokens": ct, "total_tokens": pt + ct}}),
    ];
    let mut out = String::new();
    for c in chunks {
        out.push_str(&format!("data: {c}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], out).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_shapes_and_pricing() {
        assert_eq!(usage_of(&json!({"usage": {"prompt_tokens": 10, "completion_tokens": 5}})), Some((10, 5)));
        assert_eq!(usage_of(&json!({"response": {"usage": {"input_tokens": 3, "output_tokens": 4}}})), Some((3, 4)));
        assert_eq!(usage_of(&json!({"choices": []})), None);
        let s = crate::settings::Settings::default();
        let c = price(&s, "gpt-4.1", 1_000_000, 1_000_000);
        assert!((c - 10.0).abs() < 1e-9);
        assert_eq!(price(&s, "unknown-model", 1000, 1000), 0.0);
    }
}
