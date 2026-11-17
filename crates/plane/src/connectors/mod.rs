//! Connector roles and the registry that builds a connector from its stored
//! configuration and decrypted secrets.

pub mod ado;
pub mod fake;
pub mod github;
pub mod jira;
pub mod model;
pub mod slack;

use crate::AppState;
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

// ---------- shared types ----------

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub body: String,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TicketRef {
    pub key: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Ticket {
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub assignee_email: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub children: Vec<TicketRef>,
    #[serde(default)]
    pub comments: Vec<Comment>,
    #[serde(default)]
    pub url: Option<String>,
    /// Repository the ticket's work belongs to, when the source says so.
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketEvent {
    /// Unique id for de-duplication across webhook and polling.
    pub id: String,
    pub key: String,
    /// created | updated | labeled | transitioned | commented | cancelled
    pub kind: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoInfo {
    pub full_name: String,
    pub default_branch: String,
    #[serde(default)]
    pub protected_branches: Vec<String>,
}

/// Where the plane reaches a repository's git data, with the credential the
/// plane adds. Never sent to a runner.
#[derive(Clone)]
pub struct GitRemote {
    pub url: String,
    /// Full `Authorization: ...` header value for HTTP remotes.
    pub auth_header: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Pr {
    pub number: i64,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// open | closed | merged
    pub state: String,
    pub head: String,
    pub base: String,
    pub author: String,
    #[serde(default)]
    pub head_sha: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PrThread {
    pub id: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<i64>,
    /// None (open) | resolved | wont_fix
    #[serde(default)]
    pub status: Option<String>,
    pub comments: Vec<Comment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PrReview {
    pub id: String,
    pub author: String,
    /// approved | changes_requested | commented
    pub state: String,
    #[serde(default)]
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PrDetail {
    pub pr: Pr,
    #[serde(default)]
    pub threads: Vec<PrThread>,
    #[serde(default)]
    pub reviews: Vec<PrReview>,
    /// General (non-thread) comments.
    #[serde(default)]
    pub comments: Vec<Comment>,
}

impl PrDetail {
    /// Approved when the latest review of every reviewer that reviewed is
    /// approved and at least one approval exists.
    pub fn review_decision(&self) -> &'static str {
        let mut latest: HashMap<&str, &str> = HashMap::new();
        for r in &self.reviews {
            if r.state != "commented" {
                latest.insert(&r.author, &r.state);
            }
        }
        if latest.values().any(|s| *s == "changes_requested") {
            "changes_requested"
        } else if latest.values().any(|s| *s == "approved") {
            "approved"
        } else {
            "review_required"
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CheckRun {
    pub id: String,
    pub name: String,
    /// queued | in_progress | completed
    pub status: String,
    /// success | failure | cancelled | null
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEvent {
    pub id: String,
    pub repo: String,
    #[serde(default)]
    pub pr_number: Option<i64>,
    /// pr_opened | pushed | comment | thread_reply | review | closed | merged | ci_completed
    pub kind: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    /// For reviews: approved | changes_requested | commented. For CI: conclusion.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatRef {
    pub channel: String,
    pub ts: String,
    #[serde(default)]
    pub thread_ts: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatEvent {
    pub id: String,
    pub channel: String,
    #[serde(default)]
    pub thread_ts: Option<String>,
    /// External chat user id.
    pub user: String,
    #[serde(default)]
    pub text: String,
    /// Button press: (question id, approved|rejected)
    #[serde(default)]
    pub action: Option<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatMessage {
    pub text: String,
    /// Question id for approval buttons.
    #[serde(default)]
    pub approval_for: Option<String>,
    /// Hidden marker used to find a message again after a crash.
    #[serde(default)]
    pub marker: Option<String>,
    /// External ids to mention.
    #[serde(default)]
    pub mention: Option<String>,
    /// Labels for the approve and reject buttons (default: Approve, Reject).
    #[serde(default)]
    pub button_labels: Option<(String, String)>,
}

// ---------- roles ----------

#[async_trait]
pub trait TicketSource: Send + Sync {
    async fn test_connection(&self) -> Result<String>;
    async fn search(&self, query: &str) -> Result<Vec<TicketRef>>;
    async fn get(&self, key: &str) -> Result<Ticket>;
    async fn transition(&self, key: &str, to: &str) -> Result<()>;
    /// Post a comment containing `marker` (hidden where the system allows).
    async fn comment(&self, key: &str, body: &str, marker: &str) -> Result<Comment>;
    async fn find_comment(&self, key: &str, marker: &str) -> Result<Option<Comment>>;
    /// Events since a cursor, for reconciliation polling.
    async fn poll(&self, cursor: Option<&str>) -> Result<(Vec<TicketEvent>, Option<String>)>;
}

#[async_trait]
pub trait RepoHost: Send + Sync {
    async fn test_connection(&self) -> Result<String>;
    async fn list_repos(&self) -> Result<Vec<RepoInfo>>;
    async fn repo_info(&self, repo: &str) -> Result<RepoInfo>;
    async fn list_files(&self, repo: &str, git_ref: &str) -> Result<Vec<String>>;
    async fn read_file(&self, repo: &str, git_ref: &str, path: &str) -> Result<Option<String>>;
    async fn search(&self, repo: &str, query: &str) -> Result<Vec<Value>>;
    fn git_remote(&self, repo: &str) -> Result<GitRemote>;
    async fn find_open_pr(&self, repo: &str, head: &str) -> Result<Option<Pr>>;
    async fn open_pr(&self, repo: &str, head: &str, base: &str, title: &str, body: &str) -> Result<Pr>;
    async fn get_pr(&self, repo: &str, number: i64) -> Result<PrDetail>;
    async fn list_prs(&self, repo: &str, state: &str) -> Result<Vec<Pr>>;
    /// General comment, or an inline thread when path and line are given.
    async fn comment(&self, repo: &str, number: i64, body: &str, path: Option<&str>, line: Option<i64>) -> Result<Value>;
    async fn reply(&self, repo: &str, number: i64, thread_id: &str, body: &str) -> Result<Value>;
    async fn resolve_thread(&self, repo: &str, number: i64, thread_id: &str, status: &str) -> Result<()>;
    async fn merge(&self, repo: &str, number: i64) -> Result<()>;
    async fn checks(&self, repo: &str, git_ref: &str) -> Result<Vec<CheckRun>>;
    async fn check_logs(&self, repo: &str, check_id: &str) -> Result<String>;
    async fn poll(&self, repo: &str, cursor: Option<&str>) -> Result<(Vec<RepoEvent>, Option<String>)>;
}

#[async_trait]
pub trait Chat: Send + Sync {
    async fn test_connection(&self) -> Result<String>;
    /// Post a message. `thread` is the thread to reply in, if any.
    async fn post(&self, channel: &str, thread: Option<&ChatRef>, msg: &ChatMessage) -> Result<ChatRef>;
    async fn update(&self, msg_ref: &ChatRef, text: &str) -> Result<()>;
    async fn find_by_marker(&self, channel: &str, marker: &str) -> Result<Option<ChatRef>>;
    async fn lookup_user_by_email(&self, email: &str) -> Result<Option<String>>;
    /// The email address of a chat user, if the service exposes it.
    async fn user_email(&self, user: &str) -> Result<Option<String>>;
}

// ---------- registry ----------

/// One choice of a `select` field.
#[derive(Serialize, Clone, Copy)]
pub struct Choice {
    pub value: &'static str,
    pub label: &'static str,
}

/// Show (and require) a field only while another config field has a given value.
#[derive(Serialize, Clone, Copy)]
pub struct ShowIf {
    pub field: &'static str,
    pub equals: &'static str,
}

/// One configuration or secret field of a connector kind, as the portal shows it.
#[derive(Serialize, Clone, Copy)]
pub struct Field {
    pub name: &'static str,
    pub label: &'static str,
    /// string | url | email | list (comma-separated, stored as a JSON array)
    #[serde(rename = "type")]
    pub ty: &'static str,
    pub required: bool,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub placeholder: &'static str,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub help: &'static str,
    /// Choices of a `select` field.
    #[serde(skip_serializing_if = "<[Choice]>::is_empty")]
    pub options: &'static [Choice],
    /// Value assumed when the field is not set.
    #[serde(skip_serializing_if = "str::is_empty")]
    pub default: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_if: Option<ShowIf>,
}

const fn field(name: &'static str, label: &'static str, ty: &'static str, required: bool, placeholder: &'static str, help: &'static str) -> Field {
    Field { name, label, ty, required, placeholder, help, options: &[], default: "", show_if: None }
}

impl Field {
    /// A `select` field with these choices and default.
    const fn select(self, options: &'static [Choice], default: &'static str) -> Field {
        Field { ty: "select", options, default, ..self }
    }
    /// Only shown, and only required, while config field `field` equals `equals`.
    const fn when(self, field: &'static str, equals: &'static str) -> Field {
        Field { show_if: Some(ShowIf { field, equals }), ..self }
    }
}

impl KindInfo {
    /// Effective value of a config field: the stored value or the field's default.
    pub fn config_value(&self, l: &Loaded, name: &str) -> Option<String> {
        l.cfg(name).or_else(|| self.config_fields.iter().find(|f| f.name == name).map(|f| f.default.to_string()).filter(|d| !d.is_empty()))
    }
    /// Whether a field applies under the connector's current configuration.
    pub fn applies(&self, l: &Loaded, f: &Field) -> bool {
        f.show_if.is_none_or(|c| self.config_value(l, c.field).as_deref() == Some(c.equals))
    }
}

pub struct KindInfo {
    pub kind: &'static str,
    pub label: &'static str,
    pub role: &'static str,
    pub description: &'static str,
    pub config_fields: &'static [Field],
    pub secret_fields: &'static [Field],
    /// Path segment of the inbound webhook route (`/webhooks/{segment}/{connector id}`), if any.
    pub webhook: Option<&'static str>,
    /// The webhook applies only while this holds (e.g. not in Slack Socket Mode).
    pub webhook_show_if: Option<ShowIf>,
}

/// Which OpenAI API the harness speaks to a model connector.
const WIRE_API: Field = field(
    "wire_api",
    "Wire API",
    "select",
    false,
    "",
    "Chat Completions works with any OpenAI-compatible endpoint. Use Responses for newer OpenAI models, which refuse tool calls with reasoning on Chat Completions.",
)
.select(&[Choice { value: "completions", label: "Chat Completions (/chat/completions)" }, Choice { value: "responses", label: "Responses (/responses)" }], "completions");

const WEBHOOK_SECRET: Field = field(
    "webhook_secret",
    "Webhook secret",
    "string",
    false,
    "",
    "Optional. Shared secret that signs inbound webhooks; without it webhooks are refused and the factory relies on polling.",
);

pub const KINDS: &[KindInfo] = &[
    KindInfo {
        kind: "jira",
        label: "Jira Cloud",
        role: "ticket",
        description: "Tickets from Jira Cloud (REST API v3).",
        webhook: Some("jira"),
        webhook_show_if: None,
        config_fields: &[
            field("base_url", "Site URL", "url", true, "https://acme.atlassian.net", ""),
            field("email", "Account email", "email", true, "factory-bot@acme.com", "The Atlassian account that owns the API token."),
            field(
                "project_keys",
                "Project keys",
                "string",
                false,
                "PROJ, OPS",
                "Jira project keys (not credentials), comma-separated. Limits search and polling to these projects; leave empty for every project the account can see.",
            ),
            field(
                "repo_field",
                "Repository field",
                "string",
                false,
                "customfield_10050",
                "Optional custom field holding the target repository. Without it, a label like repo:owner/name is used.",
            ),
        ],
        secret_fields: &[field("api_token", "API token", "string", true, "", "Create one at id.atlassian.com → Security → API tokens."), WEBHOOK_SECRET],
    },
    KindInfo {
        kind: "github",
        label: "GitHub",
        role: "repo",
        description: "Repositories on github.com or GitHub Enterprise Server.",
        webhook: Some("github"),
        webhook_show_if: None,
        config_fields: &[
            field("api_url", "API URL", "url", false, "https://api.github.com", "Change only for GitHub Enterprise Server (https://<host>/api/v3)."),
            field("owner", "Owner", "string", false, "acme", "User or organisation to list repositories from. Leave empty for everything the token can see."),
        ],
        secret_fields: &[field("token", "Access token", "string", true, "", "Fine-grained or classic personal access token, or an installation token."), WEBHOOK_SECRET],
    },
    KindInfo {
        kind: "azure_devops",
        label: "Azure DevOps",
        role: "repo",
        description: "Azure Repos and Pipelines in one Azure DevOps project.",
        webhook: Some("azure-devops"),
        webhook_show_if: None,
        config_fields: &[
            field("org_url", "Organisation URL", "url", true, "https://dev.azure.com/acme", ""),
            field("project", "Project", "string", true, "Platform", ""),
            field("search_url", "Code Search URL", "url", false, "https://almsearch.dev.azure.com/acme", "Derived automatically for dev.azure.com."),
        ],
        secret_fields: &[
            field("pat", "Personal access token", "string", true, "", "Needs Code (read & write), Pull Request Threads and Build (read)."),
            field("webhook_secret", "Service hook password", "string", false, "", "Optional. Basic-auth password configured on the service hook."),
        ],
    },
    KindInfo {
        kind: "slack",
        label: "Slack",
        role: "chat",
        description: "Questions, approvals and updates in Slack.",
        webhook: Some("slack"),
        webhook_show_if: Some(ShowIf { field: "mode", equals: "http" }),
        config_fields: &[
            field(
                "mode",
                "Connection mode",
                "select",
                false,
                "",
                "How Slack delivers replies and button presses. HTTP: Slack calls the webhook URL below, so the plane must be reachable over public HTTPS. Socket Mode: the plane opens an outbound WebSocket to Slack; no public URL needed.",
            )
            .select(&[Choice { value: "http", label: "HTTP (Events API)" }, Choice { value: "socket", label: "Socket Mode (WebSocket)" }], "http"),
            field("api_url", "API URL", "url", false, "https://slack.com/api", "Change only for testing against a mock."),
        ],
        secret_fields: &[
            field("bot_token", "Bot token", "string", true, "xoxb-…", ""),
            field("signing_secret", "Signing secret", "string", false, "", "From the Slack app's Basic Information page; needed for buttons and events.").when("mode", "http"),
            field("app_token", "App-level token", "string", true, "xapp-…", "From Basic Information → App-Level Tokens, with the connections:write scope.").when("mode", "socket"),
        ],
    },
    KindInfo {
        kind: "direct_model",
        label: "Model provider",
        role: "model",
        description: "An OpenAI-compatible model endpoint.",
        webhook: None,
        webhook_show_if: None,
        config_fields: &[field("base_url", "Base URL", "url", true, "https://api.example.com/v1", ""), WIRE_API],
        secret_fields: &[field("api_key", "API key", "string", false, "", "")],
    },
    KindInfo {
        kind: "litellm",
        label: "LiteLLM proxy",
        role: "model",
        description: "A LiteLLM proxy that issues per-run virtual keys.",
        webhook: None,
        webhook_show_if: None,
        config_fields: &[
            field("base_url", "Base URL", "url", true, "http://litellm:4000", "Where the plane reaches the proxy."),
            field("runner_base_url", "Runner base URL", "url", false, "", "Where runners reach the proxy, if different."),
            WIRE_API,
        ],
        secret_fields: &[field("master_key", "Master key", "string", true, "sk-…", "")],
    },
    KindInfo {
        kind: "package_feed",
        label: "Package feed",
        role: "feed",
        description: "Authenticated upstream package feed served through the egress proxy.",
        webhook: None,
        webhook_show_if: None,
        config_fields: &[
            field("name", "Feed name", "string", true, "npm-internal", "Runners reach the feed under this name on the egress proxy."),
            field("upstream", "Upstream URL", "url", true, "https://pkgs.example.com/npm/", ""),
        ],
        secret_fields: &[field("auth_header", "Authorization header", "string", false, "Bearer …", "Full header value the proxy adds upstream.")],
    },
    KindInfo { kind: "fake_tickets", label: "Fake tickets (testing)", role: "ticket", description: "In-database tickets for rehearsals.", webhook: None, webhook_show_if: None, config_fields: &[], secret_fields: &[] },
    KindInfo {
        kind: "fake_repo",
        label: "Fake repo (testing)",
        role: "repo",
        description: "Local git repositories for rehearsals.",
        webhook: None,
        webhook_show_if: None,
        config_fields: &[field("root", "Root directory", "string", true, "/data/fake-repos", ""), field("protected", "Protected branches", "list", false, "release, prod", "Comma-separated.")],
        secret_fields: &[],
    },
    KindInfo { kind: "fake_chat", label: "Fake chat (testing)", role: "chat", description: "In-database chat for rehearsals.", webhook: None, webhook_show_if: None, config_fields: &[], secret_fields: &[] },
    KindInfo {
        kind: "fake_model",
        label: "Fake model (testing)",
        role: "model",
        description: "Scripted model endpoint for rehearsals.",
        webhook: None,
        webhook_show_if: None,
        config_fields: &[field("base_url", "Base URL", "url", false, "", "Defaults to the plane's built-in fake model.")],
        secret_fields: &[field("api_key", "API key", "string", false, "", "")],
    },
];

pub fn kind_info(kind: &str) -> Option<&'static KindInfo> {
    KINDS.iter().find(|k| k.kind == kind)
}

#[derive(Clone, sqlx::FromRow)]
pub struct ConnectorRow {
    pub id: Uuid,
    pub kind: String,
    pub role: String,
    pub name: String,
    pub config: Value,
    pub service_identity: Option<String>,
}

/// A loaded connector: its row plus decrypted secrets.
pub struct Loaded {
    pub row: ConnectorRow,
    pub secrets: HashMap<String, String>,
}

impl Loaded {
    pub fn cfg(&self, k: &str) -> Option<String> {
        self.row.config.get(k).and_then(|v| v.as_str()).map(|s| s.to_string()).filter(|s| !s.is_empty())
    }
    pub fn secret(&self, k: &str) -> Result<String> {
        self.secrets
            .get(k)
            .cloned()
            .ok_or_else(|| anyhow!("connector {} has no secret {k}", self.row.name))
    }
}

pub async fn load(state: &AppState, id: Uuid) -> Result<Loaded> {
    let row: ConnectorRow = sqlx::query_as(
        "SELECT id, kind, role, name, config, service_identity FROM connectors WHERE id=$1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| anyhow!("connector {id} not found"))?;
    let rows: Vec<(String, String, Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT name, key_id, nonce, ciphertext FROM secrets WHERE connector_id=$1")
            .bind(id)
            .fetch_all(&state.pool)
            .await?;
    let mut secrets = HashMap::new();
    for (name, kid, nonce, ct) in rows {
        let pt = state.crypto.decrypt(&kid, &nonce, &ct)?;
        secrets.insert(name, String::from_utf8(pt)?);
    }
    // Audit connector and secret use (per run where known is recorded by callers).
    Ok(Loaded { row, secrets })
}

pub async fn ticket_source(state: &AppState, id: Uuid) -> Result<Arc<dyn TicketSource>> {
    ticket_source_from(state, &load(state, id).await?)
}

fn ticket_source_from(state: &AppState, l: &Loaded) -> Result<Arc<dyn TicketSource>> {
    Ok(match l.row.kind.as_str() {
        "fake_tickets" => Arc::new(fake::FakeTickets::new(state.pool.clone(), l.row.id)),
        "jira" => Arc::new(jira::Jira::from_loaded(l)?),
        k => bail!("connector kind {k} is not a ticket source"),
    })
}

pub async fn repo_host(state: &AppState, id: Uuid) -> Result<Arc<dyn RepoHost>> {
    repo_host_from(state, &load(state, id).await?)
}

fn repo_host_from(state: &AppState, l: &Loaded) -> Result<Arc<dyn RepoHost>> {
    Ok(match l.row.kind.as_str() {
        "fake_repo" => Arc::new(fake::FakeRepo::from_loaded(state.pool.clone(), l)?),
        "github" => Arc::new(github::GitHub::from_loaded(l)?),
        "azure_devops" => Arc::new(ado::AzureDevOps::from_loaded(l)?),
        k => bail!("connector kind {k} is not a repo host"),
    })
}

pub async fn chat(state: &AppState, id: Uuid) -> Result<Arc<dyn Chat>> {
    chat_from(state, &load(state, id).await?)
}

fn chat_from(state: &AppState, l: &Loaded) -> Result<Arc<dyn Chat>> {
    Ok(match l.row.kind.as_str() {
        "fake_chat" => Arc::new(fake::FakeChat::new(state.pool.clone(), l.row.id)),
        "slack" => Arc::new(slack::Slack::from_loaded(l)?),
        k => bail!("connector kind {k} is not a chat connector"),
    })
}

pub async fn service_identity(state: &AppState, id: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT service_identity FROM connectors WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .flatten()
}

/// "Test connection" for a saved connector.
pub async fn test(state: &AppState, id: Uuid) -> Result<String> {
    test_loaded(state, &load(state, id).await?).await
}

/// "Test connection" for settings that may not be saved yet: `config` replaces the stored
/// configuration and non-empty `secrets` override the stored ones (when `id` names a saved
/// connector). Nothing is written.
pub async fn test_draft(state: &AppState, id: Option<Uuid>, kind: &str, config: Value, secrets: HashMap<String, String>) -> Result<String> {
    let info = kind_info(kind).ok_or_else(|| anyhow!("unknown connector kind {kind}"))?;
    let mut l = match id {
        Some(id) => load(state, id).await?,
        None => Loaded {
            row: ConnectorRow { id: Uuid::new_v4(), kind: kind.into(), role: info.role.into(), name: "draft".into(), config: json!({}), service_identity: None },
            secrets: HashMap::new(),
        },
    };
    if l.row.kind != kind {
        bail!("connector kind cannot change");
    }
    l.row.config = config;
    l.secrets.extend(secrets.into_iter().filter(|(_, v)| !v.is_empty()));
    for f in info.config_fields.iter().filter(|f| f.required && info.applies(&l, f)) {
        if l.cfg(f.name).is_none() && !l.row.config.get(f.name).is_some_and(|v| v.is_array()) {
            bail!("{} is required", f.label);
        }
    }
    for f in info.secret_fields.iter().filter(|f| f.required && info.applies(&l, f)) {
        if !l.secrets.contains_key(f.name) {
            bail!("{} is required", f.label);
        }
    }
    test_loaded(state, &l).await
}

async fn test_loaded(state: &AppState, l: &Loaded) -> Result<String> {
    match l.row.role.as_str() {
        "ticket" => ticket_source_from(state, l)?.test_connection().await,
        "repo" => repo_host_from(state, l)?.test_connection().await,
        "chat" => chat_from(state, l)?.test_connection().await,
        "model" => model::test_connection(state, l).await,
        "feed" => Ok("feed credentials stored; used by the egress proxy".into()),
        r => bail!("unknown role {r}"),
    }
}

pub fn connector_json(row: &ConnectorRow, secrets: &[(String, String, chrono::DateTime<chrono::Utc>)]) -> Value {
    json!({
        "id": row.id, "kind": row.kind, "role": row.role, "name": row.name, "config": row.config,
        "service_identity": row.service_identity,
        "secrets": secrets.iter().map(|(n, h, u)| json!({"name": n, "hint": h, "updated_at": u})).collect::<Vec<_>>(),
    })
}
