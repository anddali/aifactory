//! Run-scoped model access. Filled in during phase 2 (direct provider with
//! the plane's model endpoint, and LiteLLM virtual keys).

use crate::AppState;
use crate::runs::Run;
use anyhow::Result;
use uuid::Uuid;

pub fn model_key(state: &AppState, run: &Run) -> String {
    format!("fmk_{}", state.crypto.derive(&format!("model|{}|{}", run.id, run.token_salt)))
}

pub async fn runner_access(state: &AppState, run: &Run) -> Result<proto::ModelAccess> {
    Ok(proto::ModelAccess {
        base_url: format!("{}/model/v1", state.cfg.public_urls.first().cloned().unwrap_or_default()),
        api_key: model_key(state, run),
        model: run.model.clone(),
    })
}

pub async fn open(_state: &AppState, _run_id: Uuid) -> Result<()> {
    Ok(())
}

pub async fn close(_state: &AppState, _run_id: Uuid) -> Result<()> {
    Ok(())
}

pub async fn update_budget(_state: &AppState, _run_id: Uuid, _budget: f64) -> Result<()> {
    Ok(())
}

pub async fn harness_usage(_state: &AppState, _run: &Run, _model: Option<&str>, _i: u64, _o: u64) -> Result<()> {
    Ok(())
}
