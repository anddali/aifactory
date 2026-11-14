//! AWS ECS on Fargate launcher. Filled in during phase 3.

use super::*;

pub struct FargateLauncher;

impl FargateLauncher {
    pub async fn from_env() -> Option<Self> {
        None
    }
}

#[async_trait]
impl Launcher for FargateLauncher {
    fn kind(&self) -> &'static str {
        "fargate"
    }
    async fn launch(&self, _spec: &LaunchSpec) -> Result<String> {
        anyhow::bail!("fargate launcher not configured")
    }
    async fn remove(&self, _handle: &str) -> Result<()> {
        Ok(())
    }
    async fn alive(&self, _handle: &str) -> Result<Option<bool>> {
        Ok(None)
    }
}
