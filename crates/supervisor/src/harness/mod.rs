//! The harness adapter interface. The plane and portal only ever see the
//! normalised events defined in `proto::HarnessEvent`.

pub mod copilot;
pub mod scripted;

use crate::workspace::Workspace;
use anyhow::Result;
use async_trait::async_trait;
use proto::{HarnessEvent, InputKind, TurnOutcome};
use serde_json::Value;
use std::path::PathBuf;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    pub resume: bool,
    pub usage: bool,
    pub interrupt: bool,
}

pub struct TurnInput {
    pub instruction_id: i64,
    pub input_id: Uuid,
    pub kind: InputKind,
    pub text: String,
    pub data: Value,
    /// Set when the session could not be resumed: a summary to seed a fresh one.
    pub handoff: Option<String>,
}

pub struct TurnCtx<'a> {
    pub ws: &'a Workspace,
    pub events: mpsc::UnboundedSender<HarnessEvent>,
    /// Flips to true when the supervisor wants the turn stopped.
    pub interrupt: watch::Receiver<bool>,
    pub deny: &'a crate::deny::DenyList,
}

#[async_trait]
pub trait Harness: Send + Sync {
    #[allow(dead_code)]
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    /// Configure the harness in the workspace (model endpoint, run model key,
    /// model, and the `factory` MCP server).
    async fn prepare(&mut self, ws: &Workspace) -> Result<()>;
    /// Run one headless turn. Resume the stored session if there is one.
    async fn run_turn(&mut self, ctx: TurnCtx<'_>, input: TurnInput) -> Result<TurnOutcome>;
    /// Paths, relative to the session directory, that hold the session.
    fn session_files(&self) -> Vec<PathBuf>;
    /// Whether a session exists to resume.
    fn has_session(&self, ws: &Workspace) -> bool;
}

pub fn make(name: &str) -> Result<Box<dyn Harness>> {
    Ok(match name {
        "scripted" => Box::new(scripted::Scripted::default()),
        "copilot" => Box::new(copilot::Copilot::default()),
        other => anyhow::bail!("unknown harness {other}"),
    })
}
