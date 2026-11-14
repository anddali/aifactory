//! GitHub Copilot CLI adapter. Implemented in phase 2.

use super::*;

#[derive(Default)]
pub struct Copilot;

#[async_trait]
impl Harness for Copilot {
    fn name(&self) -> &'static str {
        "copilot"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { resume: true, usage: true, interrupt: true }
    }
    async fn prepare(&mut self, _ws: &Workspace) -> Result<()> {
        anyhow::bail!("copilot adapter not available yet")
    }
    async fn run_turn(&mut self, _ctx: TurnCtx<'_>, _input: TurnInput) -> Result<TurnOutcome> {
        anyhow::bail!("copilot adapter not available yet")
    }
    fn session_files(&self) -> Vec<PathBuf> {
        vec![]
    }
    fn has_session(&self, _ws: &Workspace) -> bool {
        false
    }
}
