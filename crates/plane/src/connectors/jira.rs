//! Jira Cloud ticket source (REST API v3).
//!
//! Configuration: `base_url` (e.g. `https://acme.atlassian.net`), `email` (the account that owns the
//! API token), `project_keys` (comma-separated, scopes search and polling) and optionally `repo_field`
//! (a custom field id such as `customfield_10050`). Secrets: `api_token` (sent as HTTP Basic
//! `email:api_token`) and `webhook_secret` (used by [`verify_signature`]).
//!
//! Conventions:
//! - **Repository**: `Ticket.repo` comes from the custom field named by `repo_field` when it is set and
//!   non-empty (plain string, select option `{value}` or the first entry of a list), otherwise from a
//!   label of the form `repo:owner/name`.
//! - **Markers**: a comment posted by [`Jira::comment`] carries its marker twice: as a comment entity
//!   property `factory.marker` = `{"marker": "..."}` (hidden), and as a final ADF paragraph with the
//!   marker text in a code mark (visible, so it also survives copy/edit). [`Jira::find_comment`] matches
//!   either.
//! - **Event ids** are stable across webhook and polling: `jira-{issue id}-cl-{changelog id}-label-{label}`,
//!   `jira-{issue id}-cl-{changelog id}-status`, `jira-{issue id}-comment-{comment id}`,
//!   `jira-{issue id}-created`, `jira-{issue id}-created-label-{label}`, `jira-{issue id}-deleted`.
//! - A status change to a cancel-like status (Cancelled, Canceled, Won't Do, Rejected) is reported as
//!   kind `cancelled`, other status changes as `transitioned`.
//! - **Polling** uses relative JQL (`updated >= "-Nm"`), which is independent of the API user's time
//!   zone, and filters events by timestamp against the cursor (an RFC 3339 UTC timestamp). Without a
//!   cursor, polling starts now and returns no events.
//! - Comment bodies are converted to ADF as plain paragraphs (no Markdown rendering).
//!
//! Contract tests (`#[ignore]`) read `FACTORY_CONTRACT_JIRA_BASE_URL`, `FACTORY_CONTRACT_JIRA_EMAIL`,
//! `FACTORY_CONTRACT_JIRA_TOKEN` and `FACTORY_CONTRACT_JIRA_ISSUE` (an issue key). The comment round
//! trip additionally needs `FACTORY_CONTRACT_JIRA_WRITE=1`.

use super::*;
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use reqwest::{Method, RequestBuilder, StatusCode};
use std::collections::BTreeSet;
use std::time::Duration;

const MARKER_PROPERTY: &str = "factory.marker";
const PAGE: usize = 100;
const MAX_PAGES: usize = 50;
const POLL_MAX_ISSUES: usize = 500;

pub struct Jira {
    http: reqwest::Client,
    base: String,
    email: String,
    token: String,
    projects: Vec<String>,
    repo_field: Option<String>,
}

impl Jira {
    pub fn from_loaded(l: &Loaded) -> Result<Self> {
        let base = l.cfg("base_url").ok_or_else(|| anyhow!("jira connector needs config.base_url"))?;
        let email = l.cfg("email").ok_or_else(|| anyhow!("jira connector needs config.email"))?;
        let token = l.secret("api_token")?;
        let projects = l
            .cfg("project_keys")
            .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
            .unwrap_or_default();
        Ok(Jira {
            http: reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?,
            base: base.trim_end_matches('/').to_string(),
            email,
            token,
            projects,
            repo_field: l.cfg("repo_field"),
        })
    }

    fn req(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base, path))
            .basic_auth(&self.email, Some(&self.token))
            .header(reqwest::header::ACCEPT, "application/json")
    }

    /// Send a request, retrying on 429 (honouring `Retry-After`), and return the JSON body
    /// (`Null` for an empty body).
    async fn send(&self, rb: RequestBuilder) -> Result<Value> {
        let mut attempt = 0;
        loop {
            let this = rb.try_clone().ok_or_else(|| anyhow!("request cannot be retried"))?;
            let resp = this.send().await?;
            let status = resp.status();
            if status == StatusCode::TOO_MANY_REQUESTS && attempt < 3 {
                attempt += 1;
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(2)
                    .min(30);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            let url = resp.url().path().to_string();
            let text = resp.text().await?;
            if !status.is_success() {
                let snippet: String = text.chars().take(500).collect();
                bail!("jira {url} returned {status}: {snippet}");
            }
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return Ok(serde_json::from_str(&text)?);
        }
    }

    fn project_clause(&self) -> Option<String> {
        if self.projects.is_empty() {
            None
        } else {
            Some(format!("project in ({})", self.projects.iter().map(|p| jql_quote(p)).collect::<Vec<_>>().join(", ")))
        }
    }

    /// Enhanced JQL search (`GET /rest/api/3/search/jql`), following `nextPageToken`.
    async fn search_jql(&self, jql: &str, fields: &str, limit: usize) -> Result<Vec<Value>> {
        let mut out = vec![];
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut q: Vec<(&str, String)> = vec![
                ("jql", jql.to_string()),
                ("maxResults", PAGE.min(limit.saturating_sub(out.len()).max(1)).to_string()),
                ("fields", fields.to_string()),
            ];
            if let Some(t) = &token {
                q.push(("nextPageToken", t.clone()));
            }
            let v = self.send(self.req(Method::GET, "/rest/api/3/search/jql").query(&q)).await?;
            if let Some(a) = v["issues"].as_array() {
                out.extend(a.iter().cloned());
            }
            let next = v["nextPageToken"].as_str().map(String::from);
            if out.len() >= limit || v["isLast"].as_bool() == Some(true) || next.is_none() || next == token {
                break;
            }
            token = next;
        }
        out.truncate(limit);
        Ok(out)
    }

    async fn list_comments(&self, key: &str) -> Result<Vec<Value>> {
        let key = seg(key)?;
        let mut out = vec![];
        let mut start = 0usize;
        for _ in 0..MAX_PAGES {
            let q = [
                ("startAt", start.to_string()),
                ("maxResults", PAGE.to_string()),
                ("orderBy", "created".to_string()),
                ("expand", "properties".to_string()),
            ];
            let v = self.send(self.req(Method::GET, &format!("/rest/api/3/issue/{key}/comment")).query(&q)).await?;
            let page = v["comments"].as_array().cloned().unwrap_or_default();
            let n = page.len();
            out.extend(page);
            start += n;
            let total = v["total"].as_u64().unwrap_or(0) as usize;
            if n == 0 || start >= total {
                break;
            }
        }
        Ok(out)
    }

    async fn list_changelog(&self, issue_id: &str) -> Result<Vec<Value>> {
        let id = seg(issue_id)?;
        let mut out = vec![];
        let mut start = 0usize;
        for _ in 0..MAX_PAGES {
            let q = [("startAt", start.to_string()), ("maxResults", PAGE.to_string())];
            let v = self.send(self.req(Method::GET, &format!("/rest/api/3/issue/{id}/changelog")).query(&q)).await?;
            let page = v["values"].as_array().cloned().unwrap_or_default();
            let n = page.len();
            out.extend(page);
            start += n;
            let total = v["total"].as_u64().unwrap_or(0) as usize;
            if n == 0 || v["isLast"].as_bool() == Some(true) || (total > 0 && start >= total) {
                break;
            }
        }
        Ok(out)
    }

    fn repo_of(&self, fields: &Value, labels: &[String]) -> Option<String> {
        if let Some(f) = &self.repo_field {
            let v = &fields[f.as_str()];
            let s = match v {
                Value::String(s) => Some(s.clone()),
                Value::Object(_) => v["value"].as_str().or(v["name"].as_str()).map(String::from),
                Value::Array(a) => a.first().and_then(|x| x.as_str().map(String::from).or(x["value"].as_str().map(String::from))),
                _ => None,
            };
            if let Some(s) = s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
                return Some(s);
            }
        }
        labels.iter().find_map(|l| l.strip_prefix("repo:").map(String::from)).filter(|s| !s.is_empty())
    }
}

