//! GitHub repo host: REST v3 for everything, GraphQL for review-thread
//! resolution. Works against github.com and GitHub Enterprise Server
//! (`api_url` = `https://<host>/api/v3`).
//!
//! Config: `api_url` (default `https://api.github.com`), `owner` (optional;
//! limits `list_repos` to one user or organisation). Secrets: `token`
//! (installation token, fine-grained or classic PAT) and `webhook_secret`.
//!
//! Event ids are derived from the GitHub object (comment id, review id, head
//! sha, ...) so that a webhook delivery and a later poll of the same change
//! de-duplicate; only events without a stable object id fall back to the
//! `X-GitHub-Delivery` id plus an index.
//!
//! Contract tests (ignored by default) read:
//! - `FACTORY_CONTRACT_GITHUB_TOKEN`: a token with read access to the repo
//! - `FACTORY_CONTRACT_GITHUB_REPO`: `owner/name` of a repository to read
//! - `FACTORY_CONTRACT_GITHUB_API_URL` (optional): API base for GHES

use super::*;
use axum::http::HeaderMap;
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::{Method, StatusCode};
use std::time::Duration;

const API_VERSION: &str = "2022-11-28";
const MAX_PAGES: usize = 20;
/// Hidden marker on the reply that closes a thread as "won't fix", so the
/// thread reads back as `wont_fix` rather than `resolved`.
pub const WONT_FIX_MARKER: &str = "<!-- factory:wont_fix -->";

pub struct GitHub {
    http: reqwest::Client,
    api: String,
    graphql: String,
    web: String,
    owner: Option<String>,
    token: String,
}

// ---------- small JSON helpers ----------

fn s(v: &Value, p: &str) -> String {
    v.pointer(p).and_then(Value::as_str).unwrap_or("").to_string()
}
fn os(v: &Value, p: &str) -> Option<String> {
    v.pointer(p).and_then(Value::as_str).map(String::from).filter(|x| !x.is_empty())
}
fn oi(v: &Value, p: &str) -> Option<i64> {
    v.pointer(p).and_then(Value::as_i64)
}
fn ts(v: &Value, p: &str) -> Option<DateTime<Utc>> {
    v.pointer(p)
        .and_then(Value::as_str)
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|d| d.with_timezone(&Utc))
}
/// Percent-encode a path, keeping `/`.
fn enc_path(x: &str) -> String {
    x.split('/').map(|seg| url::form_urlencoded::byte_serialize(seg.as_bytes()).collect::<String>().replace('+', "%20")).collect::<Vec<_>>().join("/")
}
fn q(x: &str) -> String {
    url::form_urlencoded::byte_serialize(x.as_bytes()).collect()
}
fn split_repo(repo: &str) -> Result<(&str, &str)> {
    repo.split_once('/').filter(|(o, n)| !o.is_empty() && !n.is_empty() && !n.contains('/')).ok_or_else(|| anyhow!("GitHub repository must be owner/name, got {repo}"))
}
fn next_link(h: &HeaderMap) -> Option<String> {
    let l = h.get("link")?.to_str().ok()?;
    l.split(',').find(|p| p.contains("rel=\"next\"")).and_then(|p| {
        let a = p.find('<')? + 1;
        let b = p.find('>')?;
        Some(p[a..b].to_string())
    })
}
fn snippet(b: &str) -> String {
    b.chars().take(400).collect()
}

fn ev(id: String, repo: &str, pr: Option<i64>, kind: &str) -> RepoEvent {
    RepoEvent { id, repo: repo.into(), pr_number: pr, kind: kind.into(), author: None, body: None, state: None, thread_id: None, branch: None }
}

pub fn pr_from(v: &Value) -> Pr {
    let state = if v.get("merged_at").map(|m| !m.is_null()).unwrap_or(false) || v.get("merged").and_then(Value::as_bool) == Some(true) {
        "merged".to_string()
    } else {
        s(v, "/state")
    };
    Pr {
        number: oi(v, "/number").unwrap_or(0),
        url: s(v, "/html_url"),
        title: s(v, "/title"),
        body: s(v, "/body"),
        state,
        head: s(v, "/head/ref"),
        base: s(v, "/base/ref"),
        author: s(v, "/user/login"),
        head_sha: os(v, "/head/sha"),
    }
}

fn comment_from(v: &Value) -> Comment {
    Comment {
        id: v.get("id").map(|i| i.to_string()).unwrap_or_default(),
        author: s(v, "/user/login"),
        body: s(v, "/body"),
        created_at: os(v, "/created_at"),
    }
}

fn review_state(st: &str) -> Option<&'static str> {
    match st.to_ascii_lowercase().as_str() {
        "approved" => Some("approved"),
        "changes_requested" => Some("changes_requested"),
        "commented" => Some("commented"),
        _ => None, // pending, dismissed
    }
}

/// Group review comments into threads: thread id = root comment id. GitHub
/// points `in_reply_to_id` at the thread's top-level comment; replies to
/// replies are followed back to their root anyway.
pub fn group_threads(comments: &[Value], states: &HashMap<i64, (String, bool)>) -> Vec<PrThread> {
    let mut sorted: Vec<&Value> = comments.iter().collect();
    sorted.sort_by_key(|c| oi(c, "/id").unwrap_or(0));
    let mut root_of: HashMap<i64, i64> = HashMap::new();
    let mut order: Vec<i64> = vec![];
    let mut threads: HashMap<i64, PrThread> = HashMap::new();
    for c in sorted {
        let id = oi(c, "/id").unwrap_or(0);
        let root = match oi(c, "/in_reply_to_id") {
            Some(p) => *root_of.get(&p).unwrap_or(&p),
            None => id,
        };
        root_of.insert(id, root);
        let t = threads.entry(root).or_insert_with(|| {
            order.push(root);
            PrThread {
                id: root.to_string(),
                path: os(c, "/path"),
                line: oi(c, "/line").or_else(|| oi(c, "/original_line")),
                status: None,
                comments: vec![],
            }
        });
        t.comments.push(comment_from(c));
    }
    order
        .into_iter()
        .filter_map(|r| threads.remove(&r).map(|mut t| {
            if let Some((_, true)) = states.get(&r) {
                t.status = Some(if t.comments.iter().any(|c| c.body.contains(WONT_FIX_MARKER)) { "wont_fix" } else { "resolved" }.into());
            }
            t
        }))
        .collect()
}

fn pr_number_from_url(u: &str) -> Option<i64> {
    u.trim_end_matches('/').rsplit('/').next()?.parse().ok()
}

/// Review comment (created) → `comment` for a thread root, `thread_reply` otherwise.
fn review_comment_event(repo: &str, n: Option<i64>, c: &Value) -> RepoEvent {
    let id = oi(c, "/id").unwrap_or(0);
    let (kind, tid) = match oi(c, "/in_reply_to_id") {
        Some(p) => ("thread_reply", p),
        None => ("comment", id),
    };
    let n = n.or_else(|| os(c, "/pull_request_url").and_then(|u| pr_number_from_url(&u)));
    let mut e = ev(format!("gh-rc-{id}"), repo, n, kind);
    e.author = os(c, "/user/login");
    e.body = os(c, "/body");
    e.thread_id = Some(tid.to_string());
    e
}

fn issue_comment_event(repo: &str, n: Option<i64>, c: &Value) -> RepoEvent {
    let n = n.or_else(|| os(c, "/issue_url").and_then(|u| pr_number_from_url(&u)));
    let mut e = ev(format!("gh-ic-{}", oi(c, "/id").unwrap_or(0)), repo, n, "comment");
    e.author = os(c, "/user/login");
    e.body = os(c, "/body");
    e
}

/// A submitted review. Bodyless "commented" reviews are what GitHub creates
/// around single inline comments; those arrive as comment events instead.
fn review_event(repo: &str, n: i64, r: &Value) -> Option<RepoEvent> {
    let st = review_state(&s(r, "/state"))?;
    let body = s(r, "/body");
    if st == "commented" && body.trim().is_empty() {
        return None;
    }
    let mut e = ev(format!("gh-review-{}", oi(r, "/id").unwrap_or(0)), repo, Some(n), "review");
    e.author = os(r, "/user/login");
    e.body = Some(body);
    e.state = Some(st.into());
    Some(e)
}

fn check_run_event(repo: &str, cr: &Value, pr: Option<i64>, branch: Option<String>) -> RepoEvent {
    let mut e = ev(format!("gh-ci-{}", oi(cr, "/id").unwrap_or(0)), repo, pr, "ci_completed");
    e.author = os(cr, "/app/slug").or(Some("ci".into()));
    e.body = os(cr, "/name");
    e.state = os(cr, "/conclusion");
    e.branch = branch.or_else(|| os(cr, "/check_suite/head_branch"));
    e
}

fn opened_event(repo: &str, p: &Value) -> RepoEvent {
    let n = oi(p, "/number").unwrap_or(0);
    let mut e = ev(format!("opened-{repo}-{n}"), repo, Some(n), "pr_opened");
    e.author = os(p, "/user/login");
    e.branch = os(p, "/head/ref");
    e
}

