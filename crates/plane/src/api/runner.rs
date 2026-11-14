//! The runner protocol, the primitive API and the git endpoint. All are
//! authenticated with the run token.

use super::{ApiError, ApiResult, bad};
use crate::runs::{self, Run};
use crate::{AppState, engine, events};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::Engine;
use proto::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/runner/hello", post(hello))
        .route("/api/runner/instructions", post(instructions))
        .route("/api/runner/ack", post(ack))
        .route("/api/runner/events", post(post_events))
        .route("/api/runner/heartbeat", post(heartbeat))
        .route("/api/runner/turn-complete", post(turn_complete))
        .route("/api/runner/checkpoint", put(checkpoint))
        .route("/api/runner/session", get(session))
        .route("/api/runner/exit", post(exit))
        .route("/api/runner/handoff", post(handoff))
        .route("/api/primitives/{name}", post(primitive))
        .route("/git/{repo_id}/info/refs", get(git_info_refs))
        .route("/git/{repo_id}/{service}", post(git_rpc))
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let h = headers.get("authorization")?.to_str().ok()?;
    if let Some(t) = h.strip_prefix("Bearer ") {
        return Some(t.trim().to_string());
    }
    if let Some(b) = h.strip_prefix("Basic ") {
        let raw = base64::engine::general_purpose::STANDARD.decode(b.trim()).ok()?;
        let s = String::from_utf8(raw).ok()?;
        let (_, pass) = s.split_once(':')?;
        return Some(pass.to_string());
    }
    None
}

/// Authenticate a runner by its run token.
pub async fn run_from_token(state: &AppState, headers: &HeaderMap) -> Result<Run, ApiError> {
    let token = bearer(headers).ok_or(ApiError(StatusCode::UNAUTHORIZED, "run token required".into()))?;
    let hash = crate::crypto::sha256_hex(token.as_bytes());
    let run: Option<Run> = sqlx::query_as("SELECT * FROM runs WHERE run_token_hash=$1").bind(&hash).fetch_optional(&state.pool).await?;
    run.ok_or(ApiError(StatusCode::UNAUTHORIZED, "invalid or expired run token".into()))
}

async fn spec_for(state: &AppState, run: &Run) -> anyhow::Result<RunSpec> {
    let settings = crate::settings::get(&state.pool).await?;
    let pc = engine::project_cfg(state, run.project_id).await?;
    let repo = match run.repo_id {
        Some(id) => Some(engine::repo(state, id).await?),
        None => None,
    };
    let model = crate::model_access::runner_access(state, run).await?;
    let runner_config = repo.as_ref().and_then(|r| r.runner_config.clone()).and_then(|v| serde_json::from_value(v).ok());
    Ok(RunSpec {
        run_id: run.id,
        workflow: run.workflow(),
        harness: run.harness.clone(),
        clone_url: repo.as_ref().map(|r| format!("/git/{}", r.id)),
        branch: run.branch.clone(),
        base_branch: run.base_branch.clone(),
        wip_ref: repo.as_ref().map(|_| run.wip_ref()),
        checkpoint_commit: run.wip_commit.clone(),
        has_session: run.session_blob.is_some(),
        model,
        limits: Limits { max_turns: run.max_turns as u32, turn_timeout_secs: run.turn_timeout_secs as u64, budget_usd: run.budget_usd },
        idle_grace_secs: pc.idle_grace_secs.max(0) as u64,
        heartbeat_secs: settings.heartbeat_secs,
        runner_config,
        deny_patterns: settings.deny_patterns.clone(),
    })
}

async fn hello(State(state): State<AppState>, headers: HeaderMap, Json(req): Json<HelloRequest>) -> ApiResult<Json<HelloResponse>> {
    let run = run_from_token(&state, &headers).await?;
    let spec = spec_for(&state, &run).await?;
    if req.generation != run.runner_generation || run.state().is_terminal() {
        return Ok(Json(HelloResponse { spec, instructions: vec![], accepted: false }));
    }
    sqlx::query("UPDATE runs SET runner_state='alive', last_heartbeat_at=now(), launch_attempts=0 WHERE id=$1 AND runner_generation=$2")
        .bind(run.id)
        .bind(req.generation)
        .execute(&state.pool)
        .await?;
    // Instructions an earlier generation took but never finished are re-delivered.
    sqlx::query("UPDATE instructions SET state='pending', acked_generation=NULL WHERE run_id=$1 AND state='acked' AND acked_generation < $2")
        .bind(run.id)
        .bind(req.generation)
        .execute(&state.pool)
        .await?;
    events::append(&state.pool, &*state.blobs, run.id, "runner_hello", None, json!({"generation": req.generation, "harness": req.harness, "image": req.image, "restoring": run.wip_commit.is_some() || run.session_blob.is_some()})).await?;
    Ok(Json(HelloResponse { spec, instructions: vec![], accepted: true }))
}

