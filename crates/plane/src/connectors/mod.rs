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
}

// ---------- registry ----------

pub struct KindInfo {
    pub kind: &'static str,
    pub role: &'static str,
    pub config_fields: &'static [&'static str],
    pub secret_fields: &'static [&'static str],
}

pub const KINDS: &[KindInfo] = &[
    KindInfo { kind: "fake_tickets", role: "ticket", config_fields: &[], secret_fields: &[] },
    KindInfo { kind: "jira", role: "ticket", config_fields: &["base_url", "email", "project_keys", "webhook_path"], secret_fields: &["api_token", "webhook_secret"] },
    KindInfo { kind: "fake_repo", role: "repo", config_fields: &["root", "protected"], secret_fields: &[] },
    KindInfo { kind: "github", role: "repo", config_fields: &["api_url", "owner"], secret_fields: &["token", "webhook_secret"] },
    KindInfo { kind: "azure_devops", role: "repo", config_fields: &["org_url", "project"], secret_fields: &["pat", "webhook_secret"] },
    KindInfo { kind: "fake_chat", role: "chat", config_fields: &[], secret_fields: &[] },
    KindInfo { kind: "slack", role: "chat", config_fields: &["api_url"], secret_fields: &["bot_token", "signing_secret"] },
    KindInfo { kind: "fake_model", role: "model", config_fields: &["base_url"], secret_fields: &["api_key"] },
    KindInfo { kind: "direct_model", role: "model", config_fields: &["base_url"], secret_fields: &["api_key"] },
    KindInfo { kind: "litellm", role: "model", config_fields: &["base_url", "runner_base_url"], secret_fields: &["master_key"] },
    KindInfo { kind: "package_feed", role: "feed", config_fields: &["name", "upstream"], secret_fields: &["auth_header"] },
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
    let l = load(state, id).await?;
    Ok(match l.row.kind.as_str() {
        "fake_tickets" => Arc::new(fake::FakeTickets::new(state.pool.clone(), l.row.id)),
        "jira" => Arc::new(jira::Jira::from_loaded(&l)?),
        k => bail!("connector kind {k} is not a ticket source"),
    })
}

pub async fn repo_host(state: &AppState, id: Uuid) -> Result<Arc<dyn RepoHost>> {
    let l = load(state, id).await?;
    Ok(match l.row.kind.as_str() {
        "fake_repo" => Arc::new(fake::FakeRepo::from_loaded(state.pool.clone(), &l)?),
        "github" => Arc::new(github::GitHub::from_loaded(&l)?),
        "azure_devops" => Arc::new(ado::AzureDevOps::from_loaded(&l)?),
        k => bail!("connector kind {k} is not a repo host"),
    })
}

pub async fn chat(state: &AppState, id: Uuid) -> Result<Arc<dyn Chat>> {
    let l = load(state, id).await?;
    Ok(match l.row.kind.as_str() {
        "fake_chat" => Arc::new(fake::FakeChat::new(state.pool.clone(), l.row.id)),
        "slack" => Arc::new(slack::Slack::from_loaded(&l)?),
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

/// "Test connection" for any connector kind.
pub async fn test(state: &AppState, id: Uuid) -> Result<String> {
    let l = load(state, id).await?;
    match l.row.role.as_str() {
        "ticket" => ticket_source(state, id).await?.test_connection().await,
        "repo" => repo_host(state, id).await?.test_connection().await,
        "chat" => chat(state, id).await?.test_connection().await,
        "model" => model::test_connection(state, &l).await,
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