fn closed_event(repo: &str, p: &Value, author: Option<String>) -> RepoEvent {
    let n = oi(p, "/number").unwrap_or(0);
    let merged = !p.get("merged_at").map(Value::is_null).unwrap_or(true) || p.get("merged").and_then(Value::as_bool) == Some(true);
    let mut e = if merged {
        let mut e = ev(format!("merged-{repo}-{n}"), repo, Some(n), "merged");
        e.state = Some("merged".into());
        e
    } else {
        let mut e = ev(format!("closed-{repo}-{n}-{}", s(p, "/closed_at")), repo, Some(n), "closed");
        e.state = Some("closed".into());
        e
    };
    e.author = author.or_else(|| os(p, "/merged_by/login"));
    e.branch = os(p, "/head/ref");
    e
}

/// Same id the plane uses for its own pushes, so a push it made is not
/// treated as a new human push when GitHub reports it back.
fn pushed_event(repo: &str, p: &Value, author: Option<String>) -> Option<RepoEvent> {
    let sha = os(p, "/head/sha")?;
    let mut e = ev(format!("push-{repo}-{sha}"), repo, oi(p, "/number"), "pushed");
    e.author = author;
    e.branch = os(p, "/head/ref");
    Some(e)
}

impl GitHub {
    pub fn from_loaded(l: &Loaded) -> Result<Self> {
        let api = l.cfg("api_url").unwrap_or_else(|| "https://api.github.com".into());
        Self::new(&api, l.cfg("owner"), &l.secret("token")?)
    }

    pub fn new(api_url: &str, owner: Option<String>, token: &str) -> Result<Self> {
        let api = api_url.trim_end_matches('/').to_string();
        let u = url::Url::parse(&api).map_err(|e| anyhow!("bad GitHub api_url {api}: {e}"))?;
        let host = u.host_str().ok_or_else(|| anyhow!("GitHub api_url {api} has no host"))?;
        let web = if host == "api.github.com" {
            "https://github.com".to_string()
        } else {
            match u.port() {
                Some(p) => format!("{}://{host}:{p}", u.scheme()),
                None => format!("{}://{host}", u.scheme()),
            }
        };
        let graphql = match api.strip_suffix("/v3") {
            Some(b) => format!("{b}/graphql"),
            None => format!("{api}/graphql"),
        };
        let http = reqwest::Client::builder().user_agent("ai-factory").timeout(Duration::from_secs(60)).build()?;
        Ok(GitHub { http, api, graphql, web, owner, token: token.to_string() })
    }

    fn rb(&self, m: Method, url: &str) -> reqwest::RequestBuilder {
        self.rb_accept(m, url, "application/vnd.github+json")
    }

    fn rb_accept(&self, m: Method, url: &str, accept: &str) -> reqwest::RequestBuilder {
        let url = if url.starts_with("http://") || url.starts_with("https://") { url.to_string() } else { format!("{}{url}", self.api) };
        self.http
            .request(m, url)
            .bearer_auth(&self.token)
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", API_VERSION)
    }

    async fn send(&self, rb: reqwest::RequestBuilder) -> Result<(StatusCode, HeaderMap, String)> {
        let r = rb.send().await?;
        let st = r.status();
        let h = r.headers().clone();
        Ok((st, h, r.text().await?))
    }

    async fn call(&self, m: Method, url: &str, body: Option<&Value>) -> Result<Value> {
        let mut rb = self.rb(m.clone(), url);
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let (st, _, text) = self.send(rb).await?;
        if !st.is_success() {
            bail!("GitHub {m} {url} failed: {st} {}", snippet(&text));
        }
        Ok(if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text)? })
    }

    async fn get(&self, url: &str) -> Result<Value> {
        self.call(Method::GET, url, None).await
    }

    async fn get_opt(&self, url: &str) -> Result<Option<Value>> {
        let (st, _, text) = self.send(self.rb(Method::GET, url)).await?;
        if st == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !st.is_success() {
            bail!("GitHub GET {url} failed: {st} {}", snippet(&text));
        }
        Ok(Some(serde_json::from_str(&text)?))
    }

    /// One page: items (from an array body, or the array under `key`) and the next link.
    async fn page(&self, url: &str, key: Option<&str>) -> Result<(Vec<Value>, Option<String>)> {
        let (st, h, text) = self.send(self.rb(Method::GET, url)).await?;
        if !st.is_success() {
            bail!("GitHub GET {url} failed: {st} {}", snippet(&text));
        }
        let v: Value = serde_json::from_str(&text)?;
        let items = match key {
            Some(k) => v.get(k).cloned().unwrap_or(Value::Null),
            None => v,
        };
        Ok((items.as_array().cloned().unwrap_or_default(), next_link(&h)))
    }

    async fn pages(&self, url: &str, key: Option<&str>) -> Result<Vec<Value>> {
        let mut out = vec![];
        let mut next = Some(url.to_string());
        for _ in 0..MAX_PAGES {
            let Some(u) = next.take() else { break };
            let (items, n) = self.page(&u, key).await?;
            out.extend(items);
            next = n;
        }
        Ok(out)
    }

    async fn graphql_call(&self, query: &str, variables: Value) -> Result<Value> {
        let v = self.call(Method::POST, &self.graphql.clone(), Some(&json!({"query": query, "variables": variables}))).await?;
        if let Some(errs) = v.get("errors").and_then(Value::as_array).filter(|e| !e.is_empty()) {
            bail!("GitHub GraphQL error: {}", errs.iter().map(|e| s(e, "/message")).collect::<Vec<_>>().join("; "));
        }
        Ok(v.get("data").cloned().unwrap_or(Value::Null))
    }

    /// Review threads of a PR: root comment database id → (thread node id, isResolved).
    pub async fn thread_states(&self, repo: &str, n: i64) -> Result<HashMap<i64, (String, bool)>> {
        let (owner, name) = split_repo(repo)?;
        const Q: &str = "query($owner:String!,$name:String!,$number:Int!,$after:String){repository(owner:$owner,name:$name){pullRequest(number:$number){reviewThreads(first:100,after:$after){pageInfo{hasNextPage endCursor} nodes{id isResolved comments(first:1){nodes{databaseId}}}}}}}";
        let mut out = HashMap::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let d = self.graphql_call(Q, json!({"owner": owner, "name": name, "number": n, "after": after})).await?;
            let rt = d.pointer("/repository/pullRequest/reviewThreads").cloned().unwrap_or(Value::Null);
            for node in rt.get("nodes").and_then(Value::as_array).cloned().unwrap_or_default() {
                if let Some(root) = oi(&node, "/comments/nodes/0/databaseId") {
                    out.insert(root, (s(&node, "/id"), node.get("isResolved").and_then(Value::as_bool).unwrap_or(false)));
                }
            }
            if rt.pointer("/pageInfo/hasNextPage").and_then(Value::as_bool) != Some(true) {
                break;
            }
            after = os(&rt, "/pageInfo/endCursor");
        }
        Ok(out)
    }

    fn branch_info(&self, v: &Value) -> RepoInfo {
        RepoInfo { full_name: s(v, "/full_name"), default_branch: os(v, "/default_branch").unwrap_or_else(|| "main".into()), protected_branches: vec![] }
    }
}