#[async_trait]
impl TicketSource for Jira {
    async fn test_connection(&self) -> Result<String> {
        let v = self.send(self.req(Method::GET, "/rest/api/3/myself")).await?;
        let who = v["displayName"].as_str().or(v["emailAddress"].as_str()).unwrap_or("unknown user");
        Ok(format!("connected to Jira at {} as {who}", self.base))
    }

    async fn search(&self, query: &str) -> Result<Vec<TicketRef>> {
        let q = query.trim();
        let scope = self.project_clause();
        let jql = if looks_like_jql(q) {
            q.to_string()
        } else if is_issue_key(q) {
            format!("key = {}", jql_quote(q))
        } else {
            let mut parts: Vec<String> = scope.into_iter().collect();
            if q.is_empty() {
                if parts.is_empty() {
                    parts.push("updated >= \"-30d\"".into());
                }
            } else {
                parts.push(format!("text ~ {}", jql_quote(q)));
            }
            format!("{} ORDER BY updated DESC", parts.join(" AND "))
        };
        let issues = self.search_jql(&jql, "summary,status", 50).await?;
        Ok(issues.iter().map(issue_ref).collect())
    }

    async fn get(&self, key: &str) -> Result<Ticket> {
        let k = seg(key)?;
        let mut fields = "summary,description,status,assignee,labels,subtasks,issuetype,parent,created,updated".to_string();
        if let Some(f) = &self.repo_field {
            fields.push(',');
            fields.push_str(f);
        }
        let v = self.send(self.req(Method::GET, &format!("/rest/api/3/issue/{k}")).query(&[("fields", fields)])).await?;
        let f = &v["fields"];
        let issue_key = v["key"].as_str().unwrap_or(key).to_string();
        let labels: Vec<String> = f["labels"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        let mut children: Vec<TicketRef> = f["subtasks"].as_array().map(|a| a.iter().map(issue_ref).collect()).unwrap_or_default();
        for c in self.search_jql(&format!("parent = {}", jql_quote(&issue_key)), "summary,status", 200).await? {
            let r = issue_ref(&c);
            if !children.iter().any(|x| x.key == r.key) {
                children.push(r);
            }
        }
        let comments = self.list_comments(&issue_key).await?.iter().map(to_comment).collect();
        Ok(Ticket {
            key: issue_key.clone(),
            title: f["summary"].as_str().unwrap_or_default().to_string(),
            description: body_text(&f["description"]),
            status: f["status"]["name"].as_str().unwrap_or_default().to_string(),
            assignee_email: f["assignee"]["emailAddress"].as_str().map(String::from),
            repo: self.repo_of(f, &labels),
            labels,
            children,
            comments,
            url: Some(format!("{}/browse/{issue_key}", self.base)),
        })
    }

    async fn transition(&self, key: &str, to: &str) -> Result<()> {
        let k = seg(key)?;
        let cur = self.send(self.req(Method::GET, &format!("/rest/api/3/issue/{k}")).query(&[("fields", "status")])).await?;
        if cur["fields"]["status"]["name"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(to)) {
            return Ok(());
        }
        let v = self.send(self.req(Method::GET, &format!("/rest/api/3/issue/{k}/transitions"))).await?;
        let ts = v["transitions"].as_array().cloned().unwrap_or_default();
        let pick = ts
            .iter()
            .find(|t| t["to"]["name"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(to)))
            .or_else(|| ts.iter().find(|t| t["name"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(to))));
        let Some(t) = pick else {
            let avail: Vec<&str> = ts.iter().filter_map(|t| t["to"]["name"].as_str()).collect();
            bail!("no transition of {key} leads to status '{to}' (available: {})", avail.join(", "));
        };
        let id = t["id"].as_str().map(String::from).unwrap_or_else(|| t["id"].to_string());
        self.send(self.req(Method::POST, &format!("/rest/api/3/issue/{k}/transitions")).json(&json!({"transition": {"id": id}}))).await?;
        Ok(())
    }

    async fn comment(&self, key: &str, body: &str, marker: &str) -> Result<Comment> {
        let k = seg(key)?;
        let payload = json!({
            "body": text_to_adf(body, Some(marker)),
            "properties": [{"key": MARKER_PROPERTY, "value": {"marker": marker}}],
        });
        let v = self.send(self.req(Method::POST, &format!("/rest/api/3/issue/{k}/comment")).json(&payload)).await?;
        Ok(to_comment(&v))
    }

    async fn find_comment(&self, key: &str, marker: &str) -> Result<Option<Comment>> {
        let all = self.list_comments(key).await?;
        Ok(all.iter().find(|c| comment_has_marker(c, marker)).map(to_comment))
    }

    async fn poll(&self, cursor: Option<&str>) -> Result<(Vec<TicketEvent>, Option<String>)> {
        let now = Utc::now();
        let Some(since) = cursor.and_then(parse_time) else {
            return Ok((vec![], Some(now.to_rfc3339())));
        };
        let minutes = ((now - since).num_seconds().max(0) / 60) + 2;
        let mut parts: Vec<String> = self.project_clause().into_iter().collect();
        parts.push(format!("updated >= \"-{minutes}m\""));
        let jql = format!("{} ORDER BY updated ASC", parts.join(" AND "));
        let issues = self.search_jql(&jql, "summary,status,labels,created,updated,creator,reporter", POLL_MAX_ISSUES).await?;
        let mut events = vec![];
        let mut next = since;
        for issue in &issues {
            let f = &issue["fields"];
            let Some(updated) = f["updated"].as_str().and_then(parse_time) else { continue };
            if updated < since {
                continue;
            }
            let id = issue["id"].as_str().unwrap_or_default().to_string();
            let key = issue["key"].as_str().unwrap_or_default().to_string();
            if f["created"].as_str().and_then(parse_time).is_some_and(|c| c >= since) {
                let author = who(if f["creator"].is_object() { &f["creator"] } else { &f["reporter"] });
                events.extend(created_events(&id, &key, &f["labels"], author));
            }
            for h in self.list_changelog(&id).await? {
                if h["created"].as_str().and_then(parse_time).is_some_and(|c| c >= since) {
                    let clid = h["id"].as_str().map(String::from).unwrap_or_else(|| h["id"].to_string());
                    events.extend(changelog_events(&id, &key, &clid, &h["items"], who(&h["author"])));
                }
            }
            for c in self.list_comments(&key).await? {
                if c["created"].as_str().and_then(parse_time).is_some_and(|t| t >= since) {
                    events.push(comment_event(&id, &key, &c));
                }
            }
            if updated > next {
                next = updated;
            }
        }
        Ok((events, Some(next.to_rfc3339())))
    }
}

// ---------- webhooks ----------

/// Jira webhooks registered with a secret send `X-Hub-Signature: sha256=<hex HMAC-SHA256 of the body>`.
pub fn verify_signature(secret: &str, headers: &HeaderMap, body: &[u8]) -> bool {
    use hmac::{Hmac, KeyInit, Mac};
    let Some(sig) = headers.get("x-hub-signature").and_then(|v| v.to_str().ok()) else { return false };
    let Some((method, hexsig)) = sig.trim().split_once('=') else { return false };
    if !method.eq_ignore_ascii_case("sha256") {
        return false;
    }
    let Ok(expected) = hex::decode(hexsig.trim()) else { return false };
    let Ok(mut mac) = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(secret.as_bytes()) else { return false };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// Parse a Jira webhook delivery (`jira:issue_created`, `jira:issue_updated`, `jira:issue_deleted`,
/// `comment_created`). Other events yield no ticket events.
pub fn parse_webhook(_headers: &HeaderMap, body: &[u8]) -> Result<Vec<TicketEvent>> {
    let v: Value = serde_json::from_slice(body)?;
    let event = v["webhookEvent"].as_str().unwrap_or_default();
    let issue = &v["issue"];
    let id = issue["id"].as_str().map(String::from).unwrap_or_else(|| issue["id"].to_string());
    let key = issue["key"].as_str().unwrap_or_default().to_string();
    let user = who(&v["user"]);
    Ok(match event {
        "jira:issue_created" => created_events(&id, &key, &issue["fields"]["labels"], user),
        "jira:issue_updated" => {
            let cl = &v["changelog"];
            let mut evs = vec![];
            if cl.is_object() {
                let clid = cl["id"].as_str().map(String::from).unwrap_or_else(|| cl["id"].to_string());
                evs = changelog_events(&id, &key, &clid, &cl["items"], user);
            }
            if v["comment"].is_object() {
                evs.push(comment_event(&id, &key, &v["comment"]));
            }
            evs
        }
        "jira:issue_deleted" => vec![TicketEvent {
            id: format!("jira-{id}-deleted"),
            key,
            kind: "cancelled".into(),
            author: user,
            label: None,
            status: Some("Deleted".into()),
            body: None,
        }],
        "comment_created" => vec![comment_event(&id, &key, &v["comment"])],
        _ => vec![],
    })
}

// ---------- event helpers ----------

fn is_cancel_status(s: &str) -> bool {
    matches!(s.trim().to_lowercase().as_str(), "cancelled" | "canceled" | "won't do" | "wont do" | "wont_do" | "rejected")
}

fn changelog_events(issue_id: &str, key: &str, clid: &str, items: &Value, author: Option<String>) -> Vec<TicketEvent> {
    let mut out = vec![];
    for it in items.as_array().into_iter().flatten() {
        let field = it["fieldId"].as_str().or(it["field"].as_str()).unwrap_or_default();
        match field.to_lowercase().as_str() {
            "labels" => {
                let from: BTreeSet<&str> = it["fromString"].as_str().unwrap_or_default().split_whitespace().collect();
                for l in it["toString"].as_str().unwrap_or_default().split_whitespace() {
                    if !from.contains(l) {
                        out.push(TicketEvent {
                            id: format!("jira-{issue_id}-cl-{clid}-label-{l}"),
                            key: key.into(),
                            kind: "labeled".into(),
                            author: author.clone(),
                            label: Some(l.into()),
                            status: None,
                            body: None,
                        });
                    }
                }
            }
            "status" => {
                let to = it["toString"].as_str().unwrap_or_default().to_string();
                out.push(TicketEvent {
                    id: format!("jira-{issue_id}-cl-{clid}-status"),
                    key: key.into(),
                    kind: if is_cancel_status(&to) { "cancelled".into() } else { "transitioned".into() },
                    author: author.clone(),
                    label: None,
                    status: Some(to),
                    body: None,
                });
            }
            _ => {}
        }
    }
    out
}

fn created_events(issue_id: &str, key: &str, labels: &Value, author: Option<String>) -> Vec<TicketEvent> {
    let mut out = vec![TicketEvent {
        id: format!("jira-{issue_id}-created"),
        key: key.into(),
        kind: "created".into(),
        author: author.clone(),
        label: None,
        status: None,
        body: None,
    }];
    for l in labels.as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
        out.push(TicketEvent {
            id: format!("jira-{issue_id}-created-label-{l}"),
            key: key.into(),
            kind: "labeled".into(),
            author: author.clone(),
            label: Some(l.into()),
            status: None,
            body: None,
        });
    }
    out
}

fn comment_event(issue_id: &str, key: &str, c: &Value) -> TicketEvent {
    let cid = c["id"].as_str().map(String::from).unwrap_or_else(|| c["id"].to_string());
    TicketEvent {
        id: format!("jira-{issue_id}-comment-{cid}"),
        key: key.into(),
        kind: "commented".into(),
        author: who(&c["author"]),
        label: None,
        status: None,
        body: Some(body_text(&c["body"])),
    }
}

// ---------- conversion helpers ----------

/// The identity of a Jira user object: email when visible, otherwise account id.
fn who(u: &Value) -> Option<String> {
    u["emailAddress"]
        .as_str()
        .or(u["accountId"].as_str())
        .or(u["name"].as_str())
        .or(u["displayName"].as_str())
        .map(String::from)
}

fn issue_ref(v: &Value) -> TicketRef {
    TicketRef {
        key: v["key"].as_str().unwrap_or_default().to_string(),
        title: v["fields"]["summary"].as_str().unwrap_or_default().to_string(),
        status: v["fields"]["status"]["name"].as_str().unwrap_or_default().to_string(),
    }
}

fn to_comment(c: &Value) -> Comment {
    Comment {
        id: c["id"].as_str().map(String::from).unwrap_or_else(|| c["id"].to_string()),
        author: who(&c["author"]).unwrap_or_default(),
        body: body_text(&c["body"]),
        created_at: c["created"].as_str().map(String::from),
    }
}

fn comment_has_marker(c: &Value, marker: &str) -> bool {
    let prop = c["properties"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|p| p["key"].as_str() == Some(MARKER_PROPERTY) && p["value"]["marker"].as_str() == Some(marker));
    prop || body_text(&c["body"]).contains(marker)
}

/// Text of a body that is ADF (REST v3) or a plain/wiki string (webhook payloads use v2 shapes).
fn body_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(_) => adf_to_text(v),
        _ => String::new(),
    }
}

