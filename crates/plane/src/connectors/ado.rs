//! Azure DevOps Services repo host (REST API 7.1, Azure Repos Git + Pipelines).
//!
//! Config: `org_url` (e.g. `https://dev.azure.com/org`), `project`, optional
//! `search_url` (Code Search base; derived as `https://almsearch.dev.azure.com/org`
//! for dev.azure.com). Secret: `pat` (Basic auth with an empty user name).
//! A repository's full name is its name within the project.
//!
//! PR threads with a file context are `PrThread`s (id = thread id; status
//! active/pending → open, fixed/closed/byDesign → resolved, wontFix →
//! wont_fix). Threads without a file context are general discussion and are
//! returned as `PrDetail.comments` (comment id = `<thread>/<comment>`).
//! System threads (votes, pushes, policy) are not comments; vote threads feed
//! `poll`'s review events.
//!
//! Service hooks are authenticated with Basic auth configured on the hook;
//! `verify_signature` compares the Basic password with the shared secret.
//!
//! Contract tests (ignored by default) read:
//! - `FACTORY_CONTRACT_ADO_ORG_URL`  e.g. `https://dev.azure.com/org`
//! - `FACTORY_CONTRACT_ADO_PROJECT`  project name
//! - `FACTORY_CONTRACT_ADO_PAT`      PAT with Code (read) and Build (read)
//! - `FACTORY_CONTRACT_ADO_REPO`     repository name in the project

use super::*;
use axum::http::HeaderMap;
use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::{Method, StatusCode};
use std::time::Duration;

const API: &str = "api-version=7.1";
/// Azure DevOps rejects PR descriptions longer than this.
const MAX_DESCRIPTION: usize = 4000;

pub struct AzureDevOps {
    http: reqwest::Client,
    org: String,
    project: String,
    search_base: String,
    pat: String,
}

fn s(v: &Value, p: &str) -> String {
    v.pointer(p).and_then(Value::as_str).unwrap_or("").to_string()
}
fn os(v: &Value, p: &str) -> Option<String> {
    v.pointer(p).and_then(Value::as_str).map(String::from).filter(|x| !x.is_empty())
}
fn oi(v: &Value, p: &str) -> Option<i64> {
    v.pointer(p).and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
}
fn ts(v: &Value, p: &str) -> Option<DateTime<Utc>> {
    v.pointer(p)
        .and_then(Value::as_str)
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|d| d.with_timezone(&Utc))
}
fn seg(x: &str) -> String {
    url::form_urlencoded::byte_serialize(x.as_bytes()).collect::<String>().replace('+', "%20")
}
fn q(x: &str) -> String {
    url::form_urlencoded::byte_serialize(x.as_bytes()).collect()
}
fn snippet(b: &str) -> String {
    b.chars().take(400).collect()
}
fn strip_heads(r: &str) -> String {
    r.strip_prefix("refs/heads/").unwrap_or(r).to_string()
}
fn is_sha(r: &str) -> bool {
    r.len() == 40 && r.chars().all(|c| c.is_ascii_hexdigit())
}
fn items(v: &Value) -> Vec<Value> {
    v.get("value").and_then(Value::as_array).cloned().unwrap_or_default()
}
fn who(v: &Value, p: &str) -> String {
    os(v, &format!("{p}/uniqueName")).or_else(|| os(v, &format!("{p}/displayName"))).unwrap_or_default()
}
fn ev(id: String, repo: &str, pr: Option<i64>, kind: &str) -> RepoEvent {
    RepoEvent { id, repo: repo.into(), pr_number: pr, kind: kind.into(), author: None, body: None, state: None, thread_id: None, branch: None }
}

pub fn thread_status(st: &str) -> Option<String> {
    match st {
        "fixed" | "closed" | "byDesign" => Some("resolved".into()),
        "wontFix" => Some("wont_fix".into()),
        _ => None, // active, pending, unknown
    }
}

fn prop(t: &Value, k: &str) -> Option<String> {
    t.pointer(&format!("/properties/{k}/$value")).and_then(|v| v.as_str().map(String::from).or_else(|| v.as_i64().map(|i| i.to_string())))
}

fn is_system(t: &Value) -> bool {
    prop(t, "CodeReviewThreadType").is_some() || s(t, "/comments/0/commentType") == "system"
}

fn vote_state(v: i64) -> Option<&'static str> {
    match v {
        v if v > 0 => Some("approved"),
        v if v < 0 => Some("changes_requested"),
        _ => None,
    }
}

fn live_comments(t: &Value) -> Vec<Value> {
    t.get("comments")
        .and_then(Value::as_array)
        .map(|c| c.iter().filter(|c| c.get("isDeleted").and_then(Value::as_bool) != Some(true) && s(c, "/commentType") != "system").cloned().collect())
        .unwrap_or_default()
}

fn comment_from(tid: i64, c: &Value) -> Comment {
    Comment {
        id: format!("{tid}/{}", oi(c, "/id").unwrap_or(0)),
        author: who(c, "/author"),
        body: s(c, "/content"),
        created_at: os(c, "/publishedDate"),
    }
}

/// Split PR threads into file threads and general comments.
pub fn split_threads(threads: &[Value]) -> (Vec<PrThread>, Vec<Comment>) {
    let mut out = vec![];
    let mut general = vec![];
    for t in threads {
        if t.get("isDeleted").and_then(Value::as_bool) == Some(true) || is_system(t) {
            continue;
        }
        let tid = oi(t, "/id").unwrap_or(0);
        let cs: Vec<Comment> = live_comments(t).iter().map(|c| comment_from(tid, c)).collect();
        if cs.is_empty() {
            continue;
        }
        match os(t, "/threadContext/filePath") {
            Some(path) => out.push(PrThread {
                id: tid.to_string(),
                path: Some(path.trim_start_matches('/').to_string()),
                line: oi(t, "/threadContext/rightFileStart/line").or_else(|| oi(t, "/threadContext/leftFileStart/line")),
                status: thread_status(&s(t, "/status")),
                comments: cs,
            }),
            None => general.extend(cs),
        }
    }
    (out, general)
}

/// Keep the start and the end (where markers live) of an over-long description.
fn fit_description(body: &str) -> String {
    let n = body.chars().count();
    if n <= MAX_DESCRIPTION {
        return body.to_string();
    }
    let tail: String = body.chars().skip(n - 300).collect();
    let head: String = body.chars().take(MAX_DESCRIPTION - 320).collect();
    format!("{head}\n\n…\n\n{tail}")
}

fn ci_state(build_status: &str, result: &str) -> (String, Option<String>) {
    let status = match build_status {
        "completed" => "completed",
        "inProgress" | "cancelling" => "in_progress",
        _ => "queued",
    };
    let conclusion = if status == "completed" {
        Some(match result {
            "succeeded" => "success",
            "canceled" => "cancelled",
            "" | "none" => "neutral",
            _ => "failure", // failed, partiallySucceeded
        }.to_string())
    } else {
        None
    };
    (status.into(), conclusion)
}

fn status_state(st: &str) -> (String, Option<String>) {
    match st {
        "succeeded" => ("completed".into(), Some("success".into())),
        "failed" | "error" => ("completed".into(), Some("failure".into())),
        "notApplicable" => ("completed".into(), Some("neutral".into())),
        _ => ("in_progress".into(), None), // pending, notSet
    }
}

fn status_name(x: &Value) -> String {
    match (os(x, "/context/genre"), os(x, "/context/name")) {
        (Some(g), Some(n)) => format!("{g}/{n}"),
        (None, Some(n)) => n,
        (g, None) => g.unwrap_or_default(),
    }
}

/// Latest status per context (statuses are returned newest first, but sort by id to be sure).
fn latest_statuses(v: Vec<Value>) -> Vec<Value> {
    let mut v = v;
    v.sort_by_key(|x| -oi(x, "/id").unwrap_or(0));
    let mut seen = std::collections::HashSet::new();
    v.into_iter().filter(|x| seen.insert(status_name(x))).collect()
}

fn latest_builds(v: Vec<Value>) -> Vec<Value> {
    let mut v = v;
    v.sort_by_key(|x| -oi(x, "/id").unwrap_or(0));
    let mut seen = std::collections::HashSet::new();
    v.into_iter().filter(|x| seen.insert(oi(x, "/definition/id").unwrap_or(0))).collect()
}

fn build_check(b: &Value) -> CheckRun {
    let (status, conclusion) = ci_state(&s(b, "/status"), &s(b, "/result"));
    CheckRun {
        id: format!("build:{}", oi(b, "/id").unwrap_or(0)),
        name: os(b, "/definition/name").unwrap_or_else(|| s(b, "/buildNumber")),
        status,
        conclusion,
        url: os(b, "/_links/web/href"),
    }
}

