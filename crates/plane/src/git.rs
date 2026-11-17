//! The plane's git endpoint (smart HTTP). Runners clone, fetch and push here
//! with their run token; the plane serves a local mirror, enforces the push
//! policy and forwards accepted pushes upstream with the connector
//! credential, which never leaves the plane.

use crate::engine::{self, RepoRow};
use crate::gitutil::{ZERO_SHA, git, git_raw};
use crate::primitives::deny;
use crate::runs::Run;
use crate::{AppState, events};
use anyhow::{Result, anyhow, bail};
use proto::WorkflowKind;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

pub fn mirror_dir(state: &AppState, repo_id: Uuid) -> PathBuf {
    state.cfg.data_dir.join("mirrors").join(format!("{repo_id}.git"))
}

async fn repo_lock(state: &AppState, repo_id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
    state.git_locks.lock().await.entry(repo_id).or_default().clone()
}

fn upstream_config(remote: &crate::connectors::GitRemote) -> Vec<String> {
    let mut c = vec!["core.askPass=".to_string(), "credential.helper=".to_string()];
    if let Some(h) = &remote.auth_header {
        // `auth_header` is the header value; git wants the whole header line.
        c.push(format!("http.extraHeader=Authorization: {h}"));
    }
    c
}

#[cfg(test)]
mod upstream_tests {
    use super::*;

    #[test]
    fn upstream_config_sends_a_full_authorization_header() {
        let r = crate::connectors::GitRemote { url: "https://github.com/a/b.git".into(), auth_header: Some("Basic abc=".into()) };
        assert_eq!(upstream_config(&r), vec!["core.askPass=", "credential.helper=", "http.extraHeader=Authorization: Basic abc="]);
        let r = crate::connectors::GitRemote { url: "/tmp/x".into(), auth_header: None };
        assert_eq!(upstream_config(&r).len(), 2);
    }