/// Convert an Atlassian Document Format node to plain text.
pub fn adf_to_text(node: &Value) -> String {
    let mut out = String::new();
    adf_block(node, &mut out, "");
    out.trim().to_string()
}

fn adf_inline(node: &Value, out: &mut String) {
    match node["type"].as_str().unwrap_or_default() {
        "text" => out.push_str(node["text"].as_str().unwrap_or_default()),
        "hardBreak" => out.push('\n'),
        "mention" => out.push_str(node["attrs"]["text"].as_str().unwrap_or("@user")),
        "emoji" => out.push_str(node["attrs"]["text"].as_str().or(node["attrs"]["shortName"].as_str()).unwrap_or_default()),
        "inlineCard" | "blockCard" | "embedCard" => out.push_str(node["attrs"]["url"].as_str().unwrap_or_default()),
        "date" => out.push_str(node["attrs"]["timestamp"].as_str().unwrap_or_default()),
        "status" => out.push_str(node["attrs"]["text"].as_str().unwrap_or_default()),
        _ => {
            for c in node["content"].as_array().into_iter().flatten() {
                adf_inline(c, out);
            }
        }
    }
}

fn adf_block(node: &Value, out: &mut String, indent: &str) {
    let children = node["content"].as_array().cloned().unwrap_or_default();
    let ensure_break = |out: &mut String| {
        if !out.is_empty() && !out.ends_with("\n\n") {
            if out.ends_with('\n') {
                out.push('\n');
            } else {
                out.push_str("\n\n");
            }
        }
    };
    match node["type"].as_str().unwrap_or_default() {
        "doc" | "blockquote" | "panel" | "expand" | "nestedExpand" | "layoutSection" | "layoutColumn" => {
            for c in &children {
                adf_block(c, out, indent);
            }
        }
        "paragraph" | "heading" => {
            ensure_break(out);
            let mut s = String::new();
            for c in &children {
                adf_inline(c, &mut s);
            }
            out.push_str(&s);
        }
        "codeBlock" => {
            ensure_break(out);
            let mut s = String::new();
            for c in &children {
                adf_inline(c, &mut s);
            }
            out.push_str(&s);
        }
        "bulletList" | "orderedList" => {
            ensure_break(out);
            let ordered = node["type"] == "orderedList";
            let start = node["attrs"]["order"].as_u64().unwrap_or(1);
            for (i, item) in children.iter().enumerate() {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(indent);
                if ordered {
                    out.push_str(&format!("{}. ", start + i as u64));
                } else {
                    out.push_str("- ");
                }
                let mut first = true;
                for c in item["content"].as_array().into_iter().flatten() {
                    match c["type"].as_str().unwrap_or_default() {
                        "bulletList" | "orderedList" => {
                            out.push('\n');
                            let mut nested = String::new();
                            adf_block(c, &mut nested, &format!("{indent}  "));
                            out.push_str(nested.trim_end());
                        }
                        _ => {
                            if !first {
                                out.push(' ');
                            }
                            let mut s = String::new();
                            adf_inline(c, &mut s);
                            out.push_str(&s);
                        }
                    }
                    first = false;
                }
            }
        }
        "rule" => {
            ensure_break(out);
            out.push_str("---");
        }
        "table" => {
            ensure_break(out);
            for row in &children {
                let cells: Vec<String> = row["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|cell| {
                        let mut s = String::new();
                        adf_block(cell, &mut s, "");
                        s.trim().replace('\n', " ")
                    })
                    .collect();
                out.push_str(&cells.join(" | "));
                out.push('\n');
            }
        }
        "tableCell" | "tableHeader" | "listItem" => {
            for c in &children {
                adf_block(c, out, indent);
            }
        }
        "mediaSingle" | "mediaGroup" | "media" => {}
        _ => {
            // Inline node at block level, or unknown block: render its text.
            let mut s = String::new();
            adf_inline(node, &mut s);
            out.push_str(&s);
        }
    }
}