#[async_trait]
impl RepoHost for GitHub {
    async fn test_connection(&self) -> Result<String> {
        let (st, _, text) = self.send(self.rb(Method::GET, "/user")).await?;
        if st.is_success() {
            let v: Value = serde_json::from_str(&text)?;
            return Ok(format!("GitHub at {} as {}", self.api, s(&v, "/login")));
        }
        // Installation tokens cannot read /user.
        let (st2, _, text2) = self.send(self.rb(Method::GET, "/installation/repositories?per_page=1")).await?;
        if st2.is_success() {
            let v: Value = serde_json::from_str(&text2)?;
            return Ok(format!("GitHub at {} as an app installation with {} repositories", self.api, oi(&v, "/total_count").unwrap_or(0)));
        }
        bail!("GitHub rejected the token: {st} {}", snippet(&text))
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>> {
        let items = match &self.owner {
            Some(o) => match self.pages(&format!("/orgs/{}/repos?per_page=100", enc_path(o)), None).await {
                Ok(v) => v,
                Err(_) => self.pages(&format!("/users/{}/repos?per_page=100", enc_path(o)), None).await?,
            },
            None if self.token.starts_with("ghs_") => self.pages("/installation/repositories?per_page=100", Some("repositories")).await?,
            None => self.pages("/user/repos?per_page=100", None).await?,
        };
        let mut out = vec![];
        for v in items.iter().filter(|v| v.get("archived").and_then(Value::as_bool) != Some(true)) {
            let mut info = self.branch_info(v);
            info.protected_branches = vec![info.default_branch.clone()];
            out.push(info);
        }
        out.sort_by(|a, b| a.full_name.cmp(&b.full_name));
        Ok(out)
    }

    async fn repo_info(&self, repo: &str) -> Result<RepoInfo> {
        split_repo(repo)?;
        let v = self.get(&format!("/repos/{repo}")).await?;
        let mut info = self.branch_info(&v);
        let mut protected = vec![info.default_branch.clone()];
        // Listing protection needs admin-ish rights on some plans; the default branch is protected regardless.
        match self.pages(&format!("/repos/{repo}/branches?protected=true&per_page=100"), None).await {
            Ok(bs) => {
                for b in bs {
                    let n = s(&b, "/name");
                    if !n.is_empty() && !protected.contains(&n) {
                        protected.push(n);
                    }
                }
            }
            Err(e) => tracing::warn!("listing protected branches of {repo}: {e:#}"),
        }
        info.protected_branches = protected;
        Ok(info)
    }

    async fn list_files(&self, repo: &str, git_ref: &str) -> Result<Vec<String>> {
        split_repo(repo)?;
        let v = self.get(&format!("/repos/{repo}/git/trees/{}?recursive=1", enc_path(git_ref))).await?;
        if v.get("truncated").and_then(Value::as_bool) == Some(true) {
            tracing::warn!("GitHub truncated the file tree of {repo}@{git_ref}");
        }
        Ok(v.get("tree")
            .and_then(Value::as_array)
            .map(|t| t.iter().filter(|e| s(e, "/type") == "blob").map(|e| s(e, "/path")).collect())
            .unwrap_or_default())
    }

    async fn read_file(&self, repo: &str, git_ref: &str, path: &str) -> Result<Option<String>> {
        split_repo(repo)?;
        let url = format!("/repos/{repo}/contents/{}?ref={}", enc_path(path.trim_start_matches('/')), q(git_ref));
        let (st, _, text) = self.send(self.rb_accept(Method::GET, &url, "application/vnd.github.raw+json")).await?;
        if st == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !st.is_success() {
            bail!("GitHub GET {url} failed: {st} {}", snippet(&text));
        }
        Ok(Some(text))
    }

    /// Code search (default branch only, as indexed by GitHub). No line numbers.
    async fn search(&self, repo: &str, query: &str) -> Result<Vec<Value>> {
        split_repo(repo)?;
        let url = format!("/search/code?per_page=100&q={}", q(&format!("{query} repo:{repo}")));
        let v = self.call_accept(&url, "application/vnd.github.text-match+json").await?;
        Ok(v.get("items")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|it| {
                        let text = it.pointer("/text_matches/0/fragment").and_then(Value::as_str).unwrap_or("");
                        json!({"path": s(it, "/path"), "line": Value::Null, "text": text})
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn git_remote(&self, repo: &str) -> Result<GitRemote> {
        split_repo(repo)?;
        use base64::Engine;
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{}", self.token));
        Ok(GitRemote { url: format!("{}/{repo}.git", self.web), auth_header: Some(format!("Basic {basic}")) })
    }

    async fn find_open_pr(&self, repo: &str, head: &str) -> Result<Option<Pr>> {
        let (owner, _) = split_repo(repo)?;
        let v = self.get(&format!("/repos/{repo}/pulls?state=open&per_page=10&head={}", q(&format!("{owner}:{head}")))).await?;
        Ok(v.as_array().and_then(|a| a.first()).map(pr_from))
    }

    async fn open_pr(&self, repo: &str, head: &str, base: &str, title: &str, body: &str) -> Result<Pr> {
        split_repo(repo)?;
        let v = self.call(Method::POST, &format!("/repos/{repo}/pulls"), Some(&json!({"title": title, "head": head, "base": base, "body": body}))).await?;
        Ok(pr_from(&v))
    }

    async fn get_pr(&self, repo: &str, number: i64) -> Result<PrDetail> {
        split_repo(repo)?;
        let p = self.get(&format!("/repos/{repo}/pulls/{number}")).await?;
        let rcs = self.pages(&format!("/repos/{repo}/pulls/{number}/comments?per_page=100"), None).await?;
        let ics = self.pages(&format!("/repos/{repo}/issues/{number}/comments?per_page=100"), None).await?;
        let rvs = self.pages(&format!("/repos/{repo}/pulls/{number}/reviews?per_page=100"), None).await?;
        let states = if rcs.is_empty() {
            HashMap::new()
        } else {
            self.thread_states(repo, number).await.unwrap_or_else(|e| {
                tracing::warn!("reading review thread states of {repo}#{number}: {e:#}");
                HashMap::new()
            })
        };
        Ok(PrDetail {
            pr: pr_from(&p),
            threads: group_threads(&rcs, &states),
            reviews: rvs
                .iter()
                .filter_map(|r| {
                    Some(PrReview { id: oi(r, "/id")?.to_string(), author: s(r, "/user/login"), state: review_state(&s(r, "/state"))?.into(), body: s(r, "/body") })
                })
                .collect(),
            comments: ics.iter().map(comment_from).collect(),
        })
    }

    async fn list_prs(&self, repo: &str, state: &str) -> Result<Vec<Pr>> {
        split_repo(repo)?;
        let api_state = match state {
            "open" => "open",
            "closed" | "merged" => "closed",
            _ => "all",
        };
        let v = self.pages(&format!("/repos/{repo}/pulls?state={api_state}&per_page=100"), None).await?;
        Ok(v.iter().map(pr_from).filter(|p| state == "all" || state == "open" || p.state == state).collect())
    }

    async fn comment(&self, repo: &str, number: i64, body: &str, path: Option<&str>, line: Option<i64>) -> Result<Value> {
        split_repo(repo)?;
        match path {
            None => {
                let v = self.call(Method::POST, &format!("/repos/{repo}/issues/{number}/comments"), Some(&json!({"body": body}))).await?;
                Ok(json!({"comment_id": v.get("id").map(|i| i.to_string()).unwrap_or_default(), "url": s(&v, "/html_url")}))
            }
            Some(path) => {
                let p = self.get(&format!("/repos/{repo}/pulls/{number}")).await?;
                let sha = os(&p, "/head/sha").ok_or_else(|| anyhow!("PR {repo}#{number} has no head commit"))?;
                let mut req = json!({"body": body, "commit_id": sha, "path": path.trim_start_matches('/')});
                match line {
                    Some(l) => {
                        req["line"] = json!(l);
                        req["side"] = json!("RIGHT");
                    }
                    None => req["subject_type"] = json!("file"),
                }
                let v = self.call(Method::POST, &format!("/repos/{repo}/pulls/{number}/comments"), Some(&req)).await?;
                let id = v.get("id").map(|i| i.to_string()).unwrap_or_default();
                Ok(json!({"thread_id": id, "comment_id": id, "url": s(&v, "/html_url")}))
            }
        }
    }

    async fn reply(&self, repo: &str, number: i64, thread_id: &str, body: &str) -> Result<Value> {
        split_repo(repo)?;
        let root: i64 = thread_id.parse().map_err(|_| anyhow!("GitHub thread id must be the root review comment id, got {thread_id}"))?;
        let v = self.call(Method::POST, &format!("/repos/{repo}/pulls/{number}/comments/{root}/replies"), Some(&json!({"body": body}))).await?;
        Ok(json!({"comment_id": v.get("id").map(|i| i.to_string()).unwrap_or_default(), "url": s(&v, "/html_url")}))
    }

    async fn resolve_thread(&self, repo: &str, number: i64, thread_id: &str, status: &str) -> Result<()> {
        let root: i64 = thread_id.parse().map_err(|_| anyhow!("GitHub thread id must be the root review comment id, got {thread_id}"))?;
        match status {
            "resolved" => {}
            "wont_fix" => {
                let rcs = self.pages(&format!("/repos/{repo}/pulls/{number}/comments?per_page=100"), None).await?;
                let noted = rcs.iter().any(|c| {
                    (oi(c, "/id") == Some(root) || oi(c, "/in_reply_to_id") == Some(root)) && s(c, "/body").contains(WONT_FIX_MARKER)
                });
                if !noted {
                    self.reply(repo, number, thread_id, &format!("Won't fix.\n\n{WONT_FIX_MARKER}")).await?;
                }
            }
            s => bail!("thread status must be resolved or wont_fix, got {s}"),
        }
        let states = self.thread_states(repo, number).await?;
        let (node, resolved) = states.get(&root).cloned().ok_or_else(|| anyhow!("review thread {thread_id} not found on {repo}#{number}"))?;
        if resolved {
            return Ok(());
        }
        self.graphql_call("mutation($id:ID!){resolveReviewThread(input:{threadId:$id}){thread{id isResolved}}}", json!({"id": node})).await?;
        Ok(())
    }

    async fn merge(&self, repo: &str, number: i64) -> Result<()> {
        split_repo(repo)?;
        let p = self.get(&format!("/repos/{repo}/pulls/{number}")).await?;
        let mut req = json!({});
        if let Some(sha) = os(&p, "/head/sha") {
            req["sha"] = json!(sha);
        }
        let v = self.call(Method::PUT, &format!("/repos/{repo}/pulls/{number}/merge"), Some(&req)).await?;
        if v.get("merged").and_then(Value::as_bool) == Some(false) {
            bail!("GitHub did not merge {repo}#{number}: {}", s(&v, "/message"));
        }
        Ok(())
    }

    /// Check runs plus legacy commit statuses (id `status:<id>`) for a ref.
    async fn checks(&self, repo: &str, git_ref: &str) -> Result<Vec<CheckRun>> {
        split_repo(repo)?;
        let r = enc_path(git_ref);
        let runs = self.pages(&format!("/repos/{repo}/commits/{r}/check-runs?per_page=100"), Some("check_runs")).await?;
        let mut out: Vec<CheckRun> = runs
            .iter()
            .map(|c| CheckRun {
                id: c.get("id").map(|i| i.to_string()).unwrap_or_default(),
                name: s(c, "/name"),
                status: s(c, "/status"),
                conclusion: os(c, "/conclusion"),
                url: os(c, "/html_url").or_else(|| os(c, "/details_url")),
            })
            .collect();
        if let Some(st) = self.get_opt(&format!("/repos/{repo}/commits/{r}/status")).await? {
            for x in st.get("statuses").and_then(Value::as_array).cloned().unwrap_or_default() {
                let state = s(&x, "/state");
                let (status, conclusion) = match state.as_str() {
                    "pending" => ("in_progress", None),
                    "success" => ("completed", Some("success")),
                    _ => ("completed", Some("failure")),
                };
                out.push(CheckRun {
                    id: format!("status:{}", x.get("id").map(|i| i.to_string()).unwrap_or_default()),
                    name: s(&x, "/context"),
                    status: status.into(),
                    conclusion: conclusion.map(String::from),
                    url: os(&x, "/target_url"),
                });
            }
        }
        Ok(out)
    }

    /// Check-run output and annotations; for GitHub Actions also the tail of the job log.
    async fn check_logs(&self, repo: &str, check_id: &str) -> Result<String> {
        split_repo(repo)?;
        if check_id.starts_with("status:") {
            return Ok("commit statuses have no logs on GitHub; open the status's target URL".into());
        }
        let id: i64 = check_id.parse().map_err(|_| anyhow!("bad GitHub check run id {check_id}"))?;
        let cr = self.get(&format!("/repos/{repo}/check-runs/{id}")).await?;
        let mut out = format!("{}: {} {}\n", s(&cr, "/name"), s(&cr, "/status"), s(&cr, "/conclusion"));
        for k in ["title", "summary", "text"] {
            if let Some(t) = os(&cr, &format!("/output/{k}")) {
                out.push_str(&t);
                out.push('\n');
            }
        }
        if oi(&cr, "/output/annotations_count").unwrap_or(0) > 0 {
            for a in self.pages(&format!("/repos/{repo}/check-runs/{id}/annotations?per_page=100"), None).await? {
                out.push_str(&format!(
                    "{}:{}: {}: {}\n",
                    s(&a, "/path"),
                    oi(&a, "/start_line").unwrap_or(0),
                    s(&a, "/annotation_level"),
                    s(&a, "/message")
                ));
            }
        }
        if s(&cr, "/app/slug") == "github-actions" {
            // Redirects to a signed download URL; reqwest drops the auth header across hosts.
            if let Ok((st, _, log)) = self.send(self.rb(Method::GET, &format!("/repos/{repo}/actions/jobs/{id}/logs"))).await {
                if st.is_success() {
                    let lines: Vec<&str> = log.lines().collect();
                    let tail = &lines[lines.len().saturating_sub(400)..];
                    out.push_str("--- job log (tail) ---\n");
                    out.push_str(&tail.join("\n"));
                }
            }
        }
        Ok(out)
    }

    /// Events since `cursor` (an RFC 3339 time). Without a cursor, starts
    /// from now: history is not replayed.
    async fn poll(&self, repo: &str, cursor: Option<&str>) -> Result<(Vec<RepoEvent>, Option<String>)> {
        split_repo(repo)?;
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

        // Pull requests updated since the cursor, newest first.
        let mut updated = vec![];
        let mut next = Some(format!("/repos/{repo}/pulls?state=all&sort=updated&direction=desc&per_page=50"));
        'pages: for _ in 0..MAX_PAGES {
            let Some(u) = next.take() else { break };
            let (items, n) = self.page(&u, None).await?;
            for p in items {
                if !after(ts(&p, "/updated_at")) {
                    break 'pages;
                }
                updated.push(p);
            }
            next = n;
        }
        for p in &updated {
            let n = oi(p, "/number").unwrap_or(0);
            bump(ts(p, "/updated_at"));
            let opened = after(ts(p, "/created_at"));
            if opened {
                evs.push(opened_event(repo, p));
            } else if s(p, "/state") == "open" {
                evs.extend(pushed_event(repo, p, None));
            }
            if after(ts(p, "/merged_at")) || (s(p, "/state") == "closed" && after(ts(p, "/closed_at"))) {
                evs.push(closed_event(repo, p, None));
            }
            for r in self.pages(&format!("/repos/{repo}/pulls/{n}/reviews?per_page=100"), None).await? {
                if after(ts(&r, "/submitted_at")) {
                    bump(ts(&r, "/submitted_at"));
                    evs.extend(review_event(repo, n, &r));
                }
            }
        }

        for c in self.pages(&format!("/repos/{repo}/pulls/comments?sort=updated&direction=asc&per_page=100&since={}", q(&since_s)), None).await? {
            bump(ts(&c, "/updated_at"));
            if after(ts(&c, "/created_at")) {
                evs.push(review_comment_event(repo, None, &c));
            }
        }
        for c in self.pages(&format!("/repos/{repo}/issues/comments?sort=updated&direction=asc&per_page=100&since={}", q(&since_s)), None).await? {
            bump(ts(&c, "/updated_at"));
            if after(ts(&c, "/created_at")) && s(&c, "/html_url").contains("/pull/") {
                evs.push(issue_comment_event(repo, None, &c));
            }
        }

        // CI does not touch the PR's updated_at: look at the open PRs' heads.
        let (open, _) = self.page(&format!("/repos/{repo}/pulls?state=open&sort=updated&direction=desc&per_page=30"), None).await?;
        for p in &open {
            let Some(sha) = os(p, "/head/sha") else { continue };
            let (runs, _) = self.page(&format!("/repos/{repo}/commits/{sha}/check-runs?per_page=100"), Some("check_runs")).await?;
            for cr in runs {
                if s(&cr, "/status") == "completed" && after(ts(&cr, "/completed_at")) {
                    bump(ts(&cr, "/completed_at"));
                    evs.push(check_run_event(repo, &cr, oi(p, "/number"), os(p, "/head/ref")));
                }
            }
        }
        Ok((evs, Some(max.to_rfc3339_opts(SecondsFormat::Secs, true))))
    }
}

impl GitHub {
    async fn call_accept(&self, url: &str, accept: &str) -> Result<Value> {
        let (st, _, text) = self.send(self.rb_accept(Method::GET, url, accept)).await?;
        if !st.is_success() {
            bail!("GitHub GET {url} failed: {st} {}", snippet(&text));
        }
        Ok(serde_json::from_str(&text)?)
    }
}

// ---------- webhooks ----------

/// `X-Hub-Signature-256: sha256=<hex hmac>` over the raw body.
pub fn verify_signature(secret: &str, headers: &HeaderMap, body: &[u8]) -> bool {
    use hmac::{Hmac, KeyInit, Mac};
    if secret.is_empty() {
        return false;
    }
    let Some(sig) = headers.get("x-hub-signature-256").and_then(|v| v.to_str().ok()) else { return false };
    let Some(tag) = sig.trim().strip_prefix("sha256=").and_then(|h| hex::decode(h).ok()) else { return false };
    let Ok(mut mac) = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(secret.as_bytes()) else { return false };
    mac.update(body);
    mac.verify_slice(&tag).is_ok()
}

/// Webhook delivery → repo events. Unhandled events and actions give no events.
pub fn parse_webhook(headers: &HeaderMap, body: &[u8]) -> Result<Vec<RepoEvent>> {
    let event = headers.get("x-github-event").and_then(|v| v.to_str().ok()).unwrap_or("");
    let delivery = headers.get("x-github-delivery").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let v: Value = serde_json::from_slice(body)?;
    let repo = s(&v, "/repository/full_name");
    let action = s(&v, "/action");
    let sender = os(&v, "/sender/login");
    let mut out: Vec<RepoEvent> = vec![];
    match (event, action.as_str()) {
        ("pull_request", "opened") => out.push(opened_event(&repo, &v["pull_request"])),
        ("pull_request", "reopened") => {
            let p = &v["pull_request"];
            let mut e = ev(String::new(), &repo, oi(p, "/number"), "pr_opened");
            e.author = sender;
            e.branch = os(p, "/head/ref");
            out.push(e);
        }
        ("pull_request", "synchronize") => out.extend(pushed_event(&repo, &v["pull_request"], sender)),
        ("pull_request", "closed") => out.push(closed_event(&repo, &v["pull_request"], sender)),
        ("pull_request_review", "submitted") => {
            if let Some(e) = review_event(&repo, oi(&v, "/pull_request/number").unwrap_or(0), &v["review"]) {
                out.push(e);
            }
        }
        ("pull_request_review_comment", "created") => out.push(review_comment_event(&repo, oi(&v, "/pull_request/number"), &v["comment"])),
        ("issue_comment", "created") if v.pointer("/issue/pull_request").is_some() => {
            out.push(issue_comment_event(&repo, oi(&v, "/issue/number"), &v["comment"]))
        }
        ("check_run", "completed") => {
            let cr = &v["check_run"];
            let prs = cr.get("pull_requests").and_then(Value::as_array).cloned().unwrap_or_default();
            if prs.is_empty() {
                out.push(check_run_event(&repo, cr, None, None));
            }
            for (i, p) in prs.iter().enumerate() {
                let mut e = check_run_event(&repo, cr, oi(p, "/number"), os(p, "/head/ref"));
                if i > 0 {
                    e.id = format!("{}-pr{}", e.id, oi(p, "/number").unwrap_or(0));
                }
                out.push(e);
            }
        }
        ("check_suite", "completed") => {
            let cs = &v["check_suite"];
            let prs = cs.get("pull_requests").and_then(Value::as_array).cloned().unwrap_or_default();
            let mk = |pr: Option<i64>| {
                let mut e = ev(format!("gh-cs-{}-{}", oi(cs, "/id").unwrap_or(0), pr.unwrap_or(0)), &repo, pr, "ci_completed");
                e.author = os(cs, "/app/slug").or(Some("ci".into()));
                e.body = os(cs, "/app/name").map(|n| format!("{n} check suite"));
                e.state = os(cs, "/conclusion");
                e.branch = os(cs, "/head_branch");
                e
            };
            if prs.is_empty() {
                out.push(mk(None));
            }
            for p in &prs {
                out.push(mk(oi(p, "/number")));
            }
        }
        ("push", _) => {
            let after = s(&v, "/after");
            if let Some(branch) = s(&v, "/ref").strip_prefix("refs/heads/") {
                if v.get("deleted").and_then(Value::as_bool) != Some(true) {
                    let mut e = ev(format!("gh-branchpush-{repo}-{branch}-{after}"), &repo, None, "pushed");
                    e.author = os(&v, "/pusher/name").or(sender);
                    e.branch = Some(branch.to_string());
                    out.push(e);
                }
            }
        }
        _ => {}
    }
    for (i, e) in out.iter_mut().enumerate() {
        if e.id.is_empty() {
            if delivery.is_empty() {
                bail!("GitHub webhook has no X-GitHub-Delivery header");
            }
            e.id = format!("gh-{delivery}-{i}");
        }
    }
    Ok(out)
}

// ---------- tests ----------

#[cfg(test)]
pub(crate) mod mock {
    //! A tiny local HTTP server for connector tests: one handler for every
    //! request, and a log of what was received.
    use axum::http::HeaderMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug)]
    pub struct Req {
        pub method: String,
        pub path: String,
        pub query: String,
        pub headers: HeaderMap,
        pub body: String,
    }

    impl Req {
        pub fn header(&self, k: &str) -> String {
            self.headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
        }
        pub fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
        }
    }