    /// Against a live HTTP server: git must send the header as `Authorization`.
    #[tokio::test]
    async fn git_sends_the_authorization_header() {
        use std::sync::Mutex;
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen2 = seen.clone();
        let app = axum::Router::new().fallback(move |headers: axum::http::HeaderMap| {
            let seen = seen2.clone();
            async move {
                seen.lock().unwrap().push(headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string());
                (axum::http::StatusCode::NOT_FOUND, "")
            }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/a/b.git", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let r = crate::connectors::GitRemote { url: url.clone(), auth_header: Some("Basic abc=".into()) };
        let _ = git_raw(None, &["ls-remote", &url], None, &upstream_config(&r)).await.unwrap();
        assert_eq!(seen.lock().unwrap().first().map(String::as_str), Some("Basic abc="));
    }
}

/// Bring the mirror up to date with upstream. Caller holds the repo lock.
async fn sync_mirror(state: &AppState, r: &RepoRow) -> Result<PathBuf> {
    let dir = mirror_dir(state, r.id);
    if !dir.join("HEAD").exists() {
        std::fs::create_dir_all(&dir)?;
        git(Some(&dir), &["init", "--bare", "-q"]).await?;
        git(Some(&dir), &["config", "uploadpack.allowReachableSHA1InWant", "true"]).await?;
        git(Some(&dir), &["config", "receive.denyNonFastForwards", "true"]).await?;
        git(Some(&dir), &["config", "receive.denyDeletes", "true"]).await?;
    }
    let host = crate::connectors::repo_host(state, r.connector_id).await?;
    let remote = host.git_remote(&r.full_name)?;
    let o = git_raw(Some(&dir), &["fetch", "--prune", "--quiet", &remote.url, "+refs/heads/*:refs/heads/*"], None, &upstream_config(&remote)).await?;
    if o.status != 0 {
        bail!("fetching upstream {}: {}", r.full_name, o.stderr.trim());
    }
    let _ = git(Some(&dir), &["symbolic-ref", "HEAD", &format!("refs/heads/{}", r.default_branch)]).await;
    Ok(dir)
}

/// Push policy for one ref update.
pub fn check_ref(run: &Run, r: &RepoRow, refname: &str, deletion: bool) -> Result<()> {
    if run.workflow() == WorkflowKind::PrReview {
        return Err(deny("review_no_push", "the PR review workflow never pushes code"));
    }
    if deletion {
        return Err(deny("no_delete", format!("deleting {refname} is refused")));
    }
    let Some(branch) = refname.strip_prefix("refs/heads/") else {
        return Err(deny("branch_namespace", format!("{refname} is not a branch")));
    };
    if branch == r.default_branch || r.protected_branches.iter().any(|p| p == branch) {
        return Err(deny("protected_branch", format!("{branch} is protected")));
    }
    let ns = run.namespace();
    let ok = branch == ns || branch.starts_with(&format!("{ns}-")) || branch == run.wip_branch();
    if !ok {
        return Err(deny("branch_namespace", format!("{branch} is outside the run's namespace {ns}")));
    }
    // Checkpoints (the wip branch) are always allowed, so no work is lost.
    if run.in_plan_phase() && branch != run.wip_branch() {
        return Err(deny(
            "plan_phase",
            format!("the plan is not approved yet: nothing can be pushed to {branch} and no pull request opened until a human approves it. Submit your plan with plan.submit."),
        ));
    }
    Ok(())
}

pub async fn create_branch(state: &AppState, r: &RepoRow, name: &str, from: &str) -> Result<()> {
    let lock = repo_lock(state, r.id).await;
    let _g = lock.lock().await;
    let dir = sync_mirror(state, r).await?;
    let sha = git(Some(&dir), &["rev-parse", &format!("refs/heads/{from}")]).await.or_else(|_| Ok::<_, anyhow::Error>(from.to_string()))?;
    let host = crate::connectors::repo_host(state, r.connector_id).await?;
    let remote = host.git_remote(&r.full_name)?;
    let target = format!("refs/heads/{name}");
    let existing = git(Some(&dir), &["rev-parse", "--verify", "--quiet", &target]).await.ok();
    if existing.as_deref() == Some(sha.as_str()) {
        return Ok(());
    }
    if existing.is_some() {
        bail!("branch {name} already exists");
    }
    let o = git_raw(Some(&dir), &["push", "--quiet", &remote.url, &format!("{sha}:{target}")], None, &upstream_config(&remote)).await?;
    if o.status != 0 {
        bail!("creating branch upstream: {}", o.stderr.trim());
    }
    git(Some(&dir), &["update-ref", &target, &sha]).await?;
    Ok(())
}

/// Parse the ref-update commands at the start of a receive-pack request.
pub fn parse_commands(body: &[u8]) -> Result<Vec<(String, String, String)>> {
    let mut out = vec![];
    let mut i = 0;
    loop {
        if i + 4 > body.len() {
            bail!("truncated pkt-line");
        }
        let len = usize::from_str_radix(std::str::from_utf8(&body[i..i + 4])?, 16)?;
        if len == 0 {
            break;
        }
        if len < 4 || i + len > body.len() {
            bail!("bad pkt-line length");
        }
        let line = &body[i + 4..i + len];
        i += len;
        let line = match line.iter().position(|b| *b == 0) {
            Some(p) => &line[..p],
            None => line,
        };
        let s = String::from_utf8_lossy(line).trim_end().to_string();
        if s.starts_with("shallow ") || s.starts_with("push-cert") {
            continue;
        }
        let mut parts = s.split(' ');
        let (Some(old), Some(new), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
            bail!("bad command line {s}");
        };
        out.push((old.to_string(), new.to_string(), name.to_string()));
    }
    Ok(out)
}

pub fn pkt(s: &str) -> Vec<u8> {
    let mut v = format!("{:04x}", s.len() + 4).into_bytes();
    v.extend_from_slice(s.as_bytes());
    v
}

pub enum Service {
    Upload,
    Receive,
}

impl Service {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "git-upload-pack" => Some(Service::Upload),
            "git-receive-pack" => Some(Service::Receive),
            _ => None,
        }
    }
    fn cmd(&self) -> &'static str {
        match self {
            Service::Upload => "upload-pack",
            Service::Receive => "receive-pack",
        }
    }
    fn name(&self) -> &'static str {
        match self {
            Service::Upload => "git-upload-pack",
            Service::Receive => "git-receive-pack",
        }
    }
}

pub struct GitResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

fn text(status: u16, msg: impl Into<String>) -> GitResponse {
    GitResponse { status, content_type: "text/plain".into(), body: msg.into().into_bytes() }
}

