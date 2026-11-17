//! Run-scoped model access, for both provider modes.
//!
//! - Direct provider: the runner gets the plane's model endpoint and the
//!   run's derived model key; the plane meters and enforces the budget
//!   (`model_endpoint`).
//! - LiteLLM: at run start the plane creates a LiteLLM virtual key with the
//!   run's budget, a one-model allowlist and an expiry; the runner calls
//!   LiteLLM directly with it. At run end the plane reads the key's spend,
//!   reconciles it with what it tracked, records it, and deletes the key.
//!
//! Switching mode is the `model_connector_id` setting; nothing changes in the
//! runner or the harness, which always see an OpenAI-compatible base URL and
//! a run-scoped key.

use crate::runs::{self, Run};
use crate::{AppState, events};
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};
use uuid::Uuid;

pub fn model_key(state: &AppState, run: &Run) -> String {
    format!("fmk_{}", state.crypto.derive(&format!("model|{}|{}", run.id, run.token_salt)))
}

pub struct LiteLlm {
    pub base_url: String,
    pub runner_base_url: String,
    pub master_key: String,
}

pub async fn litellm(state: &AppState) -> Result<Option<LiteLlm>> {
    let s = crate::settings::get(&state.pool).await?;
    let Some(id) = s.model_connector_id else { return Ok(None) };
    let l = crate::connectors::load(state, id).await?;
    if l.row.kind != "litellm" {
        return Ok(None);
    }
    let base = l.cfg("base_url").ok_or_else(|| anyhow!("litellm connector needs base_url"))?;
    Ok(Some(LiteLlm {
        runner_base_url: l.cfg("runner_base_url").unwrap_or_else(|| base.clone()),
        base_url: base,
        master_key: l.secret("master_key")?,
    }))
}

impl LiteLlm {
    fn url(&self, p: &str) -> String {
        format!("{}/{p}", self.base_url.trim_end_matches('/'))
    }
    async fn post(&self, state: &AppState, p: &str, body: Value) -> Result<Value> {
        let r = state.http.post(self.url(p)).bearer_auth(&self.master_key).json(&body).send().await?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(json!({}));
        if !status.is_success() {
            bail!("litellm {p} returned {status}: {v}");
        }
        Ok(v)
    }
}

fn seal(state: &AppState, secret: &str) -> Result<String> {
    let (kid, n, c) = state.crypto.encrypt(secret.as_bytes())?;
    let b = base64::engine::general_purpose::STANDARD;
    Ok(format!("{kid}:{}:{}", b.encode(n), b.encode(c)))
}

fn unseal(state: &AppState, sealed: &str) -> Result<String> {
    let mut p = sealed.splitn(3, ':');
    let (Some(kid), Some(n), Some(c)) = (p.next(), p.next(), p.next()) else { bail!("bad sealed value") };
    let b = base64::engine::general_purpose::STANDARD;
    Ok(String::from_utf8(state.crypto.decrypt(kid, &b.decode(n)?, &b.decode(c)?)?)?)
}

/// What the runner uses to reach a model.
pub async fn runner_access(state: &AppState, run: &Run) -> Result<proto::ModelAccess> {
    let wire_api = wire_api(state).await?;
    if let (Some(ll), Some(sealed)) = (litellm(state).await?, run.model_key_ref.as_deref()) {
        return Ok(proto::ModelAccess {
            base_url: format!("{}/v1", ll.runner_base_url.trim_end_matches('/')),
            api_key: unseal(state, sealed)?,
            model: run.model.clone(),
            wire_api,
        });
    }
    Ok(proto::ModelAccess {
        base_url: format!("{}/model/v1", state.cfg.public_urls.first().cloned().unwrap_or_default()),
        api_key: model_key(state, run),
        model: run.model.clone(),
        wire_api,
    })
}

/// The model connector's `wire_api` setting (`completions` | `responses`), if set.
async fn wire_api(state: &AppState) -> Result<Option<String>> {
    let s = crate::settings::get(&state.pool).await?;
    let Some(id) = s.model_connector_id else { return Ok(None) };
    let l = crate::connectors::load(state, id).await?;
    Ok(l.cfg("wire_api").filter(|w| w == "completions" || w == "responses"))
}