    pub struct Resp {
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    pub fn ok(v: serde_json::Value) -> Resp {
        Resp { status: 200, headers: vec![("content-type".into(), "application/json".into())], body: v.to_string() }
    }
    pub fn status(code: u16, body: &str) -> Resp {
        Resp { status: code, headers: vec![], body: body.into() }
    }

    pub type Handler = Arc<dyn Fn(&Req, &str) -> Resp + Send + Sync>;

    pub async fn start(h: Handler) -> (String, Arc<Mutex<Vec<Req>>>) {
        let log: Arc<Mutex<Vec<Req>>> = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (log2, base2) = (log.clone(), base.clone());
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let (log, h, base) = (log2.clone(), h.clone(), base2.clone());
            async move {
                let (parts, body) = req.into_parts();
                let body = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
                let r = Req {
                    method: parts.method.to_string(),
                    path: parts.uri.path().to_string(),
                    query: parts.uri.query().unwrap_or("").to_string(),
                    headers: parts.headers,
                    body: String::from_utf8_lossy(&body).into_owned(),
                };
                log.lock().unwrap().push(r.clone());
                let resp = h(&r, &base);
                let mut b = axum::http::Response::builder().status(resp.status);
                for (k, v) in resp.headers {
                    b = b.header(k, v);
                }
                b.body(axum::body::Body::from(resp.body)).unwrap()
            }
        });
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, log)
    }
}

