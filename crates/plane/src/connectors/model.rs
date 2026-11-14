//! Model provider role: test connection for direct providers and LiteLLM.

use super::Loaded;
use crate::AppState;
use anyhow::{Result, bail};

pub async fn test_connection(state: &AppState, l: &Loaded) -> Result<String> {
    let base = match l.row.kind.as_str() {
        "fake_model" => l.cfg("base_url").unwrap_or_else(|| format!("{}/fake-model/v1", state.cfg.public_urls[0])),
        _ => l.cfg("base_url").ok_or_else(|| anyhow::anyhow!("base_url is required"))?,
    };
    let key = match l.row.kind.as_str() {
        "litellm" => l.secrets.get("master_key").cloned(),
        _ => l.secrets.get("api_key").cloned(),
    };
    let url = if l.row.kind == "litellm" { format!("{}/health/liveliness", base.trim_end_matches('/')) } else { format!("{}/models", base.trim_end_matches('/')) };
    let mut rb = state.http.get(&url);
    if let Some(k) = key {
        rb = rb.bearer_auth(k);
    }
    let r = rb.send().await?;
    if !r.status().is_success() {
        bail!("{url} returned {}", r.status());
    }
    Ok(format!("{} reachable at {base}", l.row.kind))
}