/// At run start (and on revival): create the LiteLLM virtual key.
pub async fn open(state: &AppState, run_id: Uuid) -> Result<()> {
    let Some(ll) = litellm(state).await? else { return Ok(()) };
    let run = runs::get(state, run_id).await?;
    if run.model_key_ref.is_some() {
        return Ok(());
    }
    // Keys created so far for this run: stable across retries of this step.
    let issued: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE run_id=$1 AND kind='model_key_created'").bind(run.id).fetch_one(&state.pool).await?;
    let pc = crate::engine::project_cfg(state, run.project_id).await?;
    // Expire with the run's lifetime plus slack; the plane deletes it earlier at run end.
    let hours = (pc.idle_expiry_secs / 3600).max(1) * 2 + 24;
    let remaining = (run.budget_usd - run.spent_usd).max(0.0);
    let alias = format!("factory-{}-{issued}", run.id);
    let key_val = crate::humans::once(state, &format!("litellm-key:{alias}"), || async {
        let v = ll
            .post(state, "key/generate", json!({
                "key_alias": alias, "max_budget": remaining, "models": [run.model], "duration": format!("{hours}h"),
                "metadata": {"factory_run_id": run.id, "project_id": run.project_id},
            }))
            .await?;
        let key = v["key"].as_str().ok_or_else(|| anyhow!("litellm key/generate returned no key"))?.to_string();
        Ok(json!({"sealed": seal(state, &key)?}))
    })
    .await?;
    let sealed = key_val["sealed"].as_str().ok_or_else(|| anyhow!("no sealed key"))?;
    sqlx::query("UPDATE runs SET model_key_ref=$2 WHERE id=$1").bind(run.id).bind(sealed).execute(&state.pool).await?;
    events::append(&state.pool, &*state.blobs, run.id, "model_key_created", None, json!({"provider": "litellm", "max_budget": remaining, "models": [run.model], "expires_in_hours": hours})).await?;
    crate::audit::record(&state.pool, &format!("run:{}", run.id), None, "connector.litellm.key_created", Some(run.project_id), Some(run.id), json!({"max_budget": remaining})).await;
    Ok(())
}

pub async fn update_budget(state: &AppState, run_id: Uuid, budget: f64) -> Result<()> {
    let Some(ll) = litellm(state).await? else { return Ok(()) };
    let run = runs::get(state, run_id).await?;
    let Some(sealed) = &run.model_key_ref else { return Ok(()) };
    let key = unseal(state, sealed)?;
    // LiteLLM's max_budget is absolute for the key: allow the new total.
    ll.post(state, "key/update", json!({"key": key, "max_budget": budget})).await?;
    Ok(())
}

/// At run end: read LiteLLM's spend, reconcile, record, delete the key.
pub async fn close(state: &AppState, run_id: Uuid) -> Result<()> {
    let Some(ll) = litellm(state).await? else { return Ok(()) };
    let run = runs::get(state, run_id).await?;
    let Some(sealed) = run.model_key_ref.clone() else { return Ok(()) };
    let key = unseal(state, &sealed)?;
    let info = state.http.get(ll.url("key/info")).query(&[("key", key.as_str())]).bearer_auth(&ll.master_key).send().await?;
    let gone = info.status() == reqwest::StatusCode::NOT_FOUND;
    if !gone {
        let v: Value = info.error_for_status()?.json().await?;
        let spend = v["info"]["spend"].as_f64().or_else(|| v["spend"].as_f64()).unwrap_or(0.0);
        let tracked: f64 = sqlx::query_scalar("SELECT COALESCE(sum(cost_usd),0)::float8 FROM usage WHERE run_id=$1").bind(run.id).fetch_one(&state.pool).await?;
        let delta = spend - tracked;
        if delta.abs() > 1e-9 {
            record_cost(state, &run, &run.model, 0, 0, delta, "litellm_reconciliation").await?;
        }
        events::append(&state.pool, &*state.blobs, run.id, "litellm_reconciled", None, json!({"litellm_spend": spend, "tracked": tracked, "adjustment": delta})).await?;
        ll.post(state, "key/delete", json!({"keys": [key]})).await?;
    }
    sqlx::query("UPDATE runs SET model_key_ref=NULL WHERE id=$1").bind(run.id).execute(&state.pool).await?;
    crate::audit::record(&state.pool, &format!("run:{}", run.id), None, "connector.litellm.key_deleted", Some(run.project_id), Some(run.id), json!({})).await;
    Ok(())
}

pub async fn record_cost(state: &AppState, run: &Run, model: &str, input: i64, output: i64, cost: f64, source: &str) -> Result<()> {
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
    if source != "litellm_reconciliation" {
        crate::engine::after_spend(state, run.id).await?;
    }
    Ok(())
}

/// Usage reported by the harness. In direct mode the plane already meters at
/// its endpoint, so this is informational. With LiteLLM it is the running
/// estimate between reconciliations.
pub async fn harness_usage(state: &AppState, run: &Run, model: Option<&str>, input: u64, output: u64) -> Result<()> {
    if run.model_key_ref.is_none() || litellm(state).await?.is_none() {
        return Ok(());
    }
    let s = crate::settings::get(&state.pool).await?;
    let m = model.unwrap_or(&run.model);
    let cost = crate::model_endpoint::price(&s, m, input as i64, output as i64);
    record_cost(state, run, m, input as i64, output as i64, cost, "harness_estimate").await
}
