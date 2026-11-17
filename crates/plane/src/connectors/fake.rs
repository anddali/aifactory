//! Fake connectors with the same interfaces as the real ones. State lives in
//! Postgres (`fake_store`) so that every plane replica sees the same fakes;
//! the fake repo host is backed by real local bare git repositories.

use super::*;
use crate::gitutil::git;
use sqlx::PgPool;
use std::path::PathBuf;

// ---------- fake_store helpers ----------

pub async fn store_get<T: serde::de::DeserializeOwned>(pool: &PgPool, ns: &str, id: &str) -> Result<Option<T>> {
    let v: Option<Value> = sqlx::query_scalar("SELECT data FROM fake_store WHERE ns=$1 AND id=$2")
        .bind(ns)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(match v {
        Some(v) => Some(serde_json::from_value(v)?),
        None => None,
    })
}

pub async fn store_put(pool: &PgPool, ns: &str, id: &str, data: &impl Serialize) -> Result<()> {
    sqlx::query(
        "INSERT INTO fake_store(ns,id,data) VALUES ($1,$2,$3) ON CONFLICT (ns,id) DO UPDATE SET data=EXCLUDED.data",
    )
    .bind(ns)
    .bind(id)
    .bind(serde_json::to_value(data)?)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn store_list<T: serde::de::DeserializeOwned>(pool: &PgPool, ns: &str, after_seq: i64) -> Result<Vec<(i64, T)>> {
    let rows: Vec<(i64, Value)> =
        sqlx::query_as("SELECT seq, data FROM fake_store WHERE ns=$1 AND seq > $2 ORDER BY seq ASC")
            .bind(ns)
            .bind(after_seq)
            .fetch_all(pool)
            .await?;
    rows.into_iter()
        .map(|(s, v)| Ok((s, serde_json::from_value(v)?)))
        .collect()
}

/// Read-modify-write one document under a row lock.
pub async fn store_update<T, F, R>(pool: &PgPool, ns: &str, id: &str, f: F) -> Result<R>
where
    T: serde::de::DeserializeOwned + Serialize,
    F: FnOnce(&mut T) -> Result<R>,
{
    let mut tx = pool.begin().await?;
    let v: Option<Value> = sqlx::query_scalar("SELECT data FROM fake_store WHERE ns=$1 AND id=$2 FOR UPDATE")
        .bind(ns)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(v) = v else { bail!("{ns}/{id} not found") };
    let mut doc: T = serde_json::from_value(v)?;
    let r = f(&mut doc)?;
    sqlx::query("UPDATE fake_store SET data=$3 WHERE ns=$1 AND id=$2")
        .bind(ns)
        .bind(id)
        .bind(serde_json::to_value(&doc)?)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(r)
}

fn new_id() -> String {
    Uuid::now_v7().simple().to_string()
}

/// Record a fake external event so pollers can reconcile it later.
pub async fn record_event(pool: &PgPool, connector: Uuid, ev: &impl Serialize, id: &str) -> Result<()> {
    store_put(pool, &format!("fake_events:{connector}"), id, ev).await
}

// ---------- tickets ----------

pub struct FakeTickets {
    pool: PgPool,
    id: Uuid,
}

impl FakeTickets {
    pub fn new(pool: PgPool, id: Uuid) -> Self {
        FakeTickets { pool, id }
    }
    fn ns(&self) -> String {
        format!("fake_ticket:{}", self.id)
    }
    pub async fn create(&self, t: &Ticket) -> Result<()> {
        store_put(&self.pool, &self.ns(), &t.key, t).await
    }
}

#[async_trait]
impl TicketSource for FakeTickets {
    async fn test_connection(&self) -> Result<String> {
        Ok("fake ticket source is reachable".into())
    }
    async fn search(&self, query: &str) -> Result<Vec<TicketRef>> {
        let all: Vec<(i64, Ticket)> = store_list(&self.pool, &self.ns(), 0).await?;
        let q = query.to_lowercase();
        Ok(all
            .into_iter()
            .map(|(_, t)| t)
            .filter(|t| q.is_empty() || t.title.to_lowercase().contains(&q) || t.key.to_lowercase() == q || t.description.to_lowercase().contains(&q))
            .map(|t| TicketRef { key: t.key, title: t.title, status: t.status })
            .collect())
    }
    async fn get(&self, key: &str) -> Result<Ticket> {
        // Like Jira, keys are case-insensitive on lookup and the stored key is returned.
        if let Some(t) = store_get::<Ticket>(&self.pool, &self.ns(), key).await? {
            return Ok(t);
        }
        store_get(&self.pool, &self.ns(), &key.to_uppercase())
            .await?
            .ok_or_else(|| anyhow!("ticket {key} not found"))
    }
    async fn transition(&self, key: &str, to: &str) -> Result<()> {
        store_update(&self.pool, &self.ns(), key, |t: &mut Ticket| {
            t.status = to.to_string();
            Ok(())
        })
        .await
    }
    async fn comment(&self, key: &str, body: &str, marker: &str) -> Result<Comment> {
        let c = Comment {
            id: new_id(),
            author: "factory-bot".into(),
            body: format!("{body}\n\n<!-- {marker} -->"),
            created_at: Some(chrono::Utc::now().to_rfc3339()),
        };
        let c2 = c.clone();
        store_update(&self.pool, &self.ns(), key, move |t: &mut Ticket| {
            t.comments.push(c2);
            Ok(())
        })
        .await?;
        Ok(c)
    }
    async fn find_comment(&self, key: &str, marker: &str) -> Result<Option<Comment>> {
        let t = self.get(key).await?;
        Ok(t.comments.into_iter().find(|c| c.body.contains(marker)))
    }
    async fn poll(&self, cursor: Option<&str>) -> Result<(Vec<TicketEvent>, Option<String>)> {
        let after: i64 = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
        let evs: Vec<(i64, TicketEvent)> = store_list(&self.pool, &format!("fake_events:{}", self.id), after).await?;
        let next = evs.last().map(|(s, _)| s.to_string()).or(cursor.map(String::from));
        Ok((evs.into_iter().map(|(_, e)| e).collect(), next))
    }
}

// ---------- repo host ----------

pub struct FakeRepo {
    pool: PgPool,
    id: Uuid,
    root: PathBuf,
    extra_protected: Vec<String>,
    identity: String,
}

impl FakeRepo {
    pub fn from_loaded(pool: PgPool, l: &Loaded) -> Result<Self> {
        let root = l.cfg("root").ok_or_else(|| anyhow!("fake_repo connector needs config.root"))?;
        let extra_protected = l
            .row
            .config
            .get("protected")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        Ok(FakeRepo {
            pool,
            id: l.row.id,
            root: PathBuf::from(root),
            extra_protected,
            identity: l.row.service_identity.clone().unwrap_or_else(|| "factory-bot".into()),
        })
    }

    pub fn repo_path(&self, repo: &str) -> Result<PathBuf> {
        if repo.contains("..") || repo.starts_with('/') || repo.starts_with('\\') {
            bail!("bad repository name {repo}");
        }
        let p = self.root.join(format!("{repo}.git"));
        if !p.exists() {
            bail!("repository {repo} not found");
        }
        Ok(p)
    }

    fn pr_ns(&self) -> String {
        format!("fake_pr:{}", self.id)
    }

    fn pr_id(repo: &str, n: i64) -> String {
        format!("{repo}#{n}")
    }

    async fn update_pr<R>(&self, repo: &str, n: i64, f: impl FnOnce(&mut PrDetail) -> Result<R>) -> Result<R> {
        store_update(&self.pool, &self.pr_ns(), &Self::pr_id(repo, n), f).await
    }

    async fn all_prs(&self, repo: &str) -> Result<Vec<PrDetail>> {
        let all: Vec<(i64, PrDetail)> = store_list(&self.pool, &self.pr_ns(), 0).await?;
        let prefix = format!("{repo}#");
        let _ = prefix;
        Ok(all
            .into_iter()
            .map(|(_, d)| d)
            .filter(|d| d.pr.url.contains(&format!("/{repo}/pull/")))
            .collect())
    }

    pub async fn emit(&self, ev: RepoEvent) -> Result<()> {
        record_event(&self.pool, self.id, &ev, &ev.id.clone()).await
    }

    /// Simulate a human action on a PR (used by the fake API and tests).
    pub async fn human_review(&self, repo: &str, n: i64, author: &str, state: &str, body: &str) -> Result<RepoEvent> {
        let rv = PrReview { id: new_id(), author: author.into(), state: state.into(), body: body.into() };
        self.update_pr(repo, n, |d| {
            d.reviews.push(rv.clone());
            Ok(())
        })
        .await?;
        let ev = RepoEvent {
            id: format!("review-{}", rv.id),
            repo: repo.into(),
            pr_number: Some(n),
            kind: "review".into(),
            author: Some(author.into()),
            body: Some(body.into()),
            state: Some(state.into()),
            thread_id: None,
            branch: None,
        };
        self.emit(ev.clone()).await?;
        Ok(ev)
    }

    pub async fn human_thread(&self, repo: &str, n: i64, author: &str, body: &str, path: Option<&str>, line: Option<i64>) -> Result<RepoEvent> {
        let th = PrThread {
            id: new_id(),
            path: path.map(String::from),
            line,
            status: None,
            comments: vec![Comment { id: new_id(), author: author.into(), body: body.into(), created_at: Some(chrono::Utc::now().to_rfc3339()) }],
        };
        let tid = th.id.clone();
        self.update_pr(repo, n, |d| {
            d.threads.push(th);
            Ok(())
        })
        .await?;
        let ev = RepoEvent {
            id: format!("thread-{tid}"),
            repo: repo.into(),
            pr_number: Some(n),
            kind: "comment".into(),
            author: Some(author.into()),
            body: Some(body.into()),
            state: None,
            thread_id: Some(tid),
            branch: None,
        };
        self.emit(ev.clone()).await?;
        Ok(ev)
    }

    pub async fn human_reply(&self, repo: &str, n: i64, author: &str, thread_id: &str, body: &str) -> Result<RepoEvent> {
        let c = Comment { id: new_id(), author: author.into(), body: body.into(), created_at: Some(chrono::Utc::now().to_rfc3339()) };
        let cid = c.id.clone();
        self.update_pr(repo, n, |d| {
            let t = d.threads.iter_mut().find(|t| t.id == thread_id).ok_or_else(|| anyhow!("no thread"))?;
            t.comments.push(c);
            Ok(())
        })
        .await?;
        let ev = RepoEvent {
            id: format!("reply-{cid}"),
            repo: repo.into(),
            pr_number: Some(n),
            kind: "thread_reply".into(),
            author: Some(author.into()),
            body: Some(body.into()),
            state: None,
            thread_id: Some(thread_id.into()),
            branch: None,
        };
        self.emit(ev.clone()).await?;
        Ok(ev)
    }

    pub async fn set_state(&self, repo: &str, n: i64, state: &str) -> Result<RepoEvent> {
        self.update_pr(repo, n, |d| {
            d.pr.state = state.into();
            Ok(())
        })
        .await?;
        let ev = RepoEvent {
            id: format!("state-{}", new_id()),
            repo: repo.into(),
            pr_number: Some(n),
            kind: if state == "merged" { "merged".into() } else { "closed".into() },
            author: Some("human".into()),
            body: None,
            state: Some(state.into()),
            thread_id: None,
            branch: None,
        };
        self.emit(ev.clone()).await?;
        Ok(ev)
    }

    /// Create a PR as a human (for the PR review workflow).
    pub async fn human_open_pr(&self, repo: &str, head: &str, base: &str, title: &str, body: &str, author: &str) -> Result<Pr> {
        let pr = self.create_pr(repo, head, base, title, body, author).await?;
        let ev = RepoEvent {
            id: format!("opened-{}-{}", repo, pr.number),
            repo: repo.into(),
            pr_number: Some(pr.number),
            kind: "pr_opened".into(),
            author: Some(author.into()),
            body: None,
            state: None,
            thread_id: None,
            branch: Some(head.into()),
        };
        self.emit(ev).await?;
        Ok(pr)
    }

    pub async fn set_check(&self, repo: &str, git_ref: &str, check: CheckRun) -> Result<()> {
        let sha = git(Some(&self.repo_path(repo)?), &["rev-parse", git_ref]).await.unwrap_or_else(|_| git_ref.to_string());
        let ns = format!("fake_ci:{}", self.id);
        let id = format!("{repo}@{sha}");
        let mut list: Vec<CheckRun> = store_get(&self.pool, &ns, &id).await?.unwrap_or_default();
        list.retain(|c| c.name != check.name);
        list.push(check.clone());
        store_put(&self.pool, &ns, &id, &list).await?;
        if check.status == "completed" {
            let prs = self.all_prs(repo).await?;
            let pr = prs.iter().find(|d| d.pr.head == git_ref || d.pr.head_sha.as_deref() == Some(&sha));
            self.emit(RepoEvent {
                id: format!("ci-{}-{}", check.id, new_id()),
                repo: repo.into(),
                pr_number: pr.map(|p| p.pr.number),
                kind: "ci_completed".into(),
                author: Some("ci".into()),
                body: Some(check.name.clone()),
                state: check.conclusion.clone(),
                thread_id: None,
                branch: pr.map(|p| p.pr.head.clone()),
            })
            .await?;
        }
        Ok(())
    }

    pub async fn set_check_logs(&self, check_id: &str, logs: &str) -> Result<()> {
        store_put(&self.pool, &format!("fake_ci_logs:{}", self.id), check_id, &json!(logs)).await
    }

    async fn create_pr(&self, repo: &str, head: &str, base: &str, title: &str, body: &str, author: &str) -> Result<Pr> {
        let path = self.repo_path(repo)?;
        let head_sha = git(Some(&path), &["rev-parse", &format!("refs/heads/{head}")]).await.ok();
        if head_sha.is_none() {
            bail!("head branch {head} does not exist in {repo}");
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(format!("fakepr:{}:{repo}", self.id))
            .execute(&mut *tx)
            .await?;
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM fake_store WHERE ns=$1 AND id LIKE $2")
            .bind(self.pr_ns())
            .bind(format!("{repo}#%"))
            .fetch_one(&mut *tx)
            .await?;
        let number = n + 1;
        let pr = Pr {
            number,
            url: format!("https://fake-repo.local/{repo}/pull/{number}"),
            title: title.into(),
            body: body.into(),
            state: "open".into(),
            head: head.into(),
            base: base.into(),
            author: author.into(),
            head_sha,
        };
        let detail = PrDetail { pr: pr.clone(), ..Default::default() };
        sqlx::query("INSERT INTO fake_store(ns,id,data) VALUES ($1,$2,$3)")
            .bind(self.pr_ns())
            .bind(Self::pr_id(repo, number))
            .bind(serde_json::to_value(&detail)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(pr)
    }

    async fn refresh_head(&self, repo: &str, d: &mut PrDetail) {
        if let Ok(p) = self.repo_path(repo) {
            if let Ok(sha) = git(Some(&p), &["rev-parse", &format!("refs/heads/{}", d.pr.head)]).await {
                d.pr.head_sha = Some(sha);
            }
        }
    }
}

#[async_trait]
impl RepoHost for FakeRepo {
    async fn test_connection(&self) -> Result<String> {
        if !self.root.exists() {
            bail!("fake repo root {} does not exist", self.root.display());
        }
        Ok(format!("fake repo host at {}", self.root.display()))
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>> {
        let mut out = vec![];
        let mut stack = vec![(self.root.clone(), String::new())];
        while let Some((dir, prefix)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !e.path().is_dir() {
                    continue;
                }
                let full = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                if let Some(stem) = full.strip_suffix(".git") {
                    if let Ok(info) = self.repo_info(stem).await {
                        out.push(info);
                    }
                } else if prefix.matches('/').count() < 2 {
                    stack.push((e.path(), full));
                }
            }
        }
        out.sort_by(|a, b| a.full_name.cmp(&b.full_name));
        Ok(out)
    }

    async fn repo_info(&self, repo: &str) -> Result<RepoInfo> {
        let p = self.repo_path(repo)?;
        let head = git(Some(&p), &["symbolic-ref", "--short", "HEAD"]).await.unwrap_or_else(|_| "main".into());
        let mut protected = vec![head.clone()];
        for b in &self.extra_protected {
            if !protected.contains(b) {
                protected.push(b.clone());
            }
        }
        Ok(RepoInfo { full_name: repo.into(), default_branch: head, protected_branches: protected })
    }

    async fn list_files(&self, repo: &str, git_ref: &str) -> Result<Vec<String>> {
        let p = self.repo_path(repo)?;
        let out = git(Some(&p), &["ls-tree", "-r", "--name-only", git_ref]).await?;
        Ok(out.lines().map(String::from).collect())
    }

    async fn read_file(&self, repo: &str, git_ref: &str, path: &str) -> Result<Option<String>> {
        let p = self.repo_path(repo)?;
        let spec = format!("{git_ref}:{path}");
        let o = crate::gitutil::git_raw(Some(&p), &["show", &spec], None, &[]).await?;
        if o.status != 0 {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&o.stdout).into_owned()))
    }

    async fn search(&self, repo: &str, query: &str) -> Result<Vec<Value>> {
        let p = self.repo_path(repo)?;
        let info = self.repo_info(repo).await?;
        let o = crate::gitutil::git_raw(Some(&p), &["grep", "-n", "-I", "-F", "-e", query, &info.default_branch], None, &[]).await?;
        Ok(String::from_utf8_lossy(&o.stdout)
            .lines()
            .take(100)
            .filter_map(|l| {
                // <ref>:<path>:<line>:<text>
                let mut parts = l.splitn(4, ':');
                let _r = parts.next()?;
                let path = parts.next()?;
                let line = parts.next()?;
                let text = parts.next().unwrap_or("");
                Some(json!({"path": path, "line": line.parse::<i64>().unwrap_or(0), "text": text}))
            })
            .collect())
    }

    fn git_remote(&self, repo: &str) -> Result<GitRemote> {
        Ok(GitRemote { url: self.repo_path(repo)?.to_string_lossy().into_owned(), auth_header: None })
    }

    async fn find_open_pr(&self, repo: &str, head: &str) -> Result<Option<Pr>> {
        Ok(self
            .all_prs(repo)
            .await?
            .into_iter()
            .map(|d| d.pr)
            .find(|p| p.head == head && p.state == "open"))
    }

    async fn open_pr(&self, repo: &str, head: &str, base: &str, title: &str, body: &str) -> Result<Pr> {
        let identity = self.identity.clone();
        self.create_pr(repo, head, base, title, body, &identity).await
    }

    async fn get_pr(&self, repo: &str, number: i64) -> Result<PrDetail> {
        let mut d: PrDetail = store_get(&self.pool, &self.pr_ns(), &Self::pr_id(repo, number))
            .await?
            .ok_or_else(|| anyhow!("PR {repo}#{number} not found"))?;
        self.refresh_head(repo, &mut d).await;
        Ok(d)
    }

    async fn list_prs(&self, repo: &str, state: &str) -> Result<Vec<Pr>> {
        Ok(self
            .all_prs(repo)
            .await?
            .into_iter()
            .map(|d| d.pr)
            .filter(|p| state == "all" || p.state == state)
            .collect())
    }

    async fn comment(&self, repo: &str, number: i64, body: &str, path: Option<&str>, line: Option<i64>) -> Result<Value> {
        let c = Comment { id: new_id(), author: self.identity.clone(), body: body.into(), created_at: Some(chrono::Utc::now().to_rfc3339()) };
        let cid = c.id.clone();
        if path.is_some() {
            let th = PrThread { id: new_id(), path: path.map(String::from), line, status: None, comments: vec![c] };
            let tid = th.id.clone();
            self.update_pr(repo, number, |d| {
                d.threads.push(th);
                Ok(())
            })
            .await?;
            Ok(json!({"thread_id": tid, "comment_id": cid}))
        } else {
            self.update_pr(repo, number, |d| {
                d.comments.push(c);
                Ok(())
            })
            .await?;
            Ok(json!({"comment_id": cid}))
        }
    }

    async fn reply(&self, repo: &str, number: i64, thread_id: &str, body: &str) -> Result<Value> {
        let c = Comment { id: new_id(), author: self.identity.clone(), body: body.into(), created_at: Some(chrono::Utc::now().to_rfc3339()) };
        let cid = c.id.clone();
        self.update_pr(repo, number, |d| {
            let t = d.threads.iter_mut().find(|t| t.id == thread_id).ok_or_else(|| anyhow!("thread {thread_id} not found"))?;
            t.comments.push(c);
            Ok(())
        })
        .await?;
        Ok(json!({"comment_id": cid}))
    }

    async fn resolve_thread(&self, repo: &str, number: i64, thread_id: &str, status: &str) -> Result<()> {
        let status = status.to_string();
        self.update_pr(repo, number, |d| {
            let t = d.threads.iter_mut().find(|t| t.id == thread_id).ok_or_else(|| anyhow!("thread {thread_id} not found"))?;
            t.status = Some(status);
            Ok(())
        })
        .await
    }

    async fn merge(&self, repo: &str, number: i64) -> Result<()> {
        let d = self.get_pr(repo, number).await?;
        if d.pr.state != "open" {
            bail!("PR is {}", d.pr.state);
        }
        let p = self.repo_path(repo)?;
        let head = d.pr.head_sha.clone().ok_or_else(|| anyhow!("no head"))?;
        let base_ref = format!("refs/heads/{}", d.pr.base);
        let base_sha = git(Some(&p), &["rev-parse", &base_ref]).await?;
        let o = crate::gitutil::git_raw(Some(&p), &["merge-base", "--is-ancestor", &base_sha, &head], None, &[]).await?;
        if o.status != 0 {
            bail!("fake host only merges fast-forward PRs");
        }
        git(Some(&p), &["update-ref", &base_ref, &head, &base_sha]).await?;
        self.update_pr(repo, number, |d| {
            d.pr.state = "merged".into();
            Ok(())
        })
        .await
    }

    async fn checks(&self, repo: &str, git_ref: &str) -> Result<Vec<CheckRun>> {
        let p = self.repo_path(repo)?;
        let sha = git(Some(&p), &["rev-parse", git_ref]).await.unwrap_or_else(|_| git_ref.to_string());
        Ok(store_get(&self.pool, &format!("fake_ci:{}", self.id), &format!("{repo}@{sha}")).await?.unwrap_or_default())
    }

    async fn check_logs(&self, _repo: &str, check_id: &str) -> Result<String> {
        let v: Option<Value> = store_get(&self.pool, &format!("fake_ci_logs:{}", self.id), check_id).await?;
        Ok(v.and_then(|v| v.as_str().map(String::from)).unwrap_or_default())
    }

    async fn poll(&self, repo: &str, cursor: Option<&str>) -> Result<(Vec<RepoEvent>, Option<String>)> {
        let after: i64 = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
        let evs: Vec<(i64, RepoEvent)> = store_list(&self.pool, &format!("fake_events:{}", self.id), after).await?;
        let next = evs.last().map(|(s, _)| s.to_string()).or(cursor.map(String::from));
        Ok((evs.into_iter().map(|(_, e)| e).filter(|e| e.repo == repo).collect(), next))
    }
}

// ---------- chat ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FakeChatMessage {
    pub channel: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub user: String,
    pub text: String,
    #[serde(default)]
    pub approval_for: Option<String>,
    #[serde(default)]
    pub marker: Option<String>,
    #[serde(default)]
    pub mention: Option<String>,
    #[serde(default)]
    pub button_labels: Option<(String, String)>,
    #[serde(default)]
    pub edited: bool,
}

