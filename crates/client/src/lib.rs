//! HTTP client for the plane's runner and primitive APIs.
//!
//! Used by the supervisor and by the `factory` binary. It holds only the run
//! token. Several plane URLs may be given; the client fails over between them
//! so that a plane replica can die without the runner noticing.

use anyhow::{Context, Result, anyhow, bail};
use proto::primitives::PrimitiveResponse;
use proto::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Clone)]
pub struct PlaneClient {
    urls: Arc<Vec<String>>,
    current: Arc<AtomicUsize>,
    token: String,
    http: reqwest::Client,
}

impl PlaneClient {
    pub fn new(urls: Vec<String>, token: String) -> Self {
        let urls = urls
            .into_iter()
            .map(|u| u.trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .collect();
        PlaneClient {
            urls: Arc::new(urls),
            current: Arc::new(AtomicUsize::new(0)),
            token,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(90))
                .build()
                .expect("http client"),
        }
    }

    /// Build from `FACTORY_PLANE_URLS` (comma separated) and `FACTORY_RUN_TOKEN`.
    pub fn from_env() -> Result<Self> {
        let urls = std::env::var("FACTORY_PLANE_URLS")
            .or_else(|_| std::env::var("FACTORY_PLANE_URL"))
            .context("FACTORY_PLANE_URLS not set")?;
        let token = std::env::var("FACTORY_RUN_TOKEN").context("FACTORY_RUN_TOKEN not set")?;
        Ok(Self::new(urls.split(',').map(|s| s.to_string()).collect(), token))
    }

    pub fn base_url(&self) -> String {
        let i = self.current.load(Ordering::Relaxed) % self.urls.len().max(1);
        self.urls.get(i).cloned().unwrap_or_default()
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Send a request, failing over to the next plane URL on connection
    /// errors and 5xx responses. Retries for up to `patience`.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Vec<u8>>,
        content_type: &str,
        headers: &[(&str, String)],
        timeout: Duration,
    ) -> Result<reqwest::Response> {
        if self.urls.is_empty() {
            bail!("no plane URLs configured");
        }
        let patience = Duration::from_secs(
            std::env::var("FACTORY_CLIENT_PATIENCE_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(120),
        );
        let start = std::time::Instant::now();
        let mut attempt = 0u32;
        loop {
            let idx = self.current.load(Ordering::Relaxed) % self.urls.len();
            let url = format!("{}{}", self.urls[idx], path);
            let mut req = self
                .http
                .request(method.clone(), &url)
                .bearer_auth(&self.token)
                .timeout(timeout)
                .header("content-type", content_type);
            for (k, v) in headers {
                req = req.header(*k, v);
            }
            if let Some(b) = &body {
                req = req.body(b.clone());
            }
            let err = match req.send().await {
                Ok(resp) if resp.status().is_server_error() => {
                    anyhow!("{} from {}", resp.status(), url)
                }
                Ok(resp) => return Ok(resp),
                Err(e) => anyhow!("request to {url} failed: {e}"),
            };
            attempt += 1;
            // Move to the next replica.
            self.current.store((idx + 1) % self.urls.len(), Ordering::Relaxed);
            if start.elapsed() > patience {
                return Err(err);
            }
            tracing::debug!("plane request failed (attempt {attempt}): {err:#}");
            tokio::time::sleep(Duration::from_millis((200 * attempt as u64).min(2000))).await;
        }
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &impl Serialize,
        headers: &[(&str, String)],
        timeout: Duration,
    ) -> Result<T> {
        let bytes = serde_json::to_vec(body)?;
        let resp = self
            .send(method, path, Some(bytes), "application/json", headers, timeout)
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("plane returned {status} for {path}: {text}");
        }
        serde_json::from_str(&text).with_context(|| format!("decoding response of {path}: {text}"))
    }

    // ---- runner protocol ----

    pub async fn hello(&self, req: &HelloRequest) -> Result<HelloResponse> {
        self.json(reqwest::Method::POST, "/api/runner/hello", req, &[], Duration::from_secs(60))
            .await
    }

    /// Long-poll for instructions.
    pub async fn instructions(&self, generation: i64, wait_secs: u64) -> Result<Vec<InstructionEnvelope>> {
        self.json(
            reqwest::Method::POST,
            "/api/runner/instructions",
            &serde_json::json!({"generation": generation, "wait_secs": wait_secs}),
            &[],
            Duration::from_secs(wait_secs + 30),
        )
        .await
    }