#[derive(Deserialize)]
struct InstrReq {
    generation: i64,
    #[serde(default)]
    wait_secs: u64,
}

async fn pending(state: &AppState, run_id: Uuid) -> anyhow::Result<Vec<InstructionEnvelope>> {
    let rows: Vec<(i64, Value)> = sqlx::query_as("SELECT id, body FROM instructions WHERE run_id=$1 AND state='pending' ORDER BY id")
        .bind(run_id)
        .fetch_all(&state.pool)
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, b)| serde_json::from_value(b).ok().map(|instruction| InstructionEnvelope { id, instruction }))
        .collect())
}

async fn instructions(State(state): State<AppState>, headers: HeaderMap, Json(req): Json<InstrReq>) -> Response {
    let run = match run_from_token(&state, &headers).await {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    if req.generation != run.runner_generation {
        return Json(vec![InstructionEnvelope { id: 0, instruction: Instruction::Teardown }]).into_response();
    }
    let mut rx = state.hub.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(req.wait_secs.min(55));
    loop {
        match pending(&state, run.id).await {
            Ok(v) if !v.is_empty() => return Json(v).into_response(),
            Ok(_) => {}
            Err(e) => return ApiError::from(e).into_response(),
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Json(Vec::<InstructionEnvelope>::new()).into_response();
        }
        // Wake on a notification for this run, or re-check every few seconds
        // in case a notification was missed.
        let wait = (deadline - now).min(Duration::from_secs(3));
        let _ = tokio::time::timeout(wait, async {
            loop {
                match rx.recv().await {
                    Ok(crate::notify::Note::Instruction { run_id }) if run_id == run.id => break,
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                    Err(_) => break,
                }
            }
        })
        .await;
    }
}

#[derive(Deserialize)]
struct AckReq {
    generation: i64,
    instruction_id: i64,
}

async fn ack(State(state): State<AppState>, headers: HeaderMap, Json(req): Json<AckReq>) -> ApiResult<Json<Value>> {
    let run = run_from_token(&state, &headers).await?;
    if req.generation != run.runner_generation {
        return Err(ApiError(StatusCode::CONFLICT, "stale runner generation".into()));
    }
    engine::instruction_acked(&state, &run, req.generation, req.instruction_id).await?;
    Ok(Json(json!({})))
}

async fn post_events(State(state): State<AppState>, headers: HeaderMap, Json(batch): Json<EventBatch>) -> ApiResult<Json<Value>> {
    let run = run_from_token(&state, &headers).await?;
    if batch.generation != run.runner_generation {
        return Err(ApiError(StatusCode::CONFLICT, "stale runner generation".into()));
    }
    for ev in batch.events {
        let v = serde_json::to_value(&ev).unwrap_or_default();
        let kind = format!("harness.{}", v.get("type").and_then(|t| t.as_str()).unwrap_or("unknown"));
        let mut payload = v.clone();
        if let Value::Object(m) = &mut payload {
            m.remove("type");
        }
        if let HarnessEvent::Usage { model, input_tokens, output_tokens } = &ev {
            crate::model_access::harness_usage(&state, &run, model.as_deref(), *input_tokens, *output_tokens).await?;
        }
        events::append(&state.pool, &*state.blobs, run.id, &kind, Some("harness"), payload).await?;
    }
    Ok(Json(json!({})))
}

async fn heartbeat(State(state): State<AppState>, headers: HeaderMap, Json(hb): Json<Heartbeat>) -> ApiResult<Json<Value>> {
    let run = run_from_token(&state, &headers).await?;
    let ok = hb.generation == run.runner_generation && !run.state().is_terminal();
    if ok {
        sqlx::query("UPDATE runs SET last_heartbeat_at=now(), runner_state='alive' WHERE id=$1 AND runner_generation=$2 AND runner_state IN ('alive','launching','lost')")
            .bind(run.id)
            .bind(hb.generation)
            .execute(&state.pool)
            .await?;
    }
    Ok(Json(json!({"accepted": ok})))
}

async fn turn_complete(State(state): State<AppState>, headers: HeaderMap, Json(tc): Json<TurnComplete>) -> ApiResult<Json<Value>> {
    let run = run_from_token(&state, &headers).await?;
    if tc.generation != run.runner_generation {
        return Err(ApiError(StatusCode::CONFLICT, "stale runner generation".into()));
    }
    engine::turn_completed(&state, &run, &tc).await?;
    Ok(Json(json!({})))
}

async fn checkpoint(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> ApiResult<Json<Value>> {
    let run = run_from_token(&state, &headers).await?;
    let meta: CheckpointMeta = headers
        .get("x-factory-checkpoint")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| serde_json::from_str(s).ok())
        .ok_or_else(|| bad("missing checkpoint metadata"))?;
    if meta.generation != run.runner_generation {
        return Err(ApiError(StatusCode::CONFLICT, "stale runner generation".into()));
    }
    let hash = if body.is_empty() { None } else { Some(state.blobs.put(&body).await?) };
    sqlx::query("UPDATE runs SET session_blob=COALESCE($2, session_blob), wip_commit=COALESCE($3, wip_commit) WHERE id=$1")
        .bind(run.id)
        .bind(&hash)
        .bind(&meta.commit)
        .execute(&state.pool)
        .await?;
    events::append(&state.pool, &*state.blobs, run.id, "checkpoint_saved", None, json!({"generation": meta.generation, "commit": meta.commit, "session_blob": hash, "size": body.len(), "reason": meta.reason})).await?;
    Ok(Json(json!({"session_blob": hash})))
}

async fn session(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let run = match run_from_token(&state, &headers).await {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    let Some(h) = run.session_blob else { return (StatusCode::NOT_FOUND, "no session").into_response() };
    match state.blobs.get(&h).await {
        Ok(Some(b)) => ([("content-type", "application/octet-stream")], b).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no session").into_response(),
        Err(e) => ApiError::from(e).into_response(),
    }
}

async fn exit(State(state): State<AppState>, headers: HeaderMap, Json(n): Json<ExitNotice>) -> ApiResult<Json<Value>> {
    let run = run_from_token(&state, &headers).await?;
    engine::runner_exited(&state, &run, n.generation, &n.reason).await?;
    Ok(Json(json!({})))
}

async fn handoff(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let run = run_from_token(&state, &headers).await?;
    let s = crate::workflows::handoff_summary(&state, &run).await?;
    Ok(Json(json!({"summary": s})))
}

async fn primitive(State(state): State<AppState>, Path(name): Path<String>, headers: HeaderMap, body: Bytes) -> ApiResult<Json<primitives::PrimitiveResponse>> {
    let run = run_from_token(&state, &headers).await?;
    let args: Value = if body.is_empty() { json!({}) } else { serde_json::from_slice(&body).map_err(|e| bad(format!("bad JSON: {e}")))? };
    let instruction_id = headers.get(INSTRUCTION_HEADER).and_then(|h| h.to_str().ok()).and_then(|s| s.parse().ok());
    let key = headers
        .get(IDEMPOTENCY_HEADER)
        .and_then(|h| h.to_str().ok())
        .map(String::from)
        .unwrap_or_else(|| client::default_key(&name, &args, instruction_id));
    let run = runs::get(&state, run.id).await?;
    Ok(Json(crate::primitives::call(&state, &run, &name, args, &key, instruction_id).await))
}

#[derive(Deserialize)]
struct SvcQ {
    service: String,
}

fn git_resp(r: crate::git::GitResponse) -> Response {
    let mut resp = (StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), r.body).into_response();
    resp.headers_mut().insert("content-type", r.content_type.parse().unwrap());
    resp.headers_mut().insert("cache-control", "no-cache".parse().unwrap());
    resp
}

fn git_unauth(e: ApiError) -> Response {
    let mut r = (e.0, e.1).into_response();
    if e.0 == StatusCode::UNAUTHORIZED {
        r.headers_mut().insert("www-authenticate", "Basic realm=\"factory\"".parse().unwrap());
    }
    r
}

async fn git_info_refs(State(state): State<AppState>, Path(repo_id): Path<Uuid>, Query(q): Query<SvcQ>, headers: HeaderMap) -> Response {
    let run = match run_from_token(&state, &headers).await {
        Ok(r) => r,
        Err(e) => return git_unauth(e),
    };
    git_resp(crate::git::info_refs(&state, &run, repo_id, &q.service).await)
}

async fn git_rpc(State(state): State<AppState>, Path((repo_id, service)): Path<(Uuid, String)>, headers: HeaderMap, body: Bytes) -> Response {
    let run = match run_from_token(&state, &headers).await {
        Ok(r) => r,
        Err(e) => return git_unauth(e),
    };
    let body = if headers.get("content-encoding").and_then(|h| h.to_str().ok()) == Some("gzip") {
        use std::io::Read;
        let mut d = flate2::read::GzDecoder::new(&body[..]);
        let mut out = vec![];
        if d.read_to_end(&mut out).is_err() {
            return (StatusCode::BAD_REQUEST, "bad gzip body").into_response();
        }
        out
    } else {
        body.to_vec()
    };
    git_resp(crate::git::rpc(&state, &run, repo_id, &service, body).await)
}