#[cfg(test)]
mod tests {
    use super::mock::{self, Req, Resp, ok, status};
    use super::*;
    use std::sync::Arc;

    const TOKEN: &str = "ghp_testtoken";

    fn loaded(api: &str) -> Loaded {
        Loaded {
            row: ConnectorRow {
                id: Uuid::nil(),
                kind: "github".into(),
                role: "repo".into(),
                name: "gh".into(),
                config: json!({"api_url": api}),
                service_identity: Some("factory-bot".into()),
            },
            secrets: HashMap::from([("token".to_string(), TOKEN.to_string())]),
        }
    }

    async fn gh(h: impl Fn(&Req, &str) -> Resp + Send + Sync + 'static) -> (GitHub, Arc<std::sync::Mutex<Vec<Req>>>) {
        let (base, log) = mock::start(Arc::new(h)).await;
        (GitHub::from_loaded(&loaded(&base)).unwrap(), log)
    }

    fn pr_json(n: i64, state: &str, merged: bool) -> Value {
        json!({
            "number": n, "html_url": format!("https://github.com/acme/app/pull/{n}"), "title": "Add thing", "body": "desc",
            "state": state, "merged_at": if merged { json!("2026-01-02T00:00:00Z") } else { Value::Null },
            "head": {"ref": "feature/x", "sha": "abc123"}, "base": {"ref": "main"}, "user": {"login": "octo"},
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z", "closed_at": null
        })
    }

    #[test]
    fn from_loaded_defaults_and_requires_token() {
        let mut l = loaded("");
        l.row.config = json!({});
        let g = GitHub::from_loaded(&l).unwrap();
        assert_eq!(g.api, "https://api.github.com");
        assert_eq!(g.graphql, "https://api.github.com/graphql");
        let r = g.git_remote("acme/app").unwrap();
        assert_eq!(r.url, "https://github.com/acme/app.git");
        use base64::Engine;
        let expect = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{TOKEN}"));
        assert_eq!(r.auth_header.unwrap(), format!("Basic {expect}"));
        l.secrets.clear();
        assert!(GitHub::from_loaded(&l).is_err());
    }

    #[test]
    fn ghes_urls() {
        let g = GitHub::new("https://ghe.example.com/api/v3/", None, "t").unwrap();
        assert_eq!(g.graphql, "https://ghe.example.com/api/graphql");
        assert_eq!(g.git_remote("a/b").unwrap().url, "https://ghe.example.com/a/b.git");
        assert!(g.git_remote("nope").is_err());
    }

    #[tokio::test]
    async fn auth_headers_and_repo_info() {
        let (g, log) = gh(|r, _| match r.path.as_str() {
            "/repos/acme/app" => ok(json!({"full_name": "acme/app", "default_branch": "trunk"})),
            "/repos/acme/app/branches" => ok(json!([{"name": "trunk"}, {"name": "release"}])),
            _ => status(404, "{}"),
        })
        .await;
        let info = g.repo_info("acme/app").await.unwrap();
        assert_eq!(info.default_branch, "trunk");
        assert_eq!(info.protected_branches, vec!["trunk", "release"]);
        let log = log.lock().unwrap();
        assert_eq!(log[0].header("authorization"), format!("Bearer {TOKEN}"));
        assert_eq!(log[0].header("x-github-api-version"), API_VERSION);
        assert_eq!(log[0].header("accept"), "application/vnd.github+json");
        assert!(!log[0].header("user-agent").is_empty());
        assert_eq!(log[1].query, "protected=true&per_page=100");
    }