async fn authorise(state: &AppState, run: &Run, repo_id: Uuid) -> Result<RepoRow, GitResponse> {
    let r = engine::repo(state, repo_id).await.map_err(|_| text(404, "repository not found"))?;
    if r.project_id != run.project_id {
        crate::audit::record(&state.pool, &format!("run:{}", run.id), None, "policy.denied", Some(run.project_id), Some(run.id), json!({"rule": "repo_allowlist", "repo": r.full_name})).await;
        crate::metrics::POLICY_DENIALS.with_label_values(&["repo_allowlist"]).inc();
        return Err(text(403, "repository is not on this project's allowlist"));
    }
    Ok(r)
}

pub async fn info_refs(state: &AppState, run: &Run, repo_id: Uuid, service: &str) -> GitResponse {
    let Some(svc) = Service::parse(service) else { return text(400, "unsupported service") };
    let r = match authorise(state, run, repo_id).await {
        Ok(r) => r,
        Err(e) => return e,
    };
    if matches!(svc, Service::Receive) && run.workflow() == WorkflowKind::PrReview {
        crate::metrics::POLICY_DENIALS.with_label_values(&["review_no_push"]).inc();
        return text(403, "the PR review workflow never pushes code");
    }
    let lock = repo_lock(state, r.id).await;
    let _g = lock.lock().await;
    let dir = match sync_mirror(state, &r).await {
        Ok(d) => d,
        Err(e) => return text(502, format!("{e:#}")),
    };
    let o = match git_raw(Some(&dir), &[svc.cmd(), "--stateless-rpc", "--advertise-refs", "."], None, &[]).await {
        Ok(o) => o,
        Err(e) => return text(500, format!("{e:#}")),
    };
    let mut body = pkt(&format!("# service={}\n", svc.name()));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&o.stdout);
    crate::audit::record(&state.pool, &format!("run:{}", run.id), None, "connector.git.use", Some(run.project_id), Some(run.id), json!({"repo": r.full_name, "service": svc.name()})).await;
    GitResponse { status: 200, content_type: format!("application/x-{}-advertisement", svc.name()), body }
}

pub async fn rpc(state: &AppState, run: &Run, repo_id: Uuid, service: &str, body: Vec<u8>) -> GitResponse {
    let Some(svc) = Service::parse(service) else { return text(400, "unsupported service") };
    let r = match authorise(state, run, repo_id).await {
        Ok(r) => r,
        Err(e) => return e,
    };
    let lock = repo_lock(state, r.id).await;
    let _g = lock.lock().await;
    let dir = mirror_dir(state, r.id);
    if !dir.join("HEAD").exists() {
        if let Err(e) = sync_mirror(state, &r).await {
            return text(502, format!("{e:#}"));
        }
    }
    match svc {
        Service::Upload => {
            let o = match git_raw(Some(&dir), &["upload-pack", "--stateless-rpc", "."], Some(&body), &[]).await {
                Ok(o) => o,
                Err(e) => return text(500, format!("{e:#}")),
            };
            GitResponse { status: 200, content_type: "application/x-git-upload-pack-result".into(), body: o.stdout }
        }
        Service::Receive => receive(state, run, &r, &dir, body).await,
    }
}

