//! Runner launching, behind one interface with process, Docker and Fargate
//! implementations.

pub mod docker;
pub mod fargate;
pub mod process;

use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub run_id: Uuid,
    pub generation: i64,
    /// Deterministic name: a retried launch job finds the same runner.
    pub name: String,
    pub image: String,
    /// Environment for the supervisor. Contains only the run token, plane
    /// URLs and run identifiers — never connector or provider credentials.
    pub env: Vec<(String, String)>,
}

impl LaunchSpec {
    pub fn runner_name(run_id: &Uuid, generation: i64) -> String {
        format!("factory-run-{}-g{generation}", run_id.simple())
    }
}

#[async_trait]
pub trait Launcher: Send + Sync {
    fn kind(&self) -> &'static str;
    /// Launch a runner. Must be idempotent on `spec.name`: if a runner with
    /// that name exists, return its handle instead of starting another.
    async fn launch(&self, spec: &LaunchSpec) -> Result<String>;
    /// Stop and remove the runner and destroy its workspace.
    async fn remove(&self, handle: &str) -> Result<()>;
    /// Best-effort liveness check. `None` means "cannot tell from here".
    async fn alive(&self, handle: &str) -> Result<Option<bool>>;
}

/// Names of keys that must never be passed to a runner.
pub fn assert_env_clean(env: &[(String, String)]) -> Result<()> {
    const ALLOWED: &[&str] = &[
        "FACTORY_PLANE_URLS",
        "FACTORY_RUN_TOKEN",
        "FACTORY_RUN_ID",
        "FACTORY_GENERATION",
        "FACTORY_HARNESS",
        "FACTORY_WORKDIR",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "RUST_LOG",
        "FACTORY_SCRIPTED_FAULT",
    ];
    for (k, _) in env {
        if !ALLOWED.contains(&k.as_str()) {
            anyhow::bail!("refusing to pass environment variable {k} to a runner");
        }
    }
    Ok(())
}