pub struct FakeChat {
    pool: PgPool,
    id: Uuid,
}

impl FakeChat {
    pub fn new(pool: PgPool, id: Uuid) -> Self {
        FakeChat { pool, id }
    }
    pub fn ns(id: Uuid) -> String {
        format!("fake_chat:{id}")
    }
    pub async fn messages(pool: &PgPool, id: Uuid) -> Result<Vec<FakeChatMessage>> {
        Ok(store_list(pool, &Self::ns(id), 0).await?.into_iter().map(|(_, m)| m).collect())
    }
    /// Record a message from a human (the fake API then dispatches the event).
    pub async fn human_post(pool: &PgPool, id: Uuid, channel: &str, thread_ts: Option<&str>, user: &str, text: &str) -> Result<FakeChatMessage> {
        let m = FakeChatMessage {
            channel: channel.into(),
            ts: new_id(),
            thread_ts: thread_ts.map(String::from),
            user: user.into(),
            text: text.into(),
            approval_for: None,
            marker: None,
            mention: None,
            button_labels: None,
            edited: false,
        };
        store_put(pool, &Self::ns(id), &m.ts, &m).await?;
        Ok(m)
    }
}

#[async_trait]
impl Chat for FakeChat {
    async fn test_connection(&self) -> Result<String> {
        Ok("fake chat is reachable".into())
    }
    async fn post(&self, channel: &str, thread: Option<&ChatRef>, msg: &ChatMessage) -> Result<ChatRef> {
        let m = FakeChatMessage {
            channel: channel.into(),
            ts: new_id(),
            thread_ts: thread.map(|t| t.thread_ts.clone().unwrap_or_else(|| t.ts.clone())),
            user: "factory-bot".into(),
            text: msg.text.clone(),
            approval_for: msg.approval_for.clone(),
            marker: msg.marker.clone(),
            mention: msg.mention.clone(),
            button_labels: msg.button_labels.clone(),
            edited: false,
        };
        store_put(&self.pool, &Self::ns(self.id), &m.ts, &m).await?;
        Ok(ChatRef { channel: channel.into(), ts: m.ts, thread_ts: m.thread_ts })
    }
    async fn update(&self, msg_ref: &ChatRef, text: &str) -> Result<()> {
        let t = text.to_string();
        store_update(&self.pool, &Self::ns(self.id), &msg_ref.ts, move |m: &mut FakeChatMessage| {
            m.text = t;
            m.edited = true;
            Ok(())
        })
        .await
    }
    async fn find_by_marker(&self, channel: &str, marker: &str) -> Result<Option<ChatRef>> {
        let all = Self::messages(&self.pool, self.id).await?;
        Ok(all
            .into_iter()
            .find(|m| m.channel == channel && m.marker.as_deref() == Some(marker))
            .map(|m| ChatRef { channel: m.channel, ts: m.ts, thread_ts: m.thread_ts }))
    }
    async fn lookup_user_by_email(&self, email: &str) -> Result<Option<String>> {
        Ok(Some(format!("U-{}", email.split('@').next().unwrap_or(email))))
    }
    async fn user_email(&self, user: &str) -> Result<Option<String>> {
        // The inverse of lookup_user_by_email: U-carol is the user whose address starts with carol@.
        let Some(local) = user.strip_prefix("U-") else { return Ok(None) };
        Ok(sqlx::query_scalar("SELECT email FROM users WHERE lower(split_part(email,'@',1))=lower($1) ORDER BY created_at LIMIT 1").bind(local).fetch_optional(&self.pool).await?)
    }
}