async fn receive(state: &AppState, run: &Run, r: &RepoRow, dir: &std::path::Path, body: Vec<u8>) -> GitResponse {
    let cmds = match parse_commands(&body) {
        Ok(c) => c,
        Err(e) => return text(400, format!("{e:#}")),
    };
    for (old, new, name) in &cmds {
        if let Err(e) = check_ref(run, r, name, new == ZERO_SHA) {
            let rule = e.downcast_ref::<crate::primitives::Denied>().map(|d| d.rule).unwrap_or("policy");
            crate::metrics::POLICY_DENIALS.with_label_values(&[rule]).inc();
            crate::audit::record(&state.pool, &format!("run:{}", run.id), None, "policy.denied", Some(run.project_id), Some(run.id), json!({"rule": rule, "ref": name, "old": old, "new": new})).await;
            let _ = events::append(&state.pool, &*state.blobs, run.id, "git_push", Some("agent"), json!({"ref": name, "ok": false, "error": format!("{e:#}"), "side_effect": true})).await;
            return text(403, format!("{e:#}"));
        }
    }
    let before: Vec<Option<String>> = {
        let mut v = vec![];
        for (_, _, name) in &cmds {
            v.push(git(Some(dir), &["rev-parse", "--verify", "--quiet", name]).await.ok());
        }
        v
    };
    let o = match git_raw(
        Some(dir),
        &["receive-pack", "--stateless-rpc", "."],
        Some(&body),
        &["receive.denyNonFastForwards=true".into(), "receive.denyDeletes=true".into()],
    )
    .await
    {
        Ok(o) => o,
        Err(e) => return text(500, format!("{e:#}")),
    };
    // Forward accepted updates upstream with the connector credential.
    let host = match crate::connectors::repo_host(state, r.connector_id).await {
        Ok(h) => h,
        Err(e) => return text(502, format!("{e:#}")),
    };
    let remote = match host.git_remote(&r.full_name) {
        Ok(x) => x,
        Err(e) => return text(502, format!("{e:#}")),
    };
    for (i, (_, new, name)) in cmds.iter().enumerate() {
        let now = git(Some(dir), &["rev-parse", "--verify", "--quiet", name]).await.ok();
        if now.as_deref() != Some(new.as_str()) {
            // Rejected by receive-pack (e.g. not a fast-forward).
            crate::metrics::POLICY_DENIALS.with_label_values(&["fast_forward_only"]).inc();
            crate::audit::record(&state.pool, &format!("run:{}", run.id), None, "policy.denied", Some(run.project_id), Some(run.id), json!({"rule": "fast_forward_only", "ref": name})).await;
            let _ = events::append(&state.pool, &*state.blobs, run.id, "git_push", Some("agent"), json!({"ref": name, "ok": false, "error": "not a fast-forward", "side_effect": true})).await;
            continue;
        }
        if before[i].as_deref() == Some(new.as_str()) {
            continue;
        }
        let up = git_raw(Some(dir), &["push", "--quiet", &remote.url, &format!("{new}:{name}")], None, &upstream_config(&remote)).await;
        let ok = matches!(&up, Ok(o) if o.status == 0);
        if !ok {
            // Roll the mirror back so it matches upstream.
            match &before[i] {
                Some(b) => {
                    let _ = git(Some(dir), &["update-ref", name, b]).await;
                }
                None => {
                    let _ = git(Some(dir), &["update-ref", "-d", name]).await;
                }
            }
            let err = match up {
                Ok(o) => o.stderr,
                Err(e) => format!("{e:#}"),
            };
            let _ = events::append(&state.pool, &*state.blobs, run.id, "git_push", Some("agent"), json!({"ref": name, "ok": false, "error": format!("upstream refused: {}", err.trim()), "side_effect": true})).await;
            return text(502, format!("upstream refused the push: {}", err.trim()));
        }
        let _ = events::append(&state.pool, &*state.blobs, run.id, "git_push", Some("agent"), json!({"ref": name, "new": new, "ok": true, "side_effect": !name.contains("factory-wip/")})).await;
        crate::audit::record(&state.pool, &format!("run:{}", run.id), None, "git.push", Some(run.project_id), Some(run.id), json!({"repo": r.full_name, "ref": name, "new": new})).await;
        // A push to the PR head is review input for the PR's other runs.
        if name == &format!("refs/heads/{}", run.branch.clone().unwrap_or_default()) {
            if let Some(n) = run.pr_number {
                let _ = state
                    .jobs
                    .push(
                        crate::jobs::Job::RepoEvent {
                            connector_id: r.connector_id,
                            event: crate::connectors::RepoEvent {
                                id: format!("push-{}-{new}", r.full_name),
                                repo: r.full_name.clone(),
                                pr_number: Some(n),
                                kind: "pushed".into(),
                                author: crate::connectors::service_identity(state, r.connector_id).await,
                                body: None,
                                state: None,
                                thread_id: None,
                                branch: run.branch.clone(),
                            },
                        },
                        None,
                    )
                    .await;
            }
        }
    }
    let _ = anyhow!("");
    GitResponse { status: 200, content_type: "application/x-git-receive-pack-result".into(), body: o.stdout }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_commands() {
        let mut b = pkt(&format!("{} {} refs/heads/factory/abc\0report-status side-band-64k\n", ZERO_SHA, "1".repeat(40)));
        b.extend(pkt(&format!("{} {} refs/heads/main\n", "2".repeat(40), "3".repeat(40))));
        b.extend_from_slice(b"0000PACK....");
        let c = parse_commands(&b).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].2, "refs/heads/factory/abc");
        assert_eq!(c[1].0, "2".repeat(40));
    }
}