impl AzureDevOps {
    pub fn from_loaded(l: &Loaded) -> Result<Self> {
        let org = l.cfg("org_url").ok_or_else(|| anyhow!("azure_devops connector needs config.org_url"))?;
        let project = l.cfg("project").ok_or_else(|| anyhow!("azure_devops connector needs config.project"))?;
        Self::new(&org, &project, l.cfg("search_url"), &l.secret("pat")?)
    }

    pub fn new(org_url: &str, project: &str, search_url: Option<String>, pat: &str) -> Result<Self> {
        let org = org_url.trim_end_matches('/').to_string();
        let u = url::Url::parse(&org).map_err(|e| anyhow!("bad Azure DevOps org_url {org}: {e}"))?;
        let search_base = match search_url {
            Some(s) => s.trim_end_matches('/').to_string(),
            None if u.host_str() == Some("dev.azure.com") => org.replacen("://dev.azure.com", "://almsearch.dev.azure.com", 1),
            None => org.clone(),
        };
        // A redirect means a sign-in page: never follow it.
        let http = reqwest::Client::builder()
            .user_agent("ai-factory")
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(AzureDevOps { http, org, project: project.to_string(), search_base, pat: pat.to_string() })
    }

    fn auth(&self) -> String {
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!(":{}", self.pat)))
    }

    /// `{org}/{project}/_apis/{path}?{query}&api-version=7.1`
    fn url(&self, path: &str, query: &str) -> String {
        let sep = if query.is_empty() { String::new() } else { format!("{query}&") };
        format!("{}/{}/_apis/{path}?{sep}{API}", self.org, seg(&self.project))
    }

    fn repo_url(&self, repo: &str, rest: &str, query: &str) -> String {
        self.url(&format!("git/repositories/{}{rest}", seg(repo)), query)
    }

    fn rb(&self, m: Method, url: &str) -> reqwest::RequestBuilder {
        self.rb_accept(m, url, "application/json")
    }

    fn rb_accept(&self, m: Method, url: &str, accept: &str) -> reqwest::RequestBuilder {
        self.http.request(m, url).header("Authorization", self.auth()).header("Accept", accept)
    }

    async fn send(&self, rb: reqwest::RequestBuilder) -> Result<(StatusCode, String)> {
        let r = rb.send().await?;
        let st = r.status();
        let text = r.text().await?;
        if st.is_redirection() || st == StatusCode::NON_AUTHORITATIVE_INFORMATION {
            bail!("Azure DevOps answered {st} (a sign-in page): check the PAT and its scopes");
        }
        Ok((st, text))
    }

    async fn call(&self, m: Method, url: &str, body: Option<&Value>) -> Result<Value> {
        let mut rb = self.rb(m.clone(), url);
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let (st, text) = self.send(rb).await?;
        if !st.is_success() {
            bail!("Azure DevOps {m} {url} failed: {st} {}", snippet(&text));
        }
        Ok(if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text)? })
    }

    async fn get(&self, url: &str) -> Result<Value> {
        self.call(Method::GET, url, None).await
    }

    async fn get_opt(&self, url: &str) -> Result<Option<Value>> {
        let (st, text) = self.send(self.rb(Method::GET, url)).await?;
        if st == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !st.is_success() {
            bail!("Azure DevOps GET {url} failed: {st} {}", snippet(&text));
        }
        Ok(Some(serde_json::from_str(&text)?))
    }

    pub fn pr_from(&self, repo: &str, v: &Value) -> Pr {
        let n = oi(v, "/pullRequestId").unwrap_or(0);
        let repo_name = os(v, "/repository/name").unwrap_or_else(|| repo.to_string());
        let state = match s(v, "/status").as_str() {
            "completed" => "merged",
            "abandoned" => "closed",
            _ => "open",
        };
        Pr {
            number: n,
            url: format!("{}/{}/_git/{}/pullrequest/{n}", self.org, seg(&self.project), seg(&repo_name)),
            title: s(v, "/title"),
            body: s(v, "/description"),
            state: state.into(),
            head: strip_heads(&s(v, "/sourceRefName")),
            base: strip_heads(&s(v, "/targetRefName")),
            author: who(v, "/createdBy"),
            head_sha: os(v, "/lastMergeSourceCommit/commitId"),
        }
    }

    async fn pr_list(&self, repo: &str, criteria: &str, limit: usize) -> Result<Vec<Value>> {
        let mut out = vec![];
        let mut skip = 0;
        loop {
            let v = self.get(&self.repo_url(repo, "/pullrequests", &format!("{criteria}&$top=100&$skip={skip}"))).await?;
            let page = items(&v);
            let n = page.len();
            out.extend(page);
            skip += n;
            if n < 100 || out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    async fn threads(&self, repo: &str, n: i64) -> Result<Vec<Value>> {
        Ok(items(&self.get(&self.repo_url(repo, &format!("/pullRequests/{n}/threads"), "")).await?))
    }

    async fn repo_id(&self, repo: &str) -> Result<String> {
        let v = self.get(&self.repo_url(repo, "", "")).await?;
        os(&v, "/id").ok_or_else(|| anyhow!("Azure DevOps repository {repo} has no id"))
    }

    async fn builds(&self, repo_id: &str, branch: &str) -> Result<Vec<Value>> {
        let v = self
            .get(&self.url("build/builds", &format!("repositoryId={}&repositoryType=TfsGit&branchName={}&queryOrder=queueTimeDescending&$top=50", q(repo_id), q(branch))))
            .await?;
        Ok(items(&v))
    }

    /// Protected branches from enabled, blocking branch policies with an exact ref scope.
    async fn protected(&self, repo_id: &str) -> Result<Vec<String>> {
        let v = self.get(&self.url("policy/configurations", &format!("repositoryId={}", q(repo_id)))).await?;
        let mut out = vec![];
        for c in items(&v) {
            if c.get("isEnabled").and_then(Value::as_bool) != Some(true) || c.get("isBlocking").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            for sc in c.pointer("/settings/scope").and_then(Value::as_array).cloned().unwrap_or_default() {
                let rid = os(&sc, "/repositoryId");
                if rid.as_deref().map(|r| r.eq_ignore_ascii_case(repo_id)).unwrap_or(true) && s(&sc, "/matchKind").eq_ignore_ascii_case("exact") {
                    if let Some(b) = os(&sc, "/refName").filter(|r| r.starts_with("refs/heads/")) {
                        let b = strip_heads(&b);
                        if !out.contains(&b) {
                            out.push(b);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn version_query(git_ref: &str) -> String {
        let (v, t) = if is_sha(git_ref) { (git_ref, "commit") } else { (git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref), "branch") };
        format!("versionDescriptor.version={}&versionDescriptor.versionType={t}", q(v))
    }

    async fn log_text(&self, url: &str) -> Result<String> {
        let (st, text) = self.send(self.rb_accept(Method::GET, url, "text/plain")).await?;
        if !st.is_success() {
            bail!("Azure DevOps GET {url} failed: {st} {}", snippet(&text));
        }
        let lines: Vec<&str> = text.lines().collect();
        Ok(lines[lines.len().saturating_sub(400)..].join("\n"))
    }
}

#[async_trait]
impl RepoHost for AzureDevOps {
    async fn test_connection(&self) -> Result<String> {
        let v = self.get(&format!("{}/_apis/projects/{}?{API}", self.org, seg(&self.project))).await?;
        Ok(format!("Azure DevOps project {} at {}", s(&v, "/name"), self.org))
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>> {
        let v = self.get(&self.url("git/repositories", "")).await?;
        let mut out: Vec<RepoInfo> = items(&v)
            .iter()
            .filter(|r| r.get("isDisabled").and_then(Value::as_bool) != Some(true))
            .map(|r| {
                let d = os(r, "/defaultBranch").map(|b| strip_heads(&b)).unwrap_or_else(|| "main".into());
                RepoInfo { full_name: s(r, "/name"), protected_branches: vec![d.clone()], default_branch: d }
            })
            .collect();
        out.sort_by(|a, b| a.full_name.cmp(&b.full_name));
        Ok(out)
    }

    async fn repo_info(&self, repo: &str) -> Result<RepoInfo> {
        let v = self.get(&self.repo_url(repo, "", "")).await?;
        let d = os(&v, "/defaultBranch").map(|b| strip_heads(&b)).unwrap_or_else(|| "main".into());
        let mut protected = vec![d.clone()];
        match self.protected(&s(&v, "/id")).await {
            Ok(bs) => protected.extend(bs.into_iter().filter(|b| *b != d)),
            Err(e) => tracing::warn!("reading branch policies of {repo}: {e:#}"),
        }
        Ok(RepoInfo { full_name: s(&v, "/name"), default_branch: d, protected_branches: protected })
    }

    async fn list_files(&self, repo: &str, git_ref: &str) -> Result<Vec<String>> {
        let v = self.get(&self.repo_url(repo, "/items", &format!("recursionLevel=Full&{}", Self::version_query(git_ref)))).await?;
        Ok(items(&v)
            .iter()
            .filter(|i| i.get("isFolder").and_then(Value::as_bool) != Some(true))
            .map(|i| s(i, "/path").trim_start_matches('/').to_string())
            .filter(|p| !p.is_empty())
            .collect())
    }

    async fn read_file(&self, repo: &str, git_ref: &str, path: &str) -> Result<Option<String>> {
        let p = format!("/{}", path.trim_start_matches('/'));
        let url = self.repo_url(repo, "/items", &format!("path={}&includeContent=true&{}", q(&p), Self::version_query(git_ref)));
        Ok(self.get_opt(&url).await?.map(|v| s(&v, "/content")))
    }

    /// Code Search (needs the Code Search extension; indexes default branches).
    async fn search(&self, repo: &str, query: &str) -> Result<Vec<Value>> {
        let url = format!("{}/{}/_apis/search/codesearchresults?{API}", self.search_base, seg(&self.project));
        let body = json!({"searchText": query, "$skip": 0, "$top": 100, "filters": {"Project": [self.project], "Repository": [repo]}});
        let v = self.call(Method::POST, &url, Some(&body)).await?;
        Ok(v.get("results")
            .and_then(Value::as_array)
            .map(|r| r.iter().map(|x| json!({"path": s(x, "/path").trim_start_matches('/'), "line": Value::Null, "text": s(x, "/fileName")})).collect())
            .unwrap_or_default())
    }

    /// `{org}/{project}/_git/{repo}` — the form of the repository's `remoteUrl`
    /// (computed here because this call cannot reach the API).
    fn git_remote(&self, repo: &str) -> Result<GitRemote> {
        if repo.is_empty() || repo.contains('/') {
            bail!("Azure DevOps repository must be a repository name in project {}, got {repo}", self.project);
        }
        Ok(GitRemote { url: format!("{}/{}/_git/{}", self.org, seg(&self.project), seg(repo)), auth_header: Some(self.auth()) })
    }

    async fn find_open_pr(&self, repo: &str, head: &str) -> Result<Option<Pr>> {
        let v = self
            .get(&self.repo_url(repo, "/pullrequests", &format!("searchCriteria.status=active&searchCriteria.sourceRefName={}", q(&format!("refs/heads/{head}")))))
            .await?;
        Ok(items(&v).first().map(|p| self.pr_from(repo, p)))
    }

    async fn open_pr(&self, repo: &str, head: &str, base: &str, title: &str, body: &str) -> Result<Pr> {
        let req = json!({
            "sourceRefName": format!("refs/heads/{head}"),
            "targetRefName": format!("refs/heads/{base}"),
            "title": title,
            "description": fit_description(body),
        });
        let v = self.call(Method::POST, &self.repo_url(repo, "/pullrequests", ""), Some(&req)).await?;
        Ok(self.pr_from(repo, &v))
    }

    async fn get_pr(&self, repo: &str, number: i64) -> Result<PrDetail> {
        let v = self.get(&self.repo_url(repo, &format!("/pullrequests/{number}"), "")).await?;
        let (threads, comments) = split_threads(&self.threads(repo, number).await?);
        let reviews = v
            .get("reviewers")
            .and_then(Value::as_array)
            .map(|rs| {
                rs.iter()
                    .filter_map(|r| {
                        Some(PrReview { id: s(r, "/id"), author: who(r, ""), state: vote_state(oi(r, "/vote").unwrap_or(0))?.into(), body: String::new() })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(PrDetail { pr: self.pr_from(repo, &v), threads, reviews, comments })
    }

    async fn list_prs(&self, repo: &str, state: &str) -> Result<Vec<Pr>> {
        let st = match state {
            "open" => "active",
            "merged" => "completed",
            "closed" => "abandoned",
            _ => "all",
        };
        Ok(self.pr_list(repo, &format!("searchCriteria.status={st}"), 1000).await?.iter().map(|p| self.pr_from(repo, p)).collect())
    }

    async fn comment(&self, repo: &str, number: i64, body: &str, path: Option<&str>, line: Option<i64>) -> Result<Value> {
        let mut req = json!({"comments": [{"parentCommentId": 0, "content": body, "commentType": 1}], "status": 1});
        if let Some(p) = path {
            let mut ctx = json!({"filePath": format!("/{}", p.trim_start_matches('/'))});
            if let Some(l) = line {
                ctx["rightFileStart"] = json!({"line": l, "offset": 1});
                ctx["rightFileEnd"] = json!({"line": l, "offset": 1});
            }
            req["threadContext"] = ctx;
        }
        let v = self.call(Method::POST, &self.repo_url(repo, &format!("/pullRequests/{number}/threads"), ""), Some(&req)).await?;
        let tid = oi(&v, "/id").unwrap_or(0);
        let cid = format!("{tid}/{}", oi(&v, "/comments/0/id").unwrap_or(1));
        Ok(if path.is_some() { json!({"thread_id": tid.to_string(), "comment_id": cid}) } else { json!({"comment_id": cid}) })
    }

    async fn reply(&self, repo: &str, number: i64, thread_id: &str, body: &str) -> Result<Value> {
        let tid: i64 = thread_id.parse().map_err(|_| anyhow!("bad Azure DevOps thread id {thread_id}"))?;
        let v = self
            .call(
                Method::POST,
                &self.repo_url(repo, &format!("/pullRequests/{number}/threads/{tid}/comments"), ""),
                Some(&json!({"parentCommentId": 1, "content": body, "commentType": 1})),
            )
            .await?;
        Ok(json!({"comment_id": format!("{tid}/{}", oi(&v, "/id").unwrap_or(0))}))
    }

    async fn resolve_thread(&self, repo: &str, number: i64, thread_id: &str, status: &str) -> Result<()> {
        let tid: i64 = thread_id.parse().map_err(|_| anyhow!("bad Azure DevOps thread id {thread_id}"))?;
        let st = match status {
            "resolved" => "fixed",
            "wont_fix" => "wontFix",
            s => bail!("thread status must be resolved or wont_fix, got {s}"),
        };
        self.call(Method::PATCH, &self.repo_url(repo, &format!("/pullRequests/{number}/threads/{tid}"), ""), Some(&json!({"status": st}))).await?;
        Ok(())
    }

    /// Complete the PR. Completion is asynchronous in Azure DevOps; this waits
    /// briefly and fails on a reported merge failure.
    async fn merge(&self, repo: &str, number: i64) -> Result<()> {
        let url = self.repo_url(repo, &format!("/pullrequests/{number}"), "");
        let v = self.get(&url).await?;
        match s(&v, "/status").as_str() {
            "completed" => return Ok(()),
            "abandoned" => bail!("PR {repo}!{number} is abandoned"),
            _ => {}
        }
        let sha = os(&v, "/lastMergeSourceCommit/commitId").ok_or_else(|| anyhow!("PR {repo}!{number} has no merge source commit yet"))?;
        let req = json!({
            "status": "completed",
            "lastMergeSourceCommit": {"commitId": sha},
            "completionOptions": {"deleteSourceBranch": false, "mergeStrategy": "noFastForward"},
        });
        let mut v = self.call(Method::PATCH, &url, Some(&req)).await?;
        for i in 0..10 {
            if s(&v, "/status") == "completed" {
                return Ok(());
            }
            match s(&v, "/mergeStatus").as_str() {
                "conflicts" | "failure" | "rejectedByPolicy" => {
                    bail!("Azure DevOps could not complete {repo}!{number}: {} {}", s(&v, "/mergeStatus"), s(&v, "/mergeFailureMessage"))
                }
                _ => {}
            }
            if i < 9 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                v = self.get(&url).await?;
            }
        }
        tracing::info!("completion of {repo}!{number} is still queued");
        Ok(())
    }

    /// Commit statuses, PR statuses and pipeline runs for a branch or commit.
    async fn checks(&self, repo: &str, git_ref: &str) -> Result<Vec<CheckRun>> {
        let (sha, branch) = if is_sha(git_ref) {
            (Some(git_ref.to_string()), None)
        } else {
            let b = strip_heads(git_ref);
            let refs = self.get(&self.repo_url(repo, "/refs", &format!("filter={}", q(&format!("heads/{b}"))))).await?;
            let sha = items(&refs).into_iter().find(|r| s(r, "/name") == format!("refs/heads/{b}")).and_then(|r| os(&r, "/objectId"));
            (sha, Some(b))
        };
        let mut out = vec![];
        if let Some(sha) = &sha {
            let v = self.get(&self.repo_url(repo, &format!("/commits/{sha}/statuses"), "latestOnly=true")).await?;
            for x in latest_statuses(items(&v)) {
                let (status, conclusion) = status_state(&s(&x, "/state"));
                out.push(CheckRun { id: format!("status:{sha}:{}", oi(&x, "/id").unwrap_or(0)), name: status_name(&x), status, conclusion, url: os(&x, "/targetUrl") });
            }
        }
        let active = self.pr_list(repo, "searchCriteria.status=active", 200).await?;
        let pr = active.iter().find(|p| {
            branch.as_deref().map(|b| s(p, "/sourceRefName") == format!("refs/heads/{b}")).unwrap_or(false)
                || (sha.is_some() && os(p, "/lastMergeSourceCommit/commitId") == sha)
        });
        let repo_id = self.repo_id(repo).await?;
        let mut builds = vec![];
        if let Some(p) = pr {
            let n = oi(p, "/pullRequestId").unwrap_or(0);
            let v = self.get(&self.repo_url(repo, &format!("/pullRequests/{n}/statuses"), "")).await?;
            for x in latest_statuses(items(&v)) {
                let (status, conclusion) = status_state(&s(&x, "/state"));
                out.push(CheckRun { id: format!("prstatus:{n}:{}", oi(&x, "/id").unwrap_or(0)), name: status_name(&x), status, conclusion, url: os(&x, "/targetUrl") });
            }
            builds.extend(self.builds(&repo_id, &format!("refs/pull/{n}/merge")).await?);
        }
        if let Some(b) = &branch {
            builds.extend(self.builds(&repo_id, &format!("refs/heads/{b}")).await?);
        }
        out.extend(latest_builds(builds).iter().map(build_check));
        Ok(out)
    }

    /// `build:<id>`: logs of the failed timeline steps (or the last log);
    /// statuses: their description and target URL.
    async fn check_logs(&self, repo: &str, check_id: &str) -> Result<String> {
        let parts: Vec<&str> = check_id.split(':').collect();
        match parts.as_slice() {
            ["build", id] => {
                let b = self.get(&self.url(&format!("build/builds/{id}"), "")).await?;
                let mut out = format!("{} #{}: {} {}\n", s(&b, "/definition/name"), s(&b, "/buildNumber"), s(&b, "/status"), s(&b, "/result"));
                let tl = self.get_opt(&self.url(&format!("build/builds/{id}/timeline"), "")).await?.unwrap_or(Value::Null);
                let recs = tl.get("records").and_then(Value::as_array).cloned().unwrap_or_default();
                let failed: Vec<&Value> = recs.iter().filter(|r| s(r, "/result") == "failed" && oi(r, "/log/id").is_some()).collect();
                for r in &recs {
                    for i in r.get("issues").and_then(Value::as_array).cloned().unwrap_or_default() {
                        out.push_str(&format!("{}: {}: {}\n", s(r, "/name"), s(&i, "/type"), s(&i, "/message")));
                    }
                }
                let log_ids: Vec<i64> = if failed.is_empty() {
                    let logs = self.get(&self.url(&format!("build/builds/{id}/logs"), "")).await?;
                    items(&logs).iter().filter_map(|l| oi(l, "/id")).max().into_iter().collect()
                } else {
                    failed.iter().filter_map(|r| oi(r, "/log/id")).collect()
                };
                for lid in log_ids.iter().take(5) {
                    out.push_str(&format!("--- log {lid} (tail) ---\n"));
                    out.push_str(&self.log_text(&self.url(&format!("build/builds/{id}/logs/{lid}"), "")).await?);
                    out.push('\n');
                }
                Ok(out)
            }
            ["status", sha, id] => {
                let v = self.get(&self.repo_url(repo, &format!("/commits/{sha}/statuses"), "")).await?;
                let x = items(&v).into_iter().find(|x| oi(x, "/id").map(|i| i.to_string()).as_deref() == Some(*id)).ok_or_else(|| anyhow!("status {check_id} not found"))?;
                Ok(format!("{}: {} {}\n{}", status_name(&x), s(&x, "/state"), s(&x, "/description"), s(&x, "/targetUrl")))
            }
            ["prstatus", n, id] => {
                let x = self.get(&self.repo_url(repo, &format!("/pullRequests/{n}/statuses/{id}"), "")).await?;
                Ok(format!("{}: {} {}\n{}", status_name(&x), s(&x, "/state"), s(&x, "/description"), s(&x, "/targetUrl")))
            }
            _ => bail!("unknown Azure DevOps check id {check_id}"),
        }
    }

    /// Events since `cursor` (RFC 3339). Without a cursor, starts from now.
    async fn poll(&self, repo: &str, cursor: Option<&str>) -> Result<(Vec<RepoEvent>, Option<String>)> {
        let Some(since) = cursor.and_then(|c| DateTime::parse_from_rfc3339(c).ok()).map(|d| d.with_timezone(&Utc)) else {
            return Ok((vec![], Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true))));
        };
        let since_s = since.to_rfc3339_opts(SecondsFormat::Secs, true);
        let mut max = since;
        let mut bump = |t: Option<DateTime<Utc>>| {
            if let Some(t) = t {
                if t > max {
                    max = t;
                }
            }
        };
        let after = |t: Option<DateTime<Utc>>| t.map(|t| t >= since).unwrap_or(false);
        let mut evs = vec![];

        for p in self.pr_list(repo, &format!("searchCriteria.status=all&searchCriteria.queryTimeRangeType=created&searchCriteria.minTime={}", q(&since_s)), 500).await? {
            bump(ts(&p, "/creationDate"));
            let n = oi(&p, "/pullRequestId");
            let mut e = ev(format!("opened-{repo}-{}", n.unwrap_or(0)), repo, n, "pr_opened");
            e.author = Some(who(&p, "/createdBy"));
            e.branch = Some(strip_heads(&s(&p, "/sourceRefName")));
            evs.push(e);
        }
        for p in self.pr_list(repo, &format!("searchCriteria.status=all&searchCriteria.queryTimeRangeType=closed&searchCriteria.minTime={}", q(&since_s)), 500).await? {
            bump(ts(&p, "/closedDate"));
            evs.extend(closed_event(repo, &p, None));
        }

        let repo_id = self.repo_id(repo).await?;
        for p in self.pr_list(repo, "searchCriteria.status=active", 50).await?.iter().take(50) {
            let n = oi(p, "/pullRequestId").unwrap_or(0);
            let branch = strip_heads(&s(p, "/sourceRefName"));
            for t in self.threads(repo, n).await? {
                let tid = oi(&t, "/id").unwrap_or(0);
                if prop(&t, "CodeReviewThreadType").as_deref() == Some("VoteUpdate") {
                    if after(ts(&t, "/publishedDate")) {
                        bump(ts(&t, "/publishedDate"));
                        let vote = prop(&t, "CodeReviewVoteResult").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
                        if let Some(st) = vote_state(vote) {
                            let mut e = ev(format!("ado-vote-{repo}-{n}-{tid}"), repo, Some(n), "review");
                            e.author = Some(who(&t, "/comments/0/author"));
                            e.body = Some(s(&t, "/comments/0/content"));
                            e.state = Some(st.into());
                            evs.push(e);
                        }
                    }
                    continue;
                }
                if is_system(&t) {
                    continue;
                }
                let file = os(&t, "/threadContext/filePath").is_some();
                for c in live_comments(&t) {
                    if !after(ts(&c, "/publishedDate")) {
                        continue;
                    }
                    bump(ts(&c, "/publishedDate"));
                    evs.push(comment_event(repo, n, tid, file, &c));
                }
            }
            let its = items(&self.get(&self.repo_url(repo, &format!("/pullRequests/{n}/iterations"), "")).await?);
            for it in its.iter().filter(|it| oi(it, "/id").unwrap_or(0) > 1) {
                if after(ts(it, "/createdDate")) {
                    bump(ts(it, "/createdDate"));
                    if let Some(sha) = os(it, "/sourceRefCommit/commitId") {
                        let mut e = ev(format!("push-{repo}-{sha}"), repo, Some(n), "pushed");
                        e.author = os(it, "/author/uniqueName");
                        e.branch = Some(branch.clone());
                        evs.push(e);
                    }
                }
            }
            for x in items(&self.get(&self.repo_url(repo, &format!("/pullRequests/{n}/statuses"), "")).await?) {
                let (status, conclusion) = status_state(&s(&x, "/state"));
                let t = ts(&x, "/updatedDate").or_else(|| ts(&x, "/creationDate"));
                if status == "completed" && after(t) {
                    bump(t);
                    let mut e = ev(format!("ado-status-{repo}-{n}-{}", oi(&x, "/id").unwrap_or(0)), repo, Some(n), "ci_completed");
                    e.author = Some("ci".into());
                    e.body = Some(status_name(&x));
                    e.state = conclusion;
                    e.branch = Some(branch.clone());
                    evs.push(e);
                }
            }
            for b in self.builds(&repo_id, &format!("refs/pull/{n}/merge")).await? {
                let c = build_check(&b);
                if c.status == "completed" && after(ts(&b, "/finishTime")) {
                    bump(ts(&b, "/finishTime"));
                    let mut e = ev(format!("ado-build-{}", oi(&b, "/id").unwrap_or(0)), repo, Some(n), "ci_completed");
                    e.author = Some("ci".into());
                    e.body = Some(c.name);
                    e.state = c.conclusion;
                    e.branch = Some(branch.clone());
                    evs.push(e);
                }
            }
        }
        Ok((evs, Some(max.to_rfc3339_opts(SecondsFormat::Secs, true))))
    }
}

/// Shared by polling and webhooks so both give the same id.
fn comment_event(repo: &str, n: i64, tid: i64, file_thread: bool, c: &Value) -> RepoEvent {
    let cid = oi(c, "/id").unwrap_or(0);
    let parent = oi(c, "/parentCommentId").unwrap_or(0);
    let kind = if file_thread && parent != 0 { "thread_reply" } else { "comment" };
    let mut e = ev(format!("ado-c-{repo}-{n}-{tid}-{cid}"), repo, Some(n), kind);
    e.author = Some(who(c, "/author"));
    e.body = Some(s(c, "/content"));
    if file_thread {
        e.thread_id = Some(tid.to_string());
    }
    e
}

fn closed_event(repo: &str, p: &Value, author: Option<String>) -> Option<RepoEvent> {
    let n = oi(p, "/pullRequestId")?;
    let mut e = match s(p, "/status").as_str() {
        "completed" => {
            let mut e = ev(format!("merged-{repo}-{n}"), repo, Some(n), "merged");
            e.state = Some("merged".into());
            e
        }
        "abandoned" => {
            let mut e = ev(format!("closed-{repo}-{n}-{}", s(p, "/closedDate")), repo, Some(n), "closed");
            e.state = Some("closed".into());
            e
        }
        _ => return None,
    };
    e.author = author.or_else(|| os(p, "/closedBy/uniqueName"));
    e.branch = Some(strip_heads(&s(p, "/sourceRefName")));
    Some(e)
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Service hooks carry `Authorization: Basic base64(user:password)` as set on
/// the subscription; the password must equal the shared secret.
pub fn verify_signature(secret: &str, headers: &HeaderMap, _body: &[u8]) -> bool {
    if secret.is_empty() {
        return false;
    }
    let Some(h) = headers.get("authorization").and_then(|v| v.to_str().ok()) else { return false };
    let Some(b64) = h.strip_prefix("Basic ").or_else(|| h.strip_prefix("basic ")) else { return false };
    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else { return false };
    let Some(pos) = raw.iter().position(|c| *c == b':') else { return false };
    ct_eq(&raw[pos + 1..], secret.as_bytes())
}

/// Service hook payload → repo events.
pub fn parse_webhook(_headers: &HeaderMap, body: &[u8]) -> Result<Vec<RepoEvent>> {
    let v: Value = serde_json::from_slice(body)?;
    let r = &v["resource"];
    let hook_id = s(&v, "/id");
    let mut out = vec![];
    match s(&v, "/eventType").as_str() {
        "git.pullrequest.created" => {
            let repo = s(r, "/repository/name");
            let n = oi(r, "/pullRequestId");
            let mut e = ev(format!("opened-{repo}-{}", n.unwrap_or(0)), &repo, n, "pr_opened");
            e.author = Some(who(r, "/createdBy"));
            e.branch = Some(strip_heads(&s(r, "/sourceRefName")));
            out.push(e);
        }
        "git.pullrequest.updated" | "git.pullrequest.merged" => {
            let repo = s(r, "/repository/name");
            if let Some(e) = closed_event(&repo, r, None) {
                out.push(e);
            } else if s(&v, "/eventType") == "git.pullrequest.updated" && s(&v, "/message/text").contains("source branch") {
                if let Some(sha) = os(r, "/lastMergeSourceCommit/commitId") {
                    let mut e = ev(format!("push-{repo}-{sha}"), &repo, oi(r, "/pullRequestId"), "pushed");
                    e.branch = Some(strip_heads(&s(r, "/sourceRefName")));
                    out.push(e);
                }
            }
        }
        "ms.vss-code.git-pullrequest-comment-event" => {
            let c = &r["comment"];
            let p = &r["pullRequest"];
            if s(c, "/commentType") != "system" && c.get("isDeleted").and_then(Value::as_bool) != Some(true) {
                let repo = s(p, "/repository/name");
                let n = oi(p, "/pullRequestId").unwrap_or(0);
                // .../pullRequests/{n}/threads/{thread}
                let tid = os(c, "/_links/threads/href").and_then(|h| h.trim_end_matches('/').rsplit('/').next().and_then(|x| x.parse().ok()));
                match tid {
                    Some(tid) => {
                        // The payload does not say whether the thread has a file context;
                        // replies are reported as thread replies.
                        let file = oi(c, "/parentCommentId").unwrap_or(0) != 0;
                        out.push(comment_event(&repo, n, tid, file, c));
                    }
                    None => {
                        let mut e = ev(format!("ado-{hook_id}-0"), &repo, Some(n), "comment");
                        e.author = Some(who(c, "/author"));
                        e.body = Some(s(c, "/content"));
                        out.push(e);
                    }
                }
            }
        }
        "git.push" => {
            let repo = s(r, "/repository/name");
            for (i, u) in r.get("refUpdates").and_then(Value::as_array).cloned().unwrap_or_default().iter().enumerate() {
                let Some(branch) = s(u, "/name").strip_prefix("refs/heads/").map(String::from) else { continue };
                let new = s(u, "/newObjectId");
                if new.chars().all(|c| c == '0') {
                    continue; // deleted
                }
                let mut e = ev(format!("ado-branchpush-{repo}-{branch}-{new}"), &repo, None, "pushed");
                if new.is_empty() {
                    e.id = format!("ado-{hook_id}-{i}");
                }
                e.author = Some(who(r, "/pushedBy"));
                e.branch = Some(branch);
                out.push(e);
            }
        }
        _ => {}
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::github::mock::{self, Req, Resp, ok, status};
    use super::*;
    use std::sync::{Arc, Mutex};

    const PAT: &str = "adopat123";

    fn loaded(org: &str) -> Loaded {
        Loaded {
            row: ConnectorRow {
                id: Uuid::nil(),
                kind: "azure_devops".into(),
                role: "repo".into(),
                name: "ado".into(),
                config: json!({"org_url": org, "project": "My Proj"}),
                service_identity: Some("factory-bot".into()),
            },
            secrets: HashMap::from([("pat".to_string(), PAT.to_string())]),
        }
    }

    async fn ado(h: impl Fn(&Req, &str) -> Resp + Send + Sync + 'static) -> (AzureDevOps, Arc<Mutex<Vec<Req>>>) {
        let (base, log) = mock::start(Arc::new(h)).await;
        (AzureDevOps::from_loaded(&loaded(&format!("{base}/org"))).unwrap(), log)
    }

    fn basic() -> String {
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!(":{PAT}")))
    }

    const R: &str = "/org/My%20Proj/_apis/git/repositories/app";

    fn pr_json(n: i64, status: &str) -> Value {
        json!({
            "pullRequestId": n, "status": status, "title": "Add thing", "description": "desc",
            "repository": {"id": "rid-1", "name": "app"},
            "sourceRefName": "refs/heads/feature/x", "targetRefName": "refs/heads/main",
            "createdBy": {"displayName": "Dev", "uniqueName": "dev@example.com"},
            "creationDate": "2026-02-01T10:00:00.123Z", "closedDate": if status == "active" { Value::Null } else { json!("2026-02-01T10:30:00.5Z") },
            "lastMergeSourceCommit": {"commitId": "0123456789abcdef0123456789abcdef01234567"},
            "mergeStatus": "succeeded",
            "reviewers": [
                {"id": "r1", "uniqueName": "alice@example.com", "vote": 10},
                {"id": "r2", "uniqueName": "bob@example.com", "vote": 0},
                {"id": "r3", "uniqueName": "carol@example.com", "vote": -5}
            ]
        })
    }

    fn threads_json() -> Value {
        json!({"value": [
            {"id": 5, "status": "active", "threadContext": {"filePath": "/src/a.rs", "rightFileStart": {"line": 4, "offset": 1}},
             "publishedDate": "2026-02-01T11:00:00Z",
             "comments": [
                {"id": 1, "parentCommentId": 0, "author": {"uniqueName": "alice@example.com"}, "content": "rename", "commentType": "text", "publishedDate": "2026-02-01T11:00:00Z"},
                {"id": 2, "parentCommentId": 1, "author": {"uniqueName": "factory-bot"}, "content": "done <!-- factory:r -->", "commentType": "text", "publishedDate": "2026-02-01T11:01:00Z"},
                {"id": 3, "parentCommentId": 1, "author": {"uniqueName": "x"}, "content": "deleted", "commentType": "text", "isDeleted": true}
             ]},
            {"id": 6, "status": "wontFix", "threadContext": {"filePath": "/src/b.rs", "leftFileStart": {"line": 9, "offset": 1}},
             "comments": [{"id": 1, "parentCommentId": 0, "author": {"uniqueName": "bob@example.com"}, "content": "why", "commentType": "text"}]},
            {"id": 7, "status": "fixed", "threadContext": {"filePath": "/src/c.rs"},
             "comments": [{"id": 1, "parentCommentId": 0, "author": {"uniqueName": "bob@example.com"}, "content": "ok", "commentType": "text"}]},
            {"id": 8, "status": "active", "threadContext": null,
             "comments": [{"id": 1, "parentCommentId": 0, "author": {"uniqueName": "factory-bot"}, "content": "status <!-- factory:g -->", "commentType": "text",
                           "publishedDate": "2026-02-01T11:02:00Z"}]},
            {"id": 9, "properties": {"CodeReviewThreadType": {"$type": "System.String", "$value": "VoteUpdate"},
                                     "CodeReviewVoteResult": {"$type": "System.String", "$value": "-10"}},
             "publishedDate": "2026-02-01T11:03:00Z",
             "comments": [{"id": 1, "author": {"uniqueName": "carol@example.com"}, "content": "Carol voted -10", "commentType": "system"}]},
            {"id": 10, "isDeleted": true, "comments": []}
        ]})
    }

    #[test]
    fn config_and_remote() {
        let a = AzureDevOps::from_loaded(&loaded("https://dev.azure.com/acme/")).unwrap();
        assert_eq!(a.search_base, "https://almsearch.dev.azure.com/acme");
        let r = a.git_remote("app").unwrap();
        assert_eq!(r.url, "https://dev.azure.com/acme/My%20Proj/_git/app");
        assert_eq!(r.auth_header.unwrap(), basic());
        assert!(a.git_remote("a/b").is_err());
        let mut l = loaded("https://dev.azure.com/acme");
        l.row.config = json!({"org_url": "https://dev.azure.com/acme"});
        assert!(AzureDevOps::from_loaded(&l).is_err());
    }

    #[test]
    fn thread_split_and_status_mapping() {
        let (th, general) = split_threads(&items(&threads_json()));
        assert_eq!(th.len(), 3);
        assert_eq!((th[0].id.as_str(), th[0].path.as_deref(), th[0].line, th[0].status.as_deref()), ("5", Some("src/a.rs"), Some(4), None));
        assert_eq!(th[0].comments.len(), 2);
        assert_eq!(th[0].comments[1].id, "5/2");
        assert_eq!((th[1].line, th[1].status.as_deref()), (Some(9), Some("wont_fix")));
        assert_eq!(th[2].status.as_deref(), Some("resolved"));
        assert_eq!(general.len(), 1);
        assert_eq!(general[0].id, "8/1");
        assert_eq!(thread_status("closed").as_deref(), Some("resolved"));
        assert_eq!(thread_status("active"), None);
    }

    #[test]
    fn long_description_keeps_marker() {
        let body = format!("{}\n\n<!-- factory:m -->", "x".repeat(10_000));
        let d = fit_description(&body);
        assert!(d.chars().count() <= MAX_DESCRIPTION);
        assert!(d.ends_with("<!-- factory:m -->"));
        assert_eq!(fit_description("short"), "short");
    }

    #[tokio::test]
    async fn repo_info_files_and_auth() {
        let (a, log) = ado(|r, _| match r.path.as_str() {
            p if p == R => ok(json!({"id": "rid-1", "name": "app", "defaultBranch": "refs/heads/main"})),
            "/org/My%20Proj/_apis/policy/configurations" => ok(json!({"value": [
                {"isEnabled": true, "isBlocking": true, "settings": {"scope": [{"repositoryId": "rid-1", "refName": "refs/heads/release", "matchKind": "Exact"}]}},
                {"isEnabled": true, "isBlocking": true, "settings": {"scope": [{"repositoryId": null, "refName": "refs/heads/main", "matchKind": "exact"}]}},
                {"isEnabled": true, "isBlocking": false, "settings": {"scope": [{"repositoryId": "rid-1", "refName": "refs/heads/dev", "matchKind": "exact"}]}},
                {"isEnabled": true, "isBlocking": true, "settings": {"scope": [{"repositoryId": "other", "refName": "refs/heads/x", "matchKind": "exact"}]}},
                {"isEnabled": true, "isBlocking": true, "settings": {"scope": [{"repositoryId": "rid-1", "refName": "refs/heads/rel/", "matchKind": "prefix"}]}}
            ]})),
            p if p == format!("{R}/items") && r.query.contains("recursionLevel") => ok(json!({"value": [
                {"path": "/", "isFolder": true}, {"path": "/src", "isFolder": true}, {"path": "/src/a.rs", "gitObjectType": "blob"}]})),
            p if p == format!("{R}/items") && r.query.contains("missing") => status(404, "{}"),
            p if p == format!("{R}/items") => ok(json!({"path": "/src/a.rs", "content": "fn main() {}"})),
            _ => status(404, ""),
        })
        .await;
        let info = a.repo_info("app").await.unwrap();
        assert_eq!(info.default_branch, "main");
        assert_eq!(info.protected_branches, vec!["main", "release"]);
        assert_eq!(a.list_files("app", "feature/x").await.unwrap(), vec!["src/a.rs"]);
        assert_eq!(a.read_file("app", "0123456789abcdef0123456789abcdef01234567", "src/a.rs").await.unwrap().as_deref(), Some("fn main() {}"));
        assert_eq!(a.read_file("app", "main", "missing.rs").await.unwrap(), None);
        let log = log.lock().unwrap();
        assert!(log.iter().all(|r| r.header("authorization") == basic()));
        assert_eq!(log[0].query, "api-version=7.1");
        assert_eq!(log[1].query, "repositoryId=rid-1&api-version=7.1");
        assert_eq!(log[2].query, "recursionLevel=Full&versionDescriptor.version=feature%2Fx&versionDescriptor.versionType=branch&api-version=7.1");
        assert!(log[3].query.contains("path=%2Fsrc%2Fa.rs&includeContent=true&versionDescriptor.version=0123456789abcdef0123456789abcdef01234567&versionDescriptor.versionType=commit"));
    }

    #[tokio::test]
    async fn sign_in_redirect_is_an_auth_error() {
        let (a, _) = ado(|_, _| Resp { status: 302, headers: vec![("location".into(), "https://login.example/".into())], body: String::new() }).await;
        let e = a.repo_info("app").await.unwrap_err().to_string();
        assert!(e.contains("PAT"), "{e}");
    }

    #[tokio::test]
    async fn pagination_and_pr_mapping() {
        let (a, log) = ado(|r, _| {
            if r.path == format!("{R}/pullrequests") {
                let skip: usize = r.query.split('&').find_map(|kv| kv.strip_prefix("$skip=")).and_then(|x| x.parse().ok()).unwrap_or(0);
                let n = if skip == 0 { 100 } else { 3 };
                ok(json!({"value": (0..n).map(|i| pr_json((skip + i) as i64 + 1, "completed")).collect::<Vec<_>>(), "count": n}))
            } else {
                status(404, "")
            }
        })
        .await;
        let prs = a.list_prs("app", "merged").await.unwrap();
        assert_eq!(prs.len(), 103);
        assert_eq!(prs[0].state, "merged");
        assert_eq!(prs[0].head, "feature/x");
        assert_eq!(prs[0].author, "dev@example.com");
        assert!(prs[0].url.ends_with("/org/My%20Proj/_git/app/pullrequest/1"));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert!(log[0].query.starts_with("searchCriteria.status=completed&$top=100&$skip=0"));
        assert!(log[1].query.contains("$skip=100"));
    }

    fn pr_handler(r: &Req, _: &str) -> Resp {
        let p = r.path.strip_prefix(R).unwrap_or(&r.path);
        match (r.method.as_str(), p) {
            ("GET", "/pullrequests") => ok(json!({"value": [pr_json(7, "active")]})),
            ("POST", "/pullrequests") => ok(pr_json(8, "active")),
            ("GET", "/pullrequests/7") => ok(pr_json(7, "active")),
            ("PATCH", "/pullrequests/7") => ok(pr_json(7, "completed")),
            ("GET", "/pullRequests/7/threads") => ok(threads_json()),
            ("POST", "/pullRequests/7/threads") => ok(json!({"id": 11, "comments": [{"id": 1}]})),
            ("POST", "/pullRequests/7/threads/5/comments") => ok(json!({"id": 4})),
            ("PATCH", "/pullRequests/7/threads/5") => ok(json!({"id": 5, "status": "fixed"})),
            _ => status(404, ""),
        }
    }

    #[tokio::test]
    async fn pr_detail_comments_and_marker_search() {
        let (a, log) = ado(pr_handler).await;
        let p = a.find_open_pr("app", "feature/x").await.unwrap().unwrap();
        assert_eq!(p.number, 7);
        assert_eq!(p.head_sha.as_deref(), Some("0123456789abcdef0123456789abcdef01234567"));
        let d = a.get_pr("app", 7).await.unwrap();
        assert_eq!(d.reviews.len(), 2);
        assert_eq!(d.review_decision(), "changes_requested");
        assert!(d.comments.iter().any(|c| c.body.contains("<!-- factory:g -->")));
        assert_eq!(d.threads.iter().find(|t| t.comments.iter().any(|c| c.body.contains("<!-- factory:r -->"))).unwrap().id, "5");
        assert_eq!(a.open_pr("app", "feature/x", "main", "T", "B").await.unwrap().number, 8);
        let log = log.lock().unwrap();
        assert_eq!(log[0].query, "searchCriteria.status=active&searchCriteria.sourceRefName=refs%2Fheads%2Ffeature%2Fx&api-version=7.1");
        assert_eq!(log.last().unwrap().json(), json!({"sourceRefName": "refs/heads/feature/x", "targetRefName": "refs/heads/main", "title": "T", "description": "B"}));
    }

    #[tokio::test]
    async fn comment_reply_resolve_merge() {
        let (a, log) = ado(pr_handler).await;
        assert_eq!(a.comment("app", 7, "general", None, None).await.unwrap(), json!({"comment_id": "11/1"}));
        assert_eq!(a.comment("app", 7, "inline", Some("src/a.rs"), Some(3)).await.unwrap(), json!({"thread_id": "11", "comment_id": "11/1"}));
        assert_eq!(a.reply("app", 7, "5", "ok").await.unwrap()["comment_id"], "5/4");
        a.resolve_thread("app", 7, "5", "resolved").await.unwrap();
        a.resolve_thread("app", 7, "5", "wont_fix").await.unwrap();
        assert!(a.resolve_thread("app", 7, "5", "maybe").await.is_err());
        a.merge("app", 7).await.unwrap();
        let log = log.lock().unwrap();
        let posts: Vec<&Req> = log.iter().filter(|r| r.method == "POST" && r.path.ends_with("/threads")).collect();
        assert_eq!(posts[0].json(), json!({"comments": [{"parentCommentId": 0, "content": "general", "commentType": 1}], "status": 1}));
        assert_eq!(posts[1].json()["threadContext"], json!({"filePath": "/src/a.rs", "rightFileStart": {"line": 3, "offset": 1}, "rightFileEnd": {"line": 3, "offset": 1}}));
        let reply = log.iter().find(|r| r.path.ends_with("/threads/5/comments")).unwrap();
        assert_eq!(reply.json(), json!({"parentCommentId": 1, "content": "ok", "commentType": 1}));
        let patches: Vec<Value> = log.iter().filter(|r| r.method == "PATCH" && r.path.ends_with("/threads/5")).map(|r| r.json()).collect();
        assert_eq!(patches, vec![json!({"status": "fixed"}), json!({"status": "wontFix"})]);
        let merge = log.iter().find(|r| r.method == "PATCH" && r.path.ends_with("/pullrequests/7")).unwrap().json();
        assert_eq!(merge["status"], "completed");
        assert_eq!(merge["lastMergeSourceCommit"]["commitId"], "0123456789abcdef0123456789abcdef01234567");
    }

    fn ci_handler(r: &Req, _: &str) -> Resp {
        let p = r.path.as_str();
        if p == R {
            return ok(json!({"id": "rid-1", "name": "app"}));
        }
        match p.strip_prefix(R).unwrap_or(p) {
            "/refs" => ok(json!({"value": [{"name": "refs/heads/feature/x", "objectId": "0123456789abcdef0123456789abcdef01234567"}]})),
            "/commits/0123456789abcdef0123456789abcdef01234567/statuses" => ok(json!({"value": [
                {"id": 3, "state": "failed", "context": {"genre": "sonar", "name": "quality"}, "description": "2 bugs", "targetUrl": "https://sonar/x"}]})),
            "/pullrequests" => ok(json!({"value": [pr_json(7, "active")]})),
            "/pullRequests/7/statuses" => ok(json!({"value": [
                {"id": 1, "state": "pending", "context": {"genre": "ci", "name": "lint"}},
                {"id": 2, "state": "succeeded", "context": {"genre": "ci", "name": "lint"}, "updatedDate": "2026-02-01T12:00:00Z"}]})),
            "/org/My%20Proj/_apis/build/builds" if r.query.contains("refs%2Fpull%2F7%2Fmerge") => ok(json!({"value": [
                {"id": 40, "definition": {"id": 1, "name": "PR build"}, "status": "completed", "result": "partiallySucceeded",
                 "finishTime": "2026-02-01T12:30:00Z", "_links": {"web": {"href": "https://ado/b/40"}}},
                {"id": 39, "definition": {"id": 1, "name": "PR build"}, "status": "completed", "result": "succeeded", "finishTime": "2026-01-01T00:00:00Z"}]})),
            "/org/My%20Proj/_apis/build/builds" => ok(json!({"value": [{"id": 41, "definition": {"id": 2, "name": "CI"}, "status": "inProgress"}]})),
            "/org/My%20Proj/_apis/build/builds/40" => ok(json!({"id": 40, "definition": {"name": "PR build"}, "buildNumber": "20260201.1", "status": "completed", "result": "failed"})),
            "/org/My%20Proj/_apis/build/builds/40/timeline" => ok(json!({"records": [
                {"name": "Build", "result": "succeeded", "log": {"id": 2}},
                {"name": "Test", "result": "failed", "log": {"id": 5}, "issues": [{"type": "error", "message": "3 tests failed"}]}]})),
            "/org/My%20Proj/_apis/build/builds/40/logs/5" => Resp { status: 200, headers: vec![], body: "running tests\nFAIL test_x".into() },
            _ => status(404, ""),
        }
    }

    #[tokio::test]
    async fn checks_and_logs() {
        let (a, log) = ado(ci_handler).await;
        let cs = a.checks("app", "feature/x").await.unwrap();
        let got: Vec<(String, String, String, Option<String>)> = cs.iter().map(|c| (c.id.clone(), c.name.clone(), c.status.clone(), c.conclusion.clone())).collect();
        assert_eq!(got, vec![
            ("status:0123456789abcdef0123456789abcdef01234567:3".into(), "sonar/quality".into(), "completed".into(), Some("failure".into())),
            ("prstatus:7:2".into(), "ci/lint".into(), "completed".into(), Some("success".into())),
            ("build:41".into(), "CI".into(), "in_progress".into(), None),
            ("build:40".into(), "PR build".into(), "completed".into(), Some("failure".into())),
        ]);
        let logs = a.check_logs("app", "build:40").await.unwrap();
        assert!(logs.contains("Test: error: 3 tests failed"));
        assert!(logs.contains("FAIL test_x"));
        let st = a.check_logs("app", "status:0123456789abcdef0123456789abcdef01234567:3").await.unwrap();
        assert!(st.contains("2 bugs") && st.contains("https://sonar/x"));
        assert!(a.check_logs("app", "nope").await.is_err());
        let log = log.lock().unwrap();
        assert!(log.iter().any(|r| r.path.ends_with("/statuses") && r.query == "latestOnly=true&api-version=7.1"));
        assert!(log.iter().any(|r| r.path.ends_with("/logs/5") && r.header("accept") == "text/plain"));
        assert!(log.iter().any(|r| r.path.ends_with("/build/builds") && r.query.contains("repositoryId=rid-1&repositoryType=TfsGit")));
    }

    #[tokio::test]
    async fn poll_produces_events() {
        let (a, log) = ado(|r, base| {
            let p = r.path.strip_prefix(R).unwrap_or(&r.path);
            match p {
                "/pullrequests" if r.query.contains("queryTimeRangeType=created") => ok(json!({"value": [pr_json(9, "active")]})),
                "/pullrequests" if r.query.contains("queryTimeRangeType=closed") => ok(json!({"value": [pr_json(6, "completed"), pr_json(5, "abandoned")]})),
                "/pullrequests" => ok(json!({"value": [pr_json(7, "active")]})),
                "/pullRequests/7/threads" => ok(threads_json()),
                "/pullRequests/7/iterations" => ok(json!({"value": [
                    {"id": 1, "createdDate": "2026-02-01T10:00:00Z", "sourceRefCommit": {"commitId": "aaa"}},
                    {"id": 2, "createdDate": "2026-02-01T11:30:00Z", "sourceRefCommit": {"commitId": "bbb"}, "author": {"uniqueName": "dev@example.com"}}]})),
                "/pullRequests/7/statuses" => ok(json!({"value": [{"id": 2, "state": "failed", "context": {"name": "lint"}, "updatedDate": "2026-02-01T12:00:00Z"}]})),
                _ => ci_handler(r, base),
            }
        })
        .await;
        let (evs, next) = a.poll("app", Some("2026-02-01T00:00:00Z")).await.unwrap();
        let got: Vec<(&str, &str)> = evs.iter().map(|e| (e.kind.as_str(), e.id.as_str())).collect();
        assert_eq!(got, vec![
            ("pr_opened", "opened-app-9"),
            ("merged", "merged-app-6"),
            ("closed", "closed-app-5-2026-02-01T10:30:00.5Z"),
            ("comment", "ado-c-app-7-5-1"),
            ("thread_reply", "ado-c-app-7-5-2"),
            ("comment", "ado-c-app-7-8-1"),
            ("review", "ado-vote-app-7-9"),
            ("pushed", "push-app-bbb"),
            ("ci_completed", "ado-status-app-7-2"),
            ("ci_completed", "ado-build-40"),
        ]);
        let reply = evs.iter().find(|e| e.kind == "thread_reply").unwrap();
        assert_eq!(reply.thread_id.as_deref(), Some("5"));
        assert_eq!(evs.iter().find(|e| e.kind == "review").unwrap().state.as_deref(), Some("changes_requested"));
        assert_eq!(evs.iter().find(|e| e.id == "ado-c-app-7-8-1").unwrap().thread_id, None);
        assert_eq!(next.as_deref(), Some("2026-02-01T12:30:00Z"));
        let log = log.lock().unwrap();
        assert!(log.iter().any(|r| r.query.contains("searchCriteria.minTime=2026-02-01T00%3A00%3A00Z")));
    }

    #[tokio::test]
    async fn poll_without_cursor_starts_now() {
        let (a, log) = ado(|_, _| status(500, "")).await;
        let (evs, next) = a.poll("app", None).await.unwrap();
        assert!(evs.is_empty() && next.is_some());
        assert!(log.lock().unwrap().is_empty());
    }

    // ---- webhooks ----

    fn auth_headers(user: &str, pass: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        let v = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        h.insert("authorization", format!("Basic {v}").parse().unwrap());
        h
    }

    #[test]
    fn signature_valid_and_invalid() {
        assert!(verify_signature("hooksecret", &auth_headers("factory", "hooksecret"), b"{}"));
        assert!(verify_signature("hooksecret", &auth_headers("", "hooksecret"), b"{}"));
        assert!(!verify_signature("hooksecret", &auth_headers("factory", "wrong"), b"{}"));
        assert!(!verify_signature("hooksecret", &auth_headers("factory", "hooksecret2"), b"{}"));
        assert!(!verify_signature("", &auth_headers("factory", ""), b"{}"));
        assert!(!verify_signature("hooksecret", &HeaderMap::new(), b"{}"));
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer hooksecret".parse().unwrap());
        assert!(!verify_signature("hooksecret", &h, b"{}"));
    }

    fn repo_obj() -> Value {
        json!({"id": "rid-1", "name": "app", "project": {"id": "p1", "name": "My Proj"}, "remoteUrl": "https://acme@dev.azure.com/acme/My%20Proj/_git/app"})
    }

    fn hook(payload: Value) -> Vec<RepoEvent> {
        parse_webhook(&HeaderMap::new(), payload.to_string().as_bytes()).unwrap()
    }

    fn pr_resource(status: &str) -> Value {
        let mut p = pr_json(7, status);
        p["repository"] = repo_obj();
        p
    }

    #[test]
    fn webhook_pull_request_events() {
        let e = hook(json!({"id": "e1", "eventType": "git.pullrequest.created", "publisherId": "tfs",
            "message": {"text": "Dev created a new pull request"}, "resource": pr_resource("active")}));
        assert_eq!((e[0].kind.as_str(), e[0].id.as_str(), e[0].branch.as_deref(), e[0].author.as_deref()),
            ("pr_opened", "opened-app-7", Some("feature/x"), Some("dev@example.com")));
        let e = hook(json!({"id": "e2", "eventType": "git.pullrequest.updated",
            "message": {"text": "Dev updated the source branch of pull request 7 (Add thing) in app"}, "resource": pr_resource("active")}));
        assert_eq!((e[0].kind.as_str(), e[0].id.as_str()), ("pushed", "push-app-0123456789abcdef0123456789abcdef01234567"));
        let e = hook(json!({"id": "e3", "eventType": "git.pullrequest.updated",
            "message": {"text": "Dev marked the pull request as completed"}, "resource": pr_resource("completed")}));
        assert_eq!((e[0].kind.as_str(), e[0].id.as_str()), ("merged", "merged-app-7"));
        let e = hook(json!({"id": "e4", "eventType": "git.pullrequest.updated", "message": {"text": "Dev abandoned the pull request"}, "resource": pr_resource("abandoned")}));
        assert_eq!(e[0].kind, "closed");
        assert!(hook(json!({"id": "e5", "eventType": "git.pullrequest.updated", "message": {"text": "Dev added a reviewer"}, "resource": pr_resource("active")})).is_empty());
        let e = hook(json!({"id": "e6", "eventType": "git.pullrequest.merged", "message": {"text": "Dev has created a pull request merge commit"}, "resource": pr_resource("completed")}));
        assert_eq!(e[0].id, "merged-app-7");
        assert!(hook(json!({"id": "e7", "eventType": "git.pullrequest.merged", "resource": pr_resource("active")})).is_empty());
    }

    #[test]
    fn webhook_comment_and_push_events() {
        let comment = |id: i64, parent: i64, ctype: &str| json!({
            "id": "c1", "eventType": "ms.vss-code.git-pullrequest-comment-event", "publisherId": "tfs",
            "message": {"text": "Alice has commented on a pull request"},
            "resource": {
                "comment": {"id": id, "parentCommentId": parent, "author": {"displayName": "Alice", "uniqueName": "alice@example.com"},
                    "content": "Please rename", "publishedDate": "2026-02-01T11:00:00Z", "commentType": ctype,
                    "_links": {"self": {"href": "https://dev.azure.com/acme/_apis/git/repositories/rid-1/pullRequests/7/threads/5/comments/2"},
                               "threads": {"href": "https://dev.azure.com/acme/_apis/git/repositories/rid-1/pullRequests/7/threads/5"}}},
                "pullRequest": pr_resource("active")
            }
        });
        let e = hook(comment(1, 0, "text"));
        assert_eq!((e[0].kind.as_str(), e[0].id.as_str(), e[0].pr_number, e[0].thread_id.as_deref()), ("comment", "ado-c-app-7-5-1", Some(7), None));
        let e = hook(comment(2, 1, "text"));
        assert_eq!((e[0].kind.as_str(), e[0].id.as_str(), e[0].thread_id.as_deref(), e[0].author.as_deref()),
            ("thread_reply", "ado-c-app-7-5-2", Some("5"), Some("alice@example.com")));
        assert!(hook(comment(3, 0, "system")).is_empty());
        let e = hook(json!({"id": "p1", "eventType": "git.push", "resource": {
            "refUpdates": [
                {"name": "refs/heads/feature/x", "oldObjectId": "aaa", "newObjectId": "bbb"},
                {"name": "refs/heads/gone", "oldObjectId": "aaa", "newObjectId": "0000000000000000000000000000000000000000"},
                {"name": "refs/tags/v1", "oldObjectId": "0", "newObjectId": "ccc"}],
            "pushedBy": {"displayName": "Dev", "uniqueName": "dev@example.com"}, "repository": repo_obj(), "pushId": 14}}));
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].kind.as_str(), e[0].id.as_str(), e[0].branch.as_deref()), ("pushed", "ado-branchpush-app-feature/x-bbb", Some("feature/x")));
        assert!(hook(json!({"id": "x", "eventType": "build.complete", "resource": {}})).is_empty());
    }

    // ---- contract tests against the real service ----

    fn contract() -> Option<(AzureDevOps, String)> {
        let org = std::env::var("FACTORY_CONTRACT_ADO_ORG_URL").ok()?;
        let project = std::env::var("FACTORY_CONTRACT_ADO_PROJECT").ok()?;
        let pat = std::env::var("FACTORY_CONTRACT_ADO_PAT").ok()?;
        let repo = std::env::var("FACTORY_CONTRACT_ADO_REPO").ok()?;
        Some((AzureDevOps::new(&org, &project, None, &pat).unwrap(), repo))
    }

    #[tokio::test]
    #[ignore]
    async fn contract_read_only() {
        let Some((a, repo)) = contract() else {
            eprintln!("set FACTORY_CONTRACT_ADO_ORG_URL, _PROJECT, _PAT and _REPO to run");
            return;
        };
        println!("{}", a.test_connection().await.unwrap());
        assert!(a.list_repos().await.unwrap().iter().any(|r| r.full_name == repo));
        let info = a.repo_info(&repo).await.unwrap();
        let files = a.list_files(&repo, &info.default_branch).await.unwrap();
        assert!(!files.is_empty());
        assert!(a.read_file(&repo, &info.default_branch, &files[0]).await.unwrap().is_some());
        assert!(a.read_file(&repo, &info.default_branch, "definitely/not/here.txt").await.unwrap().is_none());
        let _ = a.checks(&repo, &info.default_branch).await.unwrap();
        if let Some(p) = a.list_prs(&repo, "all").await.unwrap().first() {
            assert_eq!(a.get_pr(&repo, p.number).await.unwrap().pr.number, p.number);
        }
        let since = (Utc::now() - chrono::Duration::days(7)).to_rfc3339_opts(SecondsFormat::Secs, true);
        assert!(a.poll(&repo, Some(&since)).await.unwrap().1.is_some());
    }
}