    #[tokio::test]
    async fn list_repos_follows_link_pagination() {
        let (g, log) = gh(|r, base| {
            if r.path == "/user/repos" && !r.query.contains("page=2") {
                Resp {
                    status: 200,
                    headers: vec![("link".into(), format!("<{base}/user/repos?per_page=100&page=2>; rel=\"next\", <{base}/user/repos?per_page=100&page=2>; rel=\"last\""))],
                    body: json!([{"full_name": "acme/b", "default_branch": "main"}]).to_string(),
                }
            } else if r.path == "/user/repos" {
                ok(json!([{"full_name": "acme/a", "default_branch": "dev"}, {"full_name": "acme/old", "archived": true}]))
            } else {
                status(404, "")
            }
        })
        .await;
        let repos = g.list_repos().await.unwrap();
        assert_eq!(repos.iter().map(|r| r.full_name.as_str()).collect::<Vec<_>>(), vec!["acme/a", "acme/b"]);
        assert_eq!(repos[0].protected_branches, vec!["dev"]);
        assert_eq!(log.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn installation_token_lists_installation_repos() {
        let (base, log) = mock::start(Arc::new(|r: &Req, _: &str| {
            if r.path == "/installation/repositories" {
                ok(json!({"total_count": 1, "repositories": [{"full_name": "acme/a", "default_branch": "main"}]}))
            } else {
                status(404, "")
            }
        }))
        .await;
        let g = GitHub::new(&base, None, "ghs_installation").unwrap();
        assert_eq!(g.list_repos().await.unwrap()[0].full_name, "acme/a");
        assert_eq!(log.lock().unwrap()[0].path, "/installation/repositories");
    }

    #[tokio::test]
    async fn files_read_and_search() {
        let (g, log) = gh(|r, _| match r.path.as_str() {
            "/repos/acme/app/git/trees/feature/x" => ok(json!({"tree": [{"path": "src", "type": "tree"}, {"path": "src/a.rs", "type": "blob"}], "truncated": false})),
            "/repos/acme/app/contents/src/a%20b.rs" => Resp { status: 200, headers: vec![], body: "fn main() {}".into() },
            "/search/code" => ok(json!({"items": [{"path": "src/a.rs", "text_matches": [{"fragment": "fn needle()"}]}]})),
            _ => status(404, "{\"message\":\"Not Found\"}"),
        })
        .await;
        assert_eq!(g.list_files("acme/app", "feature/x").await.unwrap(), vec!["src/a.rs"]);
        assert_eq!(g.read_file("acme/app", "main", "src/a b.rs").await.unwrap().as_deref(), Some("fn main() {}"));
        assert_eq!(g.read_file("acme/app", "main", "missing.rs").await.unwrap(), None);
        let hits = g.search("acme/app", "needle").await.unwrap();
        assert_eq!(hits[0]["path"], "src/a.rs");
        assert_eq!(hits[0]["text"], "fn needle()");
        let log = log.lock().unwrap();
        assert_eq!(log[0].query, "recursive=1");
        assert_eq!(log[1].query, "ref=main");
        assert_eq!(log[1].header("accept"), "application/vnd.github.raw+json");
        assert_eq!(log[3].query, "per_page=100&q=needle+repo%3Aacme%2Fapp");
        assert_eq!(log[3].header("accept"), "application/vnd.github.text-match+json");
    }

    #[tokio::test]
    async fn prs_open_find_list() {
        let (g, log) = gh(|r, _| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/repos/acme/app/pulls") if r.query.contains("head=") => ok(json!([pr_json(7, "open", false)])),
            ("GET", "/repos/acme/app/pulls") => ok(json!([pr_json(7, "open", false), pr_json(8, "closed", true), pr_json(9, "closed", false)])),
            ("POST", "/repos/acme/app/pulls") => ok(pr_json(10, "open", false)),
            _ => status(404, ""),
        })
        .await;
        let p = g.find_open_pr("acme/app", "feature/x").await.unwrap().unwrap();
        assert_eq!((p.number, p.head.as_str(), p.base.as_str(), p.head_sha.as_deref()), (7, "feature/x", "main", Some("abc123")));
        let p = g.open_pr("acme/app", "feature/x", "main", "T", "B <!-- m -->").await.unwrap();
        assert_eq!(p.number, 10);
        let merged = g.list_prs("acme/app", "merged").await.unwrap();
        assert_eq!(merged.iter().map(|p| p.number).collect::<Vec<_>>(), vec![8]);
        let closed = g.list_prs("acme/app", "closed").await.unwrap();
        assert_eq!(closed.iter().map(|p| p.number).collect::<Vec<_>>(), vec![9]);
        let log = log.lock().unwrap();
        assert_eq!(log[0].query, "state=open&per_page=10&head=acme%3Afeature%2Fx");
        assert_eq!(log[1].json(), json!({"title": "T", "head": "feature/x", "base": "main", "body": "B <!-- m -->"}));
        assert!(log[2].query.starts_with("state=closed"));
    }

    fn review_comments() -> Value {
        json!([
            {"id": 100, "path": "src/a.rs", "line": 4, "user": {"login": "alice"}, "body": "rename this", "created_at": "2026-01-03T00:00:00Z"},
            {"id": 101, "in_reply_to_id": 100, "path": "src/a.rs", "line": 4, "user": {"login": "factory-bot"}, "body": "done <!-- factory:x -->"},
            {"id": 200, "path": "src/b.rs", "line": null, "original_line": 9, "user": {"login": "bob"}, "body": "why?"},
            {"id": 102, "in_reply_to_id": 101, "path": "src/a.rs", "user": {"login": "alice"}, "body": "thanks"},
            {"id": 300, "path": "src/c.rs", "line": 1, "user": {"login": "bob"}, "body": "nope"},
            {"id": 301, "in_reply_to_id": 300, "user": {"login": "factory-bot"}, "body": format!("Won't fix.\n\n{WONT_FIX_MARKER}")}
        ])
    }

    #[test]
    fn thread_grouping() {
        let rc = review_comments();
        let states = HashMap::from([(100, ("T100".to_string(), true)), (200, ("T200".to_string(), false)), (300, ("T300".to_string(), true))]);
        let th = group_threads(rc.as_array().unwrap(), &states);
        assert_eq!(th.len(), 3);
        assert_eq!(th[0].id, "100");
        assert_eq!(th[0].comments.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["100", "101", "102"]);
        assert_eq!(th[0].status.as_deref(), Some("resolved"));
        assert_eq!((th[1].id.as_str(), th[1].line, th[1].status.as_deref()), ("200", Some(9), None));
        assert_eq!(th[2].status.as_deref(), Some("wont_fix"));
    }

    fn pr_handler(r: &Req, _: &str) -> Resp {
        match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/repos/acme/app/pulls/7") => ok(pr_json(7, "open", false)),
            ("GET", "/repos/acme/app/pulls/7/comments") => ok(review_comments()),
            ("GET", "/repos/acme/app/issues/7/comments") => {
                ok(json!([{"id": 55, "user": {"login": "factory-bot"}, "body": "status\n\n<!-- factory:abc -->", "created_at": "2026-01-01T00:00:00Z"}]))
            }
            ("GET", "/repos/acme/app/pulls/7/reviews") => ok(json!([
                {"id": 1, "user": {"login": "alice"}, "state": "CHANGES_REQUESTED", "body": "fix"},
                {"id": 2, "user": {"login": "alice"}, "state": "APPROVED", "body": ""},
                {"id": 3, "user": {"login": "carol"}, "state": "PENDING", "body": ""}
            ])),
            ("POST", "/graphql") => {
                let q = r.json();
                if q["query"].as_str().unwrap_or("").contains("resolveReviewThread") {
                    ok(json!({"data": {"resolveReviewThread": {"thread": {"id": q["variables"]["id"], "isResolved": true}}}}))
                } else {
                    ok(json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
                        "pageInfo": {"hasNextPage": false, "endCursor": null},
                        "nodes": [
                            {"id": "PRRT_100", "isResolved": false, "comments": {"nodes": [{"databaseId": 100}]}},
                            {"id": "PRRT_200", "isResolved": true, "comments": {"nodes": [{"databaseId": 200}]}}
                        ]}}}}}))
                }
            }
            ("POST", "/repos/acme/app/issues/7/comments") => ok(json!({"id": 56, "html_url": "u"})),
            ("POST", "/repos/acme/app/pulls/7/comments") => ok(json!({"id": 400, "html_url": "u"})),
            ("POST", p) if p.ends_with("/replies") => ok(json!({"id": 401})),
            ("PUT", "/repos/acme/app/pulls/7/merge") => ok(json!({"merged": true, "sha": "m"})),
            _ => status(404, ""),
        }
    }

    #[tokio::test]
    async fn get_pr_maps_threads_reviews_and_marker_comments() {
        let (g, log) = gh(pr_handler).await;
        let d = g.get_pr("acme/app", 7).await.unwrap();
        assert_eq!(d.pr.number, 7);
        assert_eq!(d.threads.len(), 3);
        assert_eq!(d.threads[0].status, None);
        assert_eq!(d.threads[1].status.as_deref(), Some("resolved"));
        assert_eq!(d.reviews.len(), 2);
        assert_eq!(d.review_decision(), "approved");
        // Marker search, as the primitives do it.
        assert_eq!(d.comments.iter().find(|c| c.body.contains("<!-- factory:abc -->")).unwrap().id, "55");
        assert_eq!(d.threads.iter().find(|t| t.comments.iter().any(|c| c.body.contains("<!-- factory:x -->"))).unwrap().id, "100");
        let log = log.lock().unwrap();
        let gql = log.iter().find(|r| r.path == "/graphql").unwrap();
        assert_eq!(gql.json()["variables"], json!({"owner": "acme", "name": "app", "number": 7, "after": null}));
        assert_eq!(gql.header("authorization"), format!("Bearer {TOKEN}"));
    }

    #[tokio::test]
    async fn comment_reply_resolve_merge() {
        let (g, log) = gh(pr_handler).await;
        assert_eq!(g.comment("acme/app", 7, "hi", None, None).await.unwrap()["comment_id"], "56");
        let v = g.comment("acme/app", 7, "inline", Some("src/a.rs"), Some(12)).await.unwrap();
        assert_eq!(v["thread_id"], "400");
        assert_eq!(g.reply("acme/app", 7, "100", "ok").await.unwrap()["comment_id"], "401");
        g.resolve_thread("acme/app", 7, "100", "resolved").await.unwrap();
        // 300 already carries the won't-fix note: no second reply. It is unknown to GraphQL here.
        assert!(g.resolve_thread("acme/app", 7, "300", "wont_fix").await.is_err());
        assert!(g.resolve_thread("acme/app", 7, "100", "bogus").await.is_err());
        g.merge("acme/app", 7).await.unwrap();
        let log = log.lock().unwrap();
        let inline = log.iter().find(|r| r.method == "POST" && r.path == "/repos/acme/app/pulls/7/comments").unwrap();
        assert_eq!(inline.json(), json!({"body": "inline", "commit_id": "abc123", "path": "src/a.rs", "line": 12, "side": "RIGHT"}));
        assert!(log.iter().any(|r| r.path == "/repos/acme/app/pulls/7/comments/100/replies" && r.json() == json!({"body": "ok"})));
        let resolve = log.iter().find(|r| r.body.contains("resolveReviewThread")).unwrap();
        assert_eq!(resolve.json()["variables"]["id"], "PRRT_100");
        assert_eq!(log.iter().filter(|r| r.path.ends_with("/replies")).count(), 1);
        let merge = log.iter().find(|r| r.method == "PUT").unwrap();
        assert_eq!(merge.json(), json!({"sha": "abc123"}));
    }

    #[tokio::test]
    async fn wont_fix_replies_then_resolves() {
        let (g, log) = gh(pr_handler).await;
        g.resolve_thread("acme/app", 7, "200", "wont_fix").await.unwrap();
        let log = log.lock().unwrap();
        let reply = log.iter().find(|r| r.path == "/repos/acme/app/pulls/7/comments/200/replies").unwrap();
        assert!(reply.json()["body"].as_str().unwrap().contains(WONT_FIX_MARKER));
        // 200 is already resolved in GraphQL: no mutation.
        assert!(!log.iter().any(|r| r.body.contains("resolveReviewThread")));
    }

    #[tokio::test]
    async fn checks_and_logs() {
        let (g, log) = gh(|r, _| match r.path.as_str() {
            "/repos/acme/app/commits/abc123/check-runs" => ok(json!({"total_count": 1, "check_runs": [
                {"id": 9, "name": "build", "status": "completed", "conclusion": "failure", "html_url": "h"}]})),
            "/repos/acme/app/commits/abc123/status" => ok(json!({"statuses": [{"id": 5, "context": "jenkins", "state": "pending", "target_url": "j"}]})),
            "/repos/acme/app/check-runs/9" => ok(json!({"id": 9, "name": "build", "status": "completed", "conclusion": "failure",
                "app": {"slug": "github-actions"}, "output": {"title": "1 error", "summary": "compile failed", "text": null, "annotations_count": 1}})),
            "/repos/acme/app/check-runs/9/annotations" => ok(json!([{"path": "src/a.rs", "start_line": 3, "annotation_level": "failure", "message": "E0308"}])),
            "/repos/acme/app/actions/jobs/9/logs" => Resp { status: 200, headers: vec![], body: "step 1\nerror: boom".into() },
            _ => status(404, ""),
        })
        .await;
        let cs = g.checks("acme/app", "abc123").await.unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!((cs[0].id.as_str(), cs[0].conclusion.as_deref()), ("9", Some("failure")));
        assert_eq!((cs[1].id.as_str(), cs[1].status.as_str(), cs[1].conclusion.as_deref()), ("status:5", "in_progress", None));
        let logs = g.check_logs("acme/app", "9").await.unwrap();
        assert!(logs.contains("compile failed"));
        assert!(logs.contains("src/a.rs:3: failure: E0308"));
        assert!(logs.contains("error: boom"));
        assert_eq!(log.lock().unwrap()[0].query, "per_page=100");
    }

    #[tokio::test]
    async fn poll_without_cursor_starts_now() {
        let (g, log) = gh(|_, _| status(500, "")).await;
        let (evs, next) = g.poll("acme/app", None).await.unwrap();
        assert!(evs.is_empty());
        assert!(DateTime::parse_from_rfc3339(&next.unwrap()).is_ok());
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn poll_produces_events() {
        let (g, log) = gh(|r, _| match r.path.as_str() {
            "/repos/acme/app/pulls" if r.query.starts_with("state=all") => ok(json!([
                {"number": 3, "state": "open", "user": {"login": "dev"}, "head": {"ref": "f3", "sha": "s3"}, "base": {"ref": "main"},
                 "created_at": "2026-02-01T10:00:00Z", "updated_at": "2026-02-01T12:00:00Z", "merged_at": null, "closed_at": null},
                {"number": 2, "state": "open", "user": {"login": "dev"}, "head": {"ref": "f2", "sha": "s2"}, "base": {"ref": "main"},
                 "created_at": "2026-01-01T10:00:00Z", "updated_at": "2026-02-01T11:00:00Z", "merged_at": null, "closed_at": null},
                {"number": 1, "state": "closed", "user": {"login": "dev"}, "head": {"ref": "f1", "sha": "s1"}, "base": {"ref": "main"},
                 "created_at": "2026-01-01T10:00:00Z", "updated_at": "2026-02-01T10:30:00Z", "merged_at": "2026-02-01T10:30:00Z", "closed_at": "2026-02-01T10:30:00Z"},
                {"number": 0, "state": "open", "updated_at": "2025-01-01T00:00:00Z"}
            ])),
            "/repos/acme/app/pulls" => ok(json!([{"number": 3, "head": {"ref": "f3", "sha": "s3"}}])),
            "/repos/acme/app/pulls/2/reviews" => ok(json!([
                {"id": 70, "user": {"login": "alice"}, "state": "CHANGES_REQUESTED", "body": "no", "submitted_at": "2026-02-01T11:00:00Z"},
                {"id": 69, "user": {"login": "alice"}, "state": "COMMENTED", "body": "", "submitted_at": "2026-02-01T11:00:00Z"},
                {"id": 60, "user": {"login": "alice"}, "state": "APPROVED", "body": "", "submitted_at": "2025-12-01T00:00:00Z"}
            ])),
            "/repos/acme/app/pulls/comments" => ok(json!([
                {"id": 500, "pull_request_url": "https://api.github.com/repos/acme/app/pulls/2", "user": {"login": "alice"}, "body": "x",
                 "created_at": "2026-02-01T11:00:00Z", "updated_at": "2026-02-01T11:00:00Z"},
                {"id": 501, "in_reply_to_id": 500, "pull_request_url": "https://api.github.com/repos/acme/app/pulls/2", "user": {"login": "bob"}, "body": "y",
                 "created_at": "2026-02-01T11:05:00Z", "updated_at": "2026-02-01T11:05:00Z"},
                {"id": 400, "pull_request_url": "https://api.github.com/repos/acme/app/pulls/2", "user": {"login": "bob"}, "body": "edited only",
                 "created_at": "2025-01-01T00:00:00Z", "updated_at": "2026-02-01T11:06:00Z"}
            ])),
            "/repos/acme/app/issues/comments" => ok(json!([
                {"id": 900, "html_url": "https://github.com/acme/app/pull/2#issuecomment-900", "issue_url": "https://api.github.com/repos/acme/app/issues/2",
                 "user": {"login": "carol"}, "body": "general", "created_at": "2026-02-01T11:10:00Z", "updated_at": "2026-02-01T11:10:00Z"},
                {"id": 901, "html_url": "https://github.com/acme/app/issues/5#issuecomment-901", "issue_url": "https://api.github.com/repos/acme/app/issues/5",
                 "user": {"login": "carol"}, "body": "issue", "created_at": "2026-02-01T11:10:00Z", "updated_at": "2026-02-01T11:10:00Z"}
            ])),
            "/repos/acme/app/commits/s3/check-runs" => ok(json!({"check_runs": [
                {"id": 77, "name": "test", "status": "completed", "conclusion": "failure", "completed_at": "2026-02-01T13:00:00Z", "app": {"slug": "github-actions"}},
                {"id": 78, "name": "lint", "status": "in_progress", "conclusion": null}
            ]})),
            _ => ok(json!([])),
        })
        .await;
        let (evs, next) = g.poll("acme/app", Some("2026-02-01T00:00:00Z")).await.unwrap();
        let got: Vec<(String, String, Option<i64>)> = evs.iter().map(|e| (e.kind.clone(), e.id.clone(), e.pr_number)).collect();
        let want = vec![
            ("pr_opened", "opened-acme/app-3", Some(3)),
            ("pushed", "push-acme/app-s2", Some(2)),
            ("review", "gh-review-70", Some(2)),
            ("merged", "merged-acme/app-1", Some(1)),
            ("comment", "gh-rc-500", Some(2)),
            ("thread_reply", "gh-rc-501", Some(2)),
            ("comment", "gh-ic-900", Some(2)),
            ("ci_completed", "gh-ci-77", Some(3)),
        ];
        assert_eq!(got, want.into_iter().map(|(a, b, c)| (a.to_string(), b.to_string(), c)).collect::<Vec<_>>());
        let reply = evs.iter().find(|e| e.id == "gh-rc-501").unwrap();
        assert_eq!(reply.thread_id.as_deref(), Some("500"));
        assert_eq!(evs.iter().find(|e| e.kind == "review").unwrap().state.as_deref(), Some("changes_requested"));
        assert_eq!(evs.iter().find(|e| e.kind == "ci_completed").unwrap().state.as_deref(), Some("failure"));
        assert_eq!(next.as_deref(), Some("2026-02-01T13:00:00Z"));
        let log = log.lock().unwrap();
        assert!(log.iter().any(|r| r.path == "/repos/acme/app/pulls/comments" && r.query.contains("since=2026-02-01T00%3A00%3A00Z")));
    }

    // ---- webhooks ----

    fn headers(event: &str, delivery: &str, sig: Option<String>) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-github-event", event.parse().unwrap());
        h.insert("x-github-delivery", delivery.parse().unwrap());
        if let Some(s) = sig {
            h.insert("x-hub-signature-256", s.parse().unwrap());
        }
        h
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        use hmac::{Hmac, KeyInit, Mac};
        let mut m = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(secret.as_bytes()).unwrap();
        m.update(body);
        format!("sha256={}", hex::encode(m.finalize().into_bytes()))
    }

    #[test]
    fn signature_valid_and_invalid() {
        // Example from GitHub's "Validating webhook deliveries" docs.
        let h = headers("ping", "d", Some("sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17".into()));
        assert!(verify_signature("It's a Secret to Everybody", &h, b"Hello, World!"));
        assert!(!verify_signature("It's a Secret to Everybody", &h, b"Hello, World?"));
        assert!(!verify_signature("other", &h, b"Hello, World!"));
        assert!(!verify_signature("", &h, b"Hello, World!"));
        assert!(!verify_signature("s", &headers("ping", "d", None), b"x"));
        assert!(!verify_signature("s", &headers("ping", "d", Some("sha1=abc".into())), b"x"));
        let body = br#"{"a":1}"#;
        assert!(verify_signature("s3cret", &headers("push", "d", Some(sign("s3cret", body))), body));
    }

    fn repo_obj() -> Value {
        json!({"id": 1, "name": "app", "full_name": "acme/app", "owner": {"login": "acme"}})
    }

    fn hook(event: &str, payload: Value) -> Vec<RepoEvent> {
        parse_webhook(&headers(event, "72d3162e-cc78-11e3-81ab-4c9367dc0958", None), payload.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn webhook_pull_request_events() {
        let pr = json!({"number": 7, "state": "open", "user": {"login": "dev"}, "head": {"ref": "feature/x", "sha": "abc123"}, "base": {"ref": "main"},
            "merged": false, "merged_at": null, "closed_at": null});
        let e = hook("pull_request", json!({"action": "opened", "number": 7, "pull_request": pr, "repository": repo_obj(), "sender": {"login": "dev"}}));
        assert_eq!((e[0].kind.as_str(), e[0].id.as_str(), e[0].branch.as_deref()), ("pr_opened", "opened-acme/app-7", Some("feature/x")));
        let e = hook("pull_request", json!({"action": "synchronize", "before": "000", "after": "abc123", "pull_request": pr, "repository": repo_obj(), "sender": {"login": "dev"}}));
        assert_eq!((e[0].kind.as_str(), e[0].id.as_str(), e[0].pr_number), ("pushed", "push-acme/app-abc123", Some(7)));
        let e = hook("pull_request", json!({"action": "reopened", "pull_request": pr, "repository": repo_obj(), "sender": {"login": "dev"}}));
        assert_eq!(e[0].id, "gh-72d3162e-cc78-11e3-81ab-4c9367dc0958-0");
        let mut merged = pr.clone();
        merged["state"] = json!("closed");
        merged["merged"] = json!(true);
        merged["merged_at"] = json!("2026-01-02T00:00:00Z");
        let e = hook("pull_request", json!({"action": "closed", "pull_request": merged, "repository": repo_obj(), "sender": {"login": "lead"}}));
        assert_eq!((e[0].kind.as_str(), e[0].id.as_str(), e[0].author.as_deref()), ("merged", "merged-acme/app-7", Some("lead")));
        let mut closed = pr.clone();
        closed["state"] = json!("closed");
        closed["closed_at"] = json!("2026-01-02T00:00:00Z");
        let e = hook("pull_request", json!({"action": "closed", "pull_request": closed, "repository": repo_obj(), "sender": {"login": "lead"}}));
        assert_eq!((e[0].kind.as_str(), e[0].id.as_str()), ("closed", "closed-acme/app-7-2026-01-02T00:00:00Z"));
        assert!(hook("pull_request", json!({"action": "labeled", "pull_request": pr, "repository": repo_obj()})).is_empty());
    }

    #[test]
    fn webhook_review_and_comment_events() {
        let pr = json!({"number": 7, "head": {"ref": "feature/x", "sha": "abc123"}});
        let e = hook("pull_request_review", json!({"action": "submitted", "pull_request": pr, "repository": repo_obj(),
            "review": {"id": 80, "user": {"login": "alice"}, "body": "LGTM", "state": "approved", "submitted_at": "2026-01-01T00:00:00Z"}}));
        assert_eq!((e[0].kind.as_str(), e[0].id.as_str(), e[0].state.as_deref(), e[0].pr_number), ("review", "gh-review-80", Some("approved"), Some(7)));
        let e = hook("pull_request_review_comment", json!({"action": "created", "pull_request": pr, "repository": repo_obj(),
            "comment": {"id": 501, "in_reply_to_id": 500, "user": {"login": "bob"}, "body": "reply", "path": "a.rs", "line": 3}}));
        assert_eq!((e[0].kind.as_str(), e[0].thread_id.as_deref(), e[0].id.as_str()), ("thread_reply", Some("500"), "gh-rc-501"));
        let e = hook("pull_request_review_comment", json!({"action": "created", "pull_request": pr, "repository": repo_obj(),
            "comment": {"id": 500, "user": {"login": "bob"}, "body": "root", "path": "a.rs", "line": 3}}));
        assert_eq!((e[0].kind.as_str(), e[0].thread_id.as_deref()), ("comment", Some("500")));
        let e = hook("issue_comment", json!({"action": "created", "repository": repo_obj(),
            "issue": {"number": 7, "pull_request": {"url": "https://api.github.com/repos/acme/app/pulls/7"}},
            "comment": {"id": 900, "user": {"login": "carol"}, "body": "hello"}}));
        assert_eq!((e[0].kind.as_str(), e[0].id.as_str(), e[0].pr_number, e[0].body.as_deref()), ("comment", "gh-ic-900", Some(7), Some("hello")));
        assert!(hook("issue_comment", json!({"action": "created", "repository": repo_obj(), "issue": {"number": 8}, "comment": {"id": 1}})).is_empty());
    }

    #[test]
    fn webhook_ci_and_push_events() {
        let e = hook("check_run", json!({"action": "completed", "repository": repo_obj(), "check_run": {
            "id": 77, "name": "test", "status": "completed", "conclusion": "failure", "app": {"slug": "github-actions"},
            "check_suite": {"head_branch": "feature/x"},
            "pull_requests": [{"number": 7, "head": {"ref": "feature/x", "sha": "abc"}, "base": {"ref": "main"}}]}}));
        assert_eq!((e[0].kind.as_str(), e[0].id.as_str(), e[0].pr_number, e[0].state.as_deref(), e[0].body.as_deref()),
            ("ci_completed", "gh-ci-77", Some(7), Some("failure"), Some("test")));
        assert!(hook("check_run", json!({"action": "created", "repository": repo_obj(), "check_run": {"id": 1}})).is_empty());
        let e = hook("check_suite", json!({"action": "completed", "repository": repo_obj(), "check_suite": {
            "id": 5, "head_branch": "feature/x", "conclusion": "success", "app": {"slug": "ci", "name": "CI"}, "pull_requests": [{"number": 7}]}}));
        assert_eq!((e[0].id.as_str(), e[0].state.as_deref()), ("gh-cs-5-7", Some("success")));
        let e = hook("push", json!({"ref": "refs/heads/feature/x", "before": "a", "after": "b", "deleted": false, "repository": repo_obj(),
            "pusher": {"name": "dev"}, "sender": {"login": "dev"}}));
        assert_eq!((e[0].kind.as_str(), e[0].branch.as_deref(), e[0].pr_number), ("pushed", Some("feature/x"), None));
        assert!(hook("push", json!({"ref": "refs/tags/v1", "after": "b", "repository": repo_obj()})).is_empty());
        assert!(hook("ping", json!({"zen": "hi", "hook_id": 1})).is_empty());
    }

    // ---- contract tests against the real service ----

    fn contract() -> Option<(GitHub, String)> {
        let token = std::env::var("FACTORY_CONTRACT_GITHUB_TOKEN").ok()?;
        let repo = std::env::var("FACTORY_CONTRACT_GITHUB_REPO").ok()?;
        let api = std::env::var("FACTORY_CONTRACT_GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".into());
        Some((GitHub::new(&api, None, &token).unwrap(), repo))
    }

    #[tokio::test]
    #[ignore]
    async fn contract_read_only() {
        let Some((g, repo)) = contract() else {
            eprintln!("set FACTORY_CONTRACT_GITHUB_TOKEN and FACTORY_CONTRACT_GITHUB_REPO to run");
            return;
        };
        println!("{}", g.test_connection().await.unwrap());
        let info = g.repo_info(&repo).await.unwrap();
        assert_eq!(info.full_name.to_lowercase(), repo.to_lowercase());
        let files = g.list_files(&repo, &info.default_branch).await.unwrap();
        assert!(!files.is_empty());
        assert!(g.read_file(&repo, &info.default_branch, &files[0]).await.unwrap().is_some());
        assert!(g.read_file(&repo, &info.default_branch, "definitely/not/here.txt").await.unwrap().is_none());
        let _ = g.checks(&repo, &info.default_branch).await.unwrap();
        let prs = g.list_prs(&repo, "all").await.unwrap();
        if let Some(p) = prs.first() {
            let d = g.get_pr(&repo, p.number).await.unwrap();
            assert_eq!(d.pr.number, p.number);
        }
        let since = (Utc::now() - chrono::Duration::days(7)).to_rfc3339_opts(SecondsFormat::Secs, true);
        let (_evs, next) = g.poll(&repo, Some(&since)).await.unwrap();
        assert!(next.is_some());
    }
}