/// Build an ADF document from plain text: blank lines separate paragraphs, single newlines become
/// hard breaks. The marker, when given, is a final paragraph in a code mark.
pub fn text_to_adf(text: &str, marker: Option<&str>) -> Value {
    let norm = text.replace("\r\n", "\n");
    let mut content: Vec<Value> = vec![];
    for para in norm.split("\n\n") {
        let mut inl: Vec<Value> = vec![];
        for line in para.split('\n') {
            if line.is_empty() {
                continue;
            }
            if !inl.is_empty() {
                inl.push(json!({"type": "hardBreak"}));
            }
            inl.push(json!({"type": "text", "text": line}));
        }
        if !inl.is_empty() {
            content.push(json!({"type": "paragraph", "content": inl}));
        }
    }
    if let Some(m) = marker.filter(|m| !m.is_empty()) {
        content.push(json!({"type": "paragraph", "content": [{"type": "text", "text": m, "marks": [{"type": "code"}]}]}));
    }
    if content.is_empty() {
        content.push(json!({"type": "paragraph", "content": []}));
    }
    json!({"type": "doc", "version": 1, "content": content})
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .or_else(|_| DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%z"))
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn jql_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn is_issue_key(s: &str) -> bool {
    let Some((p, n)) = s.split_once('-') else { return false };
    !p.is_empty()
        && p.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !n.is_empty()
        && n.chars().all(|c| c.is_ascii_digit())
}

fn looks_like_jql(s: &str) -> bool {
    let l = s.to_lowercase();
    [" = ", " != ", " ~ ", " in (", " is ", " order by ", ">=", "<="].iter().any(|t| l.contains(t)) || l.starts_with("order by ")
}

/// Validate an issue key or id for use in a URL path.
fn seg(s: &str) -> Result<&str> {
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
        bail!("bad Jira issue key {s:?}");
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::http::{HeaderValue, Uri};
    use base64::Engine;
    use std::sync::Mutex;

    #[derive(Clone, Debug)]
    struct Rec {
        method: String,
        path: String,
        query: HashMap<String, String>,
        auth: String,
        body: Value,
    }
    type Handler = Arc<dyn Fn(&Rec) -> (u16, Value) + Send + Sync>;

    async fn mock(h: Handler) -> (String, Arc<Mutex<Vec<Rec>>>) {
        let log: Arc<Mutex<Vec<Rec>>> = Arc::default();
        let log2 = log.clone();
        let app = axum::Router::new().fallback(move |method: axum::http::Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let h = h.clone();
            let log = log2.clone();
            async move {
                let query = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()).into_owned().collect();
                let rec = Rec {
                    method: method.to_string(),
                    path: uri.path().to_string(),
                    query,
                    auth: headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string(),
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                };
                let (status, v) = h(&rec);
                log.lock().unwrap().push(rec);
                let mut resp = axum::response::Response::new(axum::body::Body::from(if v.is_null() { String::new() } else { v.to_string() }));
                *resp.status_mut() = axum::http::StatusCode::from_u16(status).unwrap();
                resp.headers_mut().insert("content-type", HeaderValue::from_static("application/json"));
                resp
            }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://127.0.0.1:{port}"), log)
    }

    fn loaded(base: &str, extra: Value) -> Loaded {
        let mut cfg = json!({"base_url": base, "email": "bot@acme.test", "project_keys": "PROJ, OPS"});
        for (k, v) in extra.as_object().unwrap() {
            cfg[k] = v.clone();
        }
        Loaded {
            row: ConnectorRow { id: Uuid::nil(), kind: "jira".into(), role: "ticket".into(), name: "jira".into(), config: cfg, service_identity: None },
            secrets: HashMap::from([("api_token".to_string(), "tok123".to_string())]),
        }
    }

    fn adf(text: &str) -> Value {
        json!({"type": "doc", "version": 1, "content": [{"type": "paragraph", "content": [{"type": "text", "text": text}]}]})
    }

    fn issue_json() -> Value {
        json!({
            "id": "10001", "key": "PROJ-1",
            "fields": {
                "summary": "Add login page",
                "description": {"type": "doc", "version": 1, "content": [
                    {"type": "heading", "attrs": {"level": 2}, "content": [{"type": "text", "text": "Goal"}]},
                    {"type": "paragraph", "content": [{"type": "text", "text": "Users sign in with "}, {"type": "text", "text": "SSO", "marks": [{"type": "strong"}]}, {"type": "text", "text": "."}]},
                    {"type": "bulletList", "content": [
                        {"type": "listItem", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "one"}]}]},
                        {"type": "listItem", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "two"}]}]}
                    ]}
                ]},
                "status": {"name": "To Do"},
                "assignee": {"accountId": "5b10a", "emailAddress": "dev@acme.test", "displayName": "Dev"},
                "labels": ["factory", "repo:acme/web"],
                "subtasks": [{"id": "10002", "key": "PROJ-2", "fields": {"summary": "Form", "status": {"name": "Done"}}}],
                "customfield_10050": {"value": "acme/api"}
            }
        })
    }

    #[tokio::test]
    async fn get_builds_ticket_with_auth_children_comments_and_repo() {
        let h: Handler = Arc::new(|r| match r.path.as_str() {
            "/rest/api/3/issue/PROJ-1" => (200, issue_json()),
            "/rest/api/3/search/jql" => (200, json!({"issues": [
                {"id": "10002", "key": "PROJ-2", "fields": {"summary": "Form", "status": {"name": "Done"}}},
                {"id": "10003", "key": "PROJ-3", "fields": {"summary": "Styles", "status": {"name": "To Do"}}}
            ], "isLast": true})),
            "/rest/api/3/issue/PROJ-1/comment" => (200, json!({"startAt": 0, "maxResults": 100, "total": 1, "comments": [
                {"id": "200", "author": {"accountId": "a1", "emailAddress": "pm@acme.test"}, "body": adf("Please hurry"), "created": "2026-01-02T10:00:00.000+0000"}
            ]})),
            _ => (404, json!({"errorMessages": ["nope"]})),
        });
        let (base, log) = mock(h).await;
        let j = Jira::from_loaded(&loaded(&base, json!({}))).unwrap();
        let t = j.get("PROJ-1").await.unwrap();
        assert_eq!(t.title, "Add login page");
        assert_eq!(t.description, "Goal\n\nUsers sign in with SSO.\n\n- one\n- two");
        assert_eq!(t.status, "To Do");
        assert_eq!(t.assignee_email.as_deref(), Some("dev@acme.test"));
        assert_eq!(t.labels, vec!["factory", "repo:acme/web"]);
        assert_eq!(t.repo.as_deref(), Some("acme/web"));
        assert_eq!(t.children.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), vec!["PROJ-2", "PROJ-3"]);
        assert_eq!(t.comments.len(), 1);
        assert_eq!(t.comments[0].body, "Please hurry");
        assert_eq!(t.comments[0].author, "pm@acme.test");
        assert_eq!(t.url, Some(format!("{base}/browse/PROJ-1")));
        {
            let log = log.lock().unwrap();
            let expect = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("bot@acme.test:tok123"));
            assert!(log.iter().all(|r| r.auth == expect));
            let s = log.iter().find(|r| r.path == "/rest/api/3/search/jql").unwrap();
            assert_eq!(s.query["jql"], "parent = \"PROJ-1\"");
            let c = log.iter().find(|r| r.path.ends_with("/comment")).unwrap();
            assert_eq!(c.query["expand"], "properties");
        }

        // Repo from a configured custom field wins over the label.
        let j = Jira::from_loaded(&loaded(&base, json!({"repo_field": "customfield_10050"}))).unwrap();
        assert_eq!(j.get("PROJ-1").await.unwrap().repo.as_deref(), Some("acme/api"));
    }

    #[tokio::test]
    async fn search_uses_jql_endpoint_and_follows_next_page_token() {
        let h: Handler = Arc::new(|r| {
            if r.path != "/rest/api/3/search/jql" {
                return (404, Value::Null);
            }
            if r.query.get("nextPageToken").map(String::as_str) == Some("p2") {
                (200, json!({"issues": [{"key": "PROJ-9", "fields": {"summary": "B", "status": {"name": "Done"}}}], "isLast": true}))
            } else {
                (200, json!({"issues": [{"key": "PROJ-8", "fields": {"summary": "A login", "status": {"name": "To Do"}}}], "nextPageToken": "p2", "isLast": false}))
            }
        });
        let (base, log) = mock(h).await;
        let j = Jira::from_loaded(&loaded(&base, json!({}))).unwrap();
        let r = j.search("login").await.unwrap();
        assert_eq!(r.iter().map(|t| t.key.as_str()).collect::<Vec<_>>(), vec!["PROJ-8", "PROJ-9"]);
        assert_eq!(r[0].status, "To Do");
        {
            let log = log.lock().unwrap();
            assert_eq!(log.len(), 2);
            assert_eq!(log[0].method, "GET");
            assert_eq!(log[0].query["jql"], "project in (\"PROJ\", \"OPS\") AND text ~ \"login\" ORDER BY updated DESC");
            assert_eq!(log[0].query["fields"], "summary,status");
            assert_eq!(log[1].query["nextPageToken"], "p2");
        }
        j.search("PROJ-8").await.unwrap();
        j.search("status = Done ORDER BY created").await.unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log[2].query["jql"], "key = \"PROJ-8\"");
        assert_eq!(log[4].query["jql"], "status = Done ORDER BY created");
    }

    #[tokio::test]
    async fn transition_finds_target_status_and_posts_it() {
        let h: Handler = Arc::new(|r| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/rest/api/3/issue/PROJ-1") => (200, json!({"id": "10001", "key": "PROJ-1", "fields": {"status": {"name": "To Do"}}})),
            ("GET", "/rest/api/3/issue/PROJ-1/transitions") => (200, json!({"transitions": [
                {"id": "11", "name": "Start work", "to": {"id": "3", "name": "In Progress"}},
                {"id": "31", "name": "Finish", "to": {"id": "5", "name": "Done"}}
            ]})),
            ("POST", "/rest/api/3/issue/PROJ-1/transitions") => (204, Value::Null),
            _ => (404, Value::Null),
        });
        let (base, log) = mock(h).await;
        let j = Jira::from_loaded(&loaded(&base, json!({}))).unwrap();
        j.transition("PROJ-1", "in progress").await.unwrap();
        {
            let log = log.lock().unwrap();
            let p = log.iter().find(|r| r.method == "POST").unwrap();
            assert_eq!(p.body, json!({"transition": {"id": "11"}}));
        }
        // Already in the target status: no POST.
        j.transition("PROJ-1", "To Do").await.unwrap();
        assert_eq!(log.lock().unwrap().iter().filter(|r| r.method == "POST").count(), 1);
        let e = j.transition("PROJ-1", "Blocked").await.unwrap_err().to_string();
        assert!(e.contains("In Progress, Done"), "{e}");
    }

    #[tokio::test]
    async fn comment_marker_round_trip() {
        let store: Arc<Mutex<Vec<Value>>> = Arc::default();
        let s2 = store.clone();
        let h: Handler = Arc::new(move |r| match (r.method.as_str(), r.path.as_str()) {
            ("POST", "/rest/api/3/issue/PROJ-1/comment") => {
                let mut c = r.body.clone();
                let id = (s2.lock().unwrap().len() + 300).to_string();
                c["id"] = json!(id);
                c["author"] = json!({"accountId": "bot", "emailAddress": "bot@acme.test"});
                c["created"] = json!("2026-01-03T10:00:00.000+0000");
                s2.lock().unwrap().push(c.clone());
                let mut resp = c;
                resp.as_object_mut().unwrap().remove("properties");
                (201, resp)
            }
            ("GET", "/rest/api/3/issue/PROJ-1/comment") => {
                let all = s2.lock().unwrap().clone();
                let mut list = vec![json!({"id": "1", "author": {"accountId": "u"}, "body": adf("unrelated")})];
                list.extend(all);
                (200, json!({"startAt": 0, "maxResults": 100, "total": list.len(), "comments": list}))
            }
            _ => (404, Value::Null),
        });
        let (base, log) = mock(h).await;
        let j = Jira::from_loaded(&loaded(&base, json!({}))).unwrap();
        assert!(j.find_comment("PROJ-1", "factory:run-1:plan").await.unwrap().is_none());
        let c = j.comment("PROJ-1", "Plan:\nstep one\n\nDone soon.", "factory:run-1:plan").await.unwrap();
        assert_eq!(c.id, "300");
        assert!(c.body.contains("factory:run-1:plan"));
        assert!(c.body.starts_with("Plan:\nstep one\n\nDone soon."));
        {
            let log = log.lock().unwrap();
            let p = log.iter().find(|r| r.method == "POST").unwrap();
            assert_eq!(p.body["body"]["type"], "doc");
            assert_eq!(p.body["body"]["content"].as_array().unwrap().len(), 3);
            assert_eq!(p.body["body"]["content"][0]["content"][1]["type"], "hardBreak");
            assert_eq!(p.body["body"]["content"][2]["content"][0]["marks"][0]["type"], "code");
            assert_eq!(p.body["properties"][0], json!({"key": "factory.marker", "value": {"marker": "factory:run-1:plan"}}));
        }
        let found = j.find_comment("PROJ-1", "factory:run-1:plan").await.unwrap().unwrap();
        assert_eq!(found.id, "300");
        // Found via the property even when the visible marker paragraph was edited away.
        store.lock().unwrap()[0]["body"] = adf("edited");
        assert_eq!(j.find_comment("PROJ-1", "factory:run-1:plan").await.unwrap().unwrap().id, "300");
        assert!(j.find_comment("PROJ-1", "factory:run-2:plan").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn poll_produces_events_from_changelog_and_comments() {
        let h: Handler = Arc::new(|r| match r.path.as_str() {
            "/rest/api/3/search/jql" => (200, json!({"issues": [
                {"id": "10001", "key": "PROJ-1", "fields": {"summary": "A", "status": {"name": "In Progress"}, "labels": ["factory"],
                    "created": "2025-12-01T00:00:00.000+0000", "updated": "2026-01-05T10:05:00.000+0000"}},
                {"id": "10007", "key": "PROJ-7", "fields": {"summary": "New", "status": {"name": "To Do"}, "labels": ["factory"],
                    "created": "2026-01-05T09:04:00.000-0100", "updated": "2026-01-05T09:04:00.000-0100", "creator": {"accountId": "c1"}}}
            ], "isLast": true})),
            "/rest/api/3/issue/10001/changelog" => (200, json!({"startAt": 0, "maxResults": 100, "total": 2, "isLast": true, "values": [
                {"id": "500", "author": {"accountId": "old"}, "created": "2026-01-01T00:00:00.000+0000",
                    "items": [{"field": "status", "fieldId": "status", "fromString": "Backlog", "toString": "To Do"}]},
                {"id": "501", "author": {"accountId": "a1", "emailAddress": "pm@acme.test"}, "created": "2026-01-05T10:01:00.000+0000",
                    "items": [
                        {"field": "labels", "fieldId": "labels", "fromString": "x", "toString": "factory x"},
                        {"field": "status", "fieldId": "status", "fromString": "To Do", "toString": "In Progress"}
                    ]}
            ]})),
            "/rest/api/3/issue/10007/changelog" => (200, json!({"startAt": 0, "maxResults": 100, "total": 0, "isLast": true, "values": []})),
            "/rest/api/3/issue/PROJ-1/comment" => (200, json!({"total": 2, "comments": [
                {"id": "10", "author": {"accountId": "a"}, "body": adf("old"), "created": "2026-01-01T00:00:00.000+0000"},
                {"id": "11", "author": {"accountId": "a2"}, "body": adf("new one"), "created": "2026-01-05T10:02:00.000+0000"}
            ]})),
            "/rest/api/3/issue/PROJ-7/comment" => (200, json!({"total": 0, "comments": []})),
            _ => (404, Value::Null),
        });
        let (base, log) = mock(h).await;
        let j = Jira::from_loaded(&loaded(&base, json!({}))).unwrap();
        let (none, c0) = j.poll(None).await.unwrap();
        assert!(none.is_empty() && c0.is_some());
        let (evs, next) = j.poll(Some("2026-01-05T10:00:00+00:00")).await.unwrap();
        let ids: Vec<&str> = evs.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec![
            "jira-10001-cl-501-label-factory",
            "jira-10001-cl-501-status",
            "jira-10001-comment-11",
            "jira-10007-created",
            "jira-10007-created-label-factory",
        ]);
        assert_eq!(evs[0].kind, "labeled");
        assert_eq!(evs[0].author.as_deref(), Some("pm@acme.test"));
        assert_eq!(evs[1].kind, "transitioned");
        assert_eq!(evs[1].status.as_deref(), Some("In Progress"));
        assert_eq!(evs[2].body.as_deref(), Some("new one"));
        assert_eq!(next.as_deref(), Some("2026-01-05T10:05:00+00:00"));
        let log = log.lock().unwrap();
        let s = log.iter().find(|r| r.path == "/rest/api/3/search/jql").unwrap();
        let jql = &s.query["jql"];
        assert!(jql.starts_with("project in (\"PROJ\", \"OPS\") AND updated >= \"-") && jql.ends_with("m\" ORDER BY updated ASC"), "{jql}");
    }

    #[test]
    fn adf_conversion() {
        let doc = json!({"type": "doc", "version": 1, "content": [
            {"type": "paragraph", "content": [
                {"type": "mention", "attrs": {"id": "1", "text": "@Ann"}}, {"type": "text", "text": " see"}, {"type": "hardBreak"},
                {"type": "inlineCard", "attrs": {"url": "https://x.test/1"}}
            ]},
            {"type": "orderedList", "attrs": {"order": 1}, "content": [
                {"type": "listItem", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "a"}]},
                    {"type": "bulletList", "content": [{"type": "listItem", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "a1"}]}]}]}]},
                {"type": "listItem", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "b"}]}]}
            ]},
            {"type": "codeBlock", "attrs": {"language": "rust"}, "content": [{"type": "text", "text": "fn main() {}"}]},
            {"type": "rule"},
            {"type": "table", "content": [{"type": "tableRow", "content": [
                {"type": "tableHeader", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "k"}]}]},
                {"type": "tableCell", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "v"}]}]}
            ]}]}
        ]});
        assert_eq!(adf_to_text(&doc), "@Ann see\nhttps://x.test/1\n\n1. a\n  - a1\n2. b\n\nfn main() {}\n\n---\n\nk | v");
        let back = text_to_adf("hello\nworld", Some("m-1"));
        assert_eq!(adf_to_text(&back), "hello\nworld\n\nm-1");
        assert_eq!(text_to_adf("", None)["content"][0]["type"], "paragraph");
    }

    #[test]
    fn signature_verification() {
        // Test vector from Atlassian's webhook documentation.
        let secret = "It's a Secret to Everybody";
        let mut h = HeaderMap::new();
        h.insert("x-hub-signature", HeaderValue::from_static("sha256=a4771c39fbe90f317c7824e83ddef3caae9cb3d976c214ace1f2937e133263c9"));
        assert!(verify_signature(secret, &h, b"Hello World!"));
        assert!(!verify_signature(secret, &h, b"Hello World?"));
        assert!(!verify_signature("other", &h, b"Hello World!"));
        h.insert("x-hub-signature", HeaderValue::from_static("sha1=a4771c39fbe90f317c7824e83ddef3caae9cb3d976c214ace1f2937e133263c9"));
        assert!(!verify_signature(secret, &h, b"Hello World!"));
        h.insert("x-hub-signature", HeaderValue::from_static("sha256=zz"));
        assert!(!verify_signature(secret, &h, b"Hello World!"));
        assert!(!verify_signature(secret, &HeaderMap::new(), b"Hello World!"));
    }

    #[test]
    fn webhook_parsing() {
        let h = HeaderMap::new();
        let updated = json!({
            "timestamp": 1767607200000i64,
            "webhookEvent": "jira:issue_updated",
            "issue_event_type_name": "issue_generic",
            "user": {"self": "https://acme.atlassian.net/rest/api/2/user?accountId=a1", "accountId": "a1", "displayName": "PM", "active": true},
            "issue": {"id": "10001", "self": "https://acme.atlassian.net/rest/api/2/10001", "key": "PROJ-1",
                "fields": {"summary": "A", "labels": ["factory", "x"], "status": {"name": "Won't Do"}}},
            "changelog": {"id": "10500", "items": [
                {"field": "labels", "fieldtype": "jira", "fieldId": "labels", "from": null, "fromString": "x", "to": null, "toString": "factory x"},
                {"field": "status", "fieldtype": "jira", "fieldId": "status", "from": "10000", "fromString": "To Do", "to": "10002", "toString": "Won't Do"},
                {"field": "summary", "fieldtype": "jira", "fromString": "a", "toString": "A"}
            ]}
        });
        let evs = parse_webhook(&h, updated.to_string().as_bytes()).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].id, "jira-10001-cl-10500-label-factory");
        assert_eq!(evs[0].kind, "labeled");
        assert_eq!(evs[0].label.as_deref(), Some("factory"));
        assert_eq!(evs[0].author.as_deref(), Some("a1"));
        assert_eq!(evs[1].id, "jira-10001-cl-10500-status");
        assert_eq!(evs[1].kind, "cancelled");
        assert_eq!(evs[1].status.as_deref(), Some("Won't Do"));

        let created = json!({
            "timestamp": 1767607200000i64, "webhookEvent": "jira:issue_created", "issue_event_type_name": "issue_created",
            "user": {"accountId": "c1", "emailAddress": "c@acme.test"},
            "issue": {"id": "10007", "key": "PROJ-7", "fields": {"summary": "New", "labels": ["factory"], "status": {"name": "To Do"}}}
        });
        let evs = parse_webhook(&h, created.to_string().as_bytes()).unwrap();
        assert_eq!(evs.iter().map(|e| (e.id.as_str(), e.kind.as_str())).collect::<Vec<_>>(), vec![
            ("jira-10007-created", "created"),
            ("jira-10007-created-label-factory", "labeled"),
        ]);
        assert_eq!(evs[0].author.as_deref(), Some("c@acme.test"));

        // Comment payloads use the v2 shape (string body); ADF bodies are handled too.
        let comment = json!({
            "timestamp": 1767607200000i64, "webhookEvent": "comment_created",
            "comment": {"id": "10200", "self": "https://acme.atlassian.net/rest/api/2/issue/10001/comment/10200",
                "author": {"accountId": "a2", "displayName": "Dev"}, "body": "Looks good, *ship it*",
                "created": "2026-01-05T10:02:00.000+0000", "updated": "2026-01-05T10:02:00.000+0000"},
            "issue": {"id": "10001", "key": "PROJ-1", "fields": {"summary": "A"}}
        });
        let evs = parse_webhook(&h, comment.to_string().as_bytes()).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].id, "jira-10001-comment-10200");
        assert_eq!(evs[0].kind, "commented");
        assert_eq!(evs[0].body.as_deref(), Some("Looks good, *ship it*"));
        let mut c2 = comment.clone();
        c2["comment"]["body"] = adf("adf body");
        assert_eq!(parse_webhook(&h, c2.to_string().as_bytes()).unwrap()[0].body.as_deref(), Some("adf body"));

        // An issue_commented update carries the comment: same id as comment_created.
        let mut uc = updated.clone();
        uc["changelog"]["items"] = json!([]);
        uc["comment"] = comment["comment"].clone();
        let evs = parse_webhook(&h, uc.to_string().as_bytes()).unwrap();
        assert_eq!(evs.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["jira-10001-comment-10200"]);

        let deleted = json!({"webhookEvent": "jira:issue_deleted", "user": {"accountId": "a1"}, "issue": {"id": "10001", "key": "PROJ-1", "fields": {}}});
        let evs = parse_webhook(&h, deleted.to_string().as_bytes()).unwrap();
        assert_eq!(evs[0].kind, "cancelled");
        assert!(parse_webhook(&h, br#"{"webhookEvent":"worklog_created"}"#).unwrap().is_empty());
        assert!(parse_webhook(&h, b"not json").is_err());
    }

    fn contract() -> Option<(Jira, String)> {
        let get = |k: &str| std::env::var(format!("FACTORY_CONTRACT_JIRA_{k}")).ok().filter(|s| !s.is_empty());
        let (Some(base), Some(email), Some(token), Some(issue)) = (get("BASE_URL"), get("EMAIL"), get("TOKEN"), get("ISSUE")) else {
            eprintln!("skipping: set FACTORY_CONTRACT_JIRA_BASE_URL, _EMAIL, _TOKEN and _ISSUE");
            return None;
        };
        let project = issue.split('-').next().unwrap_or_default().to_string();
        let l = Loaded {
            row: ConnectorRow {
                id: Uuid::nil(),
                kind: "jira".into(),
                role: "ticket".into(),
                name: "contract".into(),
                config: json!({"base_url": base, "email": email, "project_keys": project}),
                service_identity: None,
            },
            secrets: HashMap::from([("api_token".to_string(), token)]),
        };
        Some((Jira::from_loaded(&l).unwrap(), issue))
    }

    #[tokio::test]
    #[ignore]
    async fn contract_read() {
        let Some((j, issue)) = contract() else { return };
        println!("{}", j.test_connection().await.unwrap());
        let t = j.get(&issue).await.unwrap();
        assert_eq!(t.key, issue);
        assert!(!t.status.is_empty());
        let found = j.search(&issue).await.unwrap();
        assert!(found.iter().any(|r| r.key == issue));
        let cursor = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let (_evs, next) = j.poll(Some(&cursor)).await.unwrap();
        assert!(next.is_some());
        assert!(j.find_comment(&issue, &format!("factory:contract:{}", Uuid::now_v7())).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn contract_comment_round_trip() {
        let Some((j, issue)) = contract() else { return };
        if std::env::var("FACTORY_CONTRACT_JIRA_WRITE").ok().as_deref() != Some("1") {
            eprintln!("skipping: set FACTORY_CONTRACT_JIRA_WRITE=1 to post a comment on {issue}");
            return;
        }
        let marker = format!("factory:contract:{}", Uuid::now_v7());
        let c = j.comment(&issue, "Contract test comment.\nSecond line.", &marker).await.unwrap();
        let found = j.find_comment(&issue, &marker).await.unwrap().expect("comment found by marker");
        assert_eq!(found.id, c.id);
    }
}
