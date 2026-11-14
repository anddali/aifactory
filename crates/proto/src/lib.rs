//! Types shared by the plane, the runner supervisor and the `factory` binary.
//!
//! Everything that crosses a process boundary is defined here so that the
//! three sides cannot drift apart.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub mod primitives;

/// Lifecycle state of a run as stored in the plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Starting,
    Running,
    Waiting,
    Paused,
    Finished,
    Failed,
    Cancelled,
    Expired,
}

impl RunState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunState::Queued => "queued",
            RunState::Starting => "starting",
            RunState::Running => "running",
            RunState::Waiting => "waiting",
            RunState::Paused => "paused",
            RunState::Finished => "finished",
            RunState::Failed => "failed",
            RunState::Cancelled => "cancelled",
            RunState::Expired => "expired",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => RunState::Queued,
            "starting" => RunState::Starting,
            "running" => RunState::Running,
            "waiting" => RunState::Waiting,
            "paused" => RunState::Paused,
            "finished" => RunState::Finished,
            "failed" => RunState::Failed,
            "cancelled" => RunState::Cancelled,
            "expired" => RunState::Expired,
            _ => return None,
        })
    }
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RunState::Finished | RunState::Failed | RunState::Cancelled | RunState::Expired
        )
    }
    pub const TERMINAL: [&'static str; 4] = ["finished", "failed", "cancelled", "expired"];
}

/// Workflow types. Defined in code; instructions come from templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowKind {
    TicketBuild,
    PrReview,
    AdHoc,
    SecurityAudit,
}

impl WorkflowKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkflowKind::TicketBuild => "ticket_build",
            WorkflowKind::PrReview => "pr_review",
            WorkflowKind::AdHoc => "ad_hoc",
            WorkflowKind::SecurityAudit => "security_audit",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ticket_build" => WorkflowKind::TicketBuild,
            "pr_review" => WorkflowKind::PrReview,
            "ad_hoc" => WorkflowKind::AdHoc,
            "security_audit" => WorkflowKind::SecurityAudit,
            _ => return None,
        })
    }
    pub const ALL: [WorkflowKind; 4] = [
        WorkflowKind::TicketBuild,
        WorkflowKind::PrReview,
        WorkflowKind::AdHoc,
        WorkflowKind::SecurityAudit,
    ];
}

/// Normalised harness event. Every adapter maps its native output to these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    TurnStarted { input_id: Uuid },
    ModelTurn { model: Option<String>, text: Option<String> },
    ToolCall { call_id: String, name: String, args: Value },
    ToolResult { call_id: String, ok: bool, output: Value },
    AgentMessage { text: String },
    Usage {
        model: Option<String>,
        input_tokens: u64,
        output_tokens: u64,
    },
    Error { message: String },
    TurnEnded { input_id: Uuid, outcome: TurnOutcome },
    Log { level: String, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    /// The harness stopped normally.
    Completed,
    /// Interrupted by the supervisor on request.
    Interrupted,
    /// The per-turn wall-clock limit was hit.
    TimedOut,
    /// The harness crashed or exited non-zero.
    Failed,
    /// A shell command matched the deny-list.
    Denied,
}

/// What the plane asks a supervisor to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Instruction {
    /// Run one turn with this input.
    Turn {
        input_id: Uuid,
        kind: InputKind,
        text: String,
        #[serde(default)]
        data: Value,
    },
    /// Stop the current turn now (the next Turn carries the steering message).
    Interrupt,
    /// Checkpoint now and keep going.
    Checkpoint,
    /// Checkpoint (if `checkpoint`) and exit; the run continues elsewhere later.
    Stop { checkpoint: bool },
    /// The run is over. Exit without checkpoint.
    Teardown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    /// The first turn of a run.
    Start,
    /// Answer to a question the agent asked.
    Answer,
    /// Decision on an approval the agent requested.
    Approval,
    /// A steering message from a human.
    Steer,
    /// A PR comment or review.
    Review,
    /// A CI result.
    Ci,
    /// A new push to the PR under review (PR review workflow).
    Push,
    /// A human decision after a cap was hit.
    CapDecision,
    /// Anything else.
    Other,
}

impl InputKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            InputKind::Start => "start",
            InputKind::Answer => "answer",
            InputKind::Approval => "approval",
            InputKind::Steer => "steer",
            InputKind::Review => "review",
            InputKind::Ci => "ci",
            InputKind::Push => "push",
            InputKind::CapDecision => "cap_decision",
            InputKind::Other => "other",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "start" => InputKind::Start,
            "answer" => InputKind::Answer,
            "approval" => InputKind::Approval,
            "steer" => InputKind::Steer,
            "review" => InputKind::Review,
            "ci" => InputKind::Ci,
            "push" => InputKind::Push,
            "cap_decision" => InputKind::CapDecision,
            _ => InputKind::Other,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionEnvelope {
    pub id: i64,
    pub instruction: Instruction,
}

/// Sent by the supervisor when it starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloRequest {
    pub generation: i64,
    pub harness: String,
    pub image: Option<String>,
}

/// Everything a supervisor needs to set up its workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSpec {
    pub run_id: Uuid,
    pub workflow: WorkflowKind,
    pub harness: String,
    /// Git URL on the plane's git endpoint (no credential in it).
    pub clone_url: Option<String>,
    /// The run's work branch, e.g. `factory/ab12cd34`.
    pub branch: Option<String>,
    /// Branch to start from when no checkpoint exists.
    pub base_branch: Option<String>,
    /// Ref holding the work-in-progress checkpoint.
    pub wip_ref: Option<String>,
    /// Commit of the latest checkpoint, if any.
    pub checkpoint_commit: Option<String>,
    /// Whether harness session files are stored for this run.
    pub has_session: bool,
    pub model: ModelAccess,
    pub limits: Limits,
    /// Grace period in seconds before an idle runner checkpoints and exits.
    pub idle_grace_secs: u64,
    pub heartbeat_secs: u64,
    #[serde(default)]
    pub runner_config: Option<RunnerConfig>,
    #[serde(default)]
    pub deny_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelAccess {
    /// OpenAI-compatible base URL (ends in `/v1`).
    pub base_url: String,
    /// Run-scoped key. Never a provider key.
    pub api_key: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    pub max_turns: u32,
    pub turn_timeout_secs: u64,
    pub budget_usd: f64,
}

/// Optional `.factory/runner.yml` contents.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct RunnerConfig {
    #[serde(default)]
    pub setup: Vec<String>,
    #[serde(default)]
    pub test: Vec<String>,
    #[serde(default)]
    pub lint: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloResponse {
    pub spec: RunSpec,
    /// Pending instructions at the time of hello.
    pub instructions: Vec<InstructionEnvelope>,
    /// If false the supervisor must exit at once (stale generation).
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventBatch {
    pub generation: i64,
    pub events: Vec<HarnessEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub generation: i64,
    pub busy: bool,
    pub current_input: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnComplete {
    pub generation: i64,
    pub instruction_id: i64,
    pub input_id: Uuid,
    pub outcome: TurnOutcome,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointMeta {
    pub generation: i64,
    pub commit: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitNotice {
    pub generation: i64,
    pub reason: String,
}

/// Header that carries an idempotency key on primitive calls.
pub const IDEMPOTENCY_HEADER: &str = "idempotency-key";
/// Header that tells the plane which instruction a primitive call belongs to.
pub const INSTRUCTION_HEADER: &str = "x-factory-instruction";

pub fn short_id(id: &Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}