    pub async fn ack(&self, generation: i64, id: i64) -> Result<()> {
        let _: Value = self
            .json(
                reqwest::Method::POST,
                "/api/runner/ack",
                &serde_json::json!({"generation": generation, "instruction_id": id}),
                &[],
                Duration::from_secs(30),
            )
            .await?;
        Ok(())
    }

    pub async fn events(&self, batch: &EventBatch) -> Result<()> {
        let _: Value = self
            .json(reqwest::Method::POST, "/api/runner/events", batch, &[], Duration::from_secs(30))
            .await?;
        Ok(())
    }

    /// Returns false if the plane says this runner is stale and must exit.
    pub async fn heartbeat(&self, hb: &Heartbeat) -> Result<bool> {
        let v: Value = self
            .json(reqwest::Method::POST, "/api/runner/heartbeat", hb, &[], Duration::from_secs(20))
            .await?;
        Ok(v.get("accepted").and_then(|a| a.as_bool()).unwrap_or(true))
    }

    pub async fn turn_complete(&self, tc: &TurnComplete) -> Result<()> {
        let _: Value = self
            .json(reqwest::Method::POST, "/api/runner/turn-complete", tc, &[], Duration::from_secs(30))
            .await?;
        Ok(())
    }

    pub async fn upload_checkpoint(&self, meta: &CheckpointMeta, session: Vec<u8>) -> Result<()> {
        let meta_json = serde_json::to_string(meta)?;
        let resp = self
            .send(
                reqwest::Method::PUT,
                "/api/runner/checkpoint",
                Some(session),
                "application/octet-stream",
                &[("x-factory-checkpoint", meta_json)],
                Duration::from_secs(120),
            )
            .await?;
        if !resp.status().is_success() {
            bail!("checkpoint upload failed: {} {}", resp.status(), resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Download the stored session archive, if any.
    pub async fn download_session(&self) -> Result<Option<Vec<u8>>> {
        let resp = self
            .send(reqwest::Method::GET, "/api/runner/session", None, "application/json", &[], Duration::from_secs(120))
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            bail!("session download failed: {}", resp.status());
        }
        Ok(Some(resp.bytes().await?.to_vec()))
    }

    pub async fn exit(&self, notice: &ExitNotice) -> Result<()> {
        let _: Value = self
            .json(reqwest::Method::POST, "/api/runner/exit", notice, &[], Duration::from_secs(20))
            .await?;
        Ok(())
    }

    /// Ask the plane for a handoff summary of the run (resume fallback).
    pub async fn handoff_summary(&self) -> Result<String> {
        let v: Value = self
            .json(reqwest::Method::POST, "/api/runner/handoff", &serde_json::json!({}), &[], Duration::from_secs(30))
            .await?;
        Ok(v.get("summary").and_then(|s| s.as_str()).unwrap_or("").to_string())
    }

    // ---- primitives ----

    /// Call a primitive. The idempotency key defaults to a hash of the
    /// primitive name, the arguments and the instruction id, so a retried
    /// call inside the same turn is recognised by the plane.
    pub async fn primitive(
        &self,
        name: &str,
        args: &Value,
        idempotency_key: Option<String>,
        instruction_id: Option<i64>,
    ) -> Result<PrimitiveResponse> {
        let key = idempotency_key.unwrap_or_else(|| default_key(name, args, instruction_id));
        let mut headers = vec![(IDEMPOTENCY_HEADER, key)];
        if let Some(i) = instruction_id {
            headers.push((INSTRUCTION_HEADER, i.to_string()));
        }
        let resp = self
            .send(
                reqwest::Method::POST,
                &format!("/api/primitives/{name}"),
                Some(serde_json::to_vec(args)?),
                "application/json",
                &headers,
                Duration::from_secs(120),
            )
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        match serde_json::from_str::<PrimitiveResponse>(&text) {
            Ok(r) => Ok(r),
            Err(_) => Ok(PrimitiveResponse {
                ok: false,
                result: Value::Null,
                error: Some(format!("{status}: {text}")),
                ends_turn: false,
                replayed: false,
            }),
        }
    }
}

pub fn default_key(name: &str, args: &Value, instruction_id: Option<i64>) -> String {
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    h.update(b"\0");
    h.update(canonical_json(args).as_bytes());
    h.update(b"\0");
    h.update(instruction_id.unwrap_or(0).to_string().as_bytes());
    hex::encode(h.finalize())
}

/// JSON with sorted object keys, so equal values hash equally.
pub fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<_> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", serde_json::to_string(k).unwrap(), canonical_json(&m[k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical_json).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}
