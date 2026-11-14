//! Controls for the fake external systems, standing in for the humans and
//! systems on the other side of each connector. Mounted only when the
//! development sign-in is enabled. Each action takes the same path a real
//! webhook would (an event job), and is recorded for polling reconciliation.

use super::auth::User;
use super::{ApiResult, bad};
use crate::AppState;
use crate::connectors::fake::{FakeChat, FakeRepo, FakeTickets};
use crate::connectors::{CheckRun, ChatEvent, Ticket, TicketEvent};
use crate::gitutil::git;
use crate::jobs::Job;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/fake/{cid}/tickets", post(create_ticket))
        .route("/api/fake/{cid}/ticket-event", post(ticket_event))
        .route("/api/fake/{cid}/repos", post(create_repo))
        .route("/api/fake/{cid}/pr-action", post(pr_action))
        .route("/api/fake/{cid}/pr", post(get_pr))
        .route("/api/fake/{cid}/chat/messages", get(chat_messages))
        .route("/api/fake/{cid}/chat/reply", post(chat_reply))
}

async fn fake_repo(state: &AppState, cid: Uuid) -> ApiResult<FakeRepo> {
    let l = crate::connectors::load(state, cid).await?;
    if l.row.kind != "fake_repo" {
        return Err(bad("not a fake repo connector"));
    }
    Ok(FakeRepo::from_loaded(state.pool.clone(), &l)?)
}

async fn create_ticket(State(state): State<AppState>, _u: User, Path(cid): Path<Uuid>, Json(t): Json<Ticket>) -> ApiResult<Json<Value>> {
    FakeTickets::new(state.pool.clone(), cid).create(&t).await?;
    Ok(Json(json!({"key": t.key})))
}

/// A change to a ticket by a human: label, transition, comment, cancel.
async fn ticket_event(State(state): State<AppState>, _u: User, Path(cid): Path<Uuid>, Json(mut ev): Json<TicketEvent>) -> ApiResult<Json<Value>> {
    if ev.id.is_empty() {
        ev.id = Uuid::now_v7().to_string();
    }
    let ns = format!("fake_ticket:{cid}");
    let e2 = ev.clone();
    crate::connectors::fake::store_update(&state.pool, &ns, &ev.key, move |t: &mut Ticket| {
        match e2.kind.as_str() {
            "labeled" => t.labels.extend(e2.label.clone()),
            "transitioned" | "cancelled" => t.status = e2.status.clone().unwrap_or_else(|| "Cancelled".into()),
            "commented" => t.comments.push(crate::connectors::Comment {
                id: e2.id.clone(),
                author: e2.author.clone().unwrap_or_default(),
                body: e2.body.clone().unwrap_or_default(),
                created_at: Some(chrono::Utc::now().to_rfc3339()),
            }),
            _ => {}
        }
        Ok(())
    })
    .await?;
    crate::connectors::fake::record_event(&state.pool, cid, &ev, &ev.id).await?;
    state.jobs.push(Job::TicketEvent { connector_id: cid, event: ev.clone() }, Some(format!("tev:{cid}:{}", ev.id))).await?;
    Ok(Json(json!({"id": ev.id})))
}

#[derive(Deserialize)]
struct NewRepo {
    name: String,
    #[serde(default)]
    default_branch: Option<String>,
    files: BTreeMap<String, String>,
}

/// Create a bare repository with one initial commit.
async fn create_repo(State(state): State<AppState>, _u: User, Path(cid): Path<Uuid>, Json(req): Json<NewRepo>) -> ApiResult<Json<Value>> {
    let l = crate::connectors::load(&state, cid).await?;
    let root = std::path::PathBuf::from(l.cfg("root").ok_or_else(|| bad("no root"))?);
    if req.name.contains("..") {
        return Err(bad("bad name"));
    }
    let bare = root.join(format!("{}.git", req.name));
    let branch = req.default_branch.unwrap_or_else(|| "main".into());
    let tmp = tempfile::tempdir().map_err(anyhow::Error::from)?;
    let w = tmp.path();
    git(Some(w), &["init", "-q", "-b", &branch]).await?;
    for (p, c) in &req.files {
        let f = w.join(p);
        if let Some(d) = f.parent() {
            std::fs::create_dir_all(d).map_err(anyhow::Error::from)?;
        }
        std::fs::write(&f, c).map_err(anyhow::Error::from)?;
    }
    git(Some(w), &["add", "-A"]).await?;
    git(Some(w), &["-c", "user.name=Fake Human", "-c", "user.email=human@example.test", "commit", "-q", "-m", "initial"]).await?;
    std::fs::create_dir_all(bare.parent().unwrap()).map_err(anyhow::Error::from)?;
    git(None, &["clone", "-q", "--bare", &w.to_string_lossy(), &bare.to_string_lossy()]).await?;
    Ok(Json(json!({"name": req.name, "path": bare})))
}

#[derive(Deserialize)]
struct PrAction {
    repo: String,
    #[serde(default)]
    number: Option<i64>,
    action: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<i64>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    head: Option<String>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    check: Option<CheckRun>,
    #[serde(default)]
    git_ref: Option<String>,
    #[serde(default)]
    logs: Option<String>,
    /// For "push": files to commit onto `head`.
    #[serde(default)]
    files: Option<BTreeMap<String, String>>,
}

async fn pr_action(State(state): State<AppState>, _u: User, Path(cid): Path<Uuid>, Json(a): Json<PrAction>) -> ApiResult<Json<Value>> {
    let f = fake_repo(&state, cid).await?;
    let author = a.author.clone().unwrap_or_else(|| "reviewer".into());
    let n = a.number.unwrap_or(0);
    let ev = match a.action.as_str() {
        "review" => Some(f.human_review(&a.repo, n, &author, a.state.as_deref().unwrap_or("commented"), a.body.as_deref().unwrap_or("")).await?),
        "thread" => Some(f.human_thread(&a.repo, n, &author, a.body.as_deref().unwrap_or(""), a.path.as_deref(), a.line).await?),
        "reply" => Some(f.human_reply(&a.repo, n, &author, a.thread_id.as_deref().unwrap_or(""), a.body.as_deref().unwrap_or("")).await?),
        "state" => Some(f.set_state(&a.repo, n, a.state.as_deref().unwrap_or("closed")).await?),
        "open" => {
            let pr = f
                .human_open_pr(&a.repo, a.head.as_deref().unwrap_or(""), a.base.as_deref().unwrap_or("main"), a.title.as_deref().unwrap_or("Change"), a.body.as_deref().unwrap_or(""), &author)
                .await?;
            let ev = crate::connectors::RepoEvent { id: format!("opened-{}-{}", a.repo, pr.number), repo: a.repo.clone(), pr_number: Some(pr.number), kind: "pr_opened".into(), author: Some(author.clone()), body: None, state: None, thread_id: None, branch: a.head.clone() };
            state.jobs.push(Job::RepoEvent { connector_id: cid, event: ev }, None).await?;
            return Ok(Json(serde_json::to_value(pr).unwrap()));
        }
        "push" => {
            // A human pushes a commit to a branch (e.g. the author of a reviewed PR).
            let bare = f.repo_path(&a.repo)?;
            let head = a.head.clone().ok_or_else(|| bad("head is required"))?;
            let tmp = tempfile::tempdir().map_err(anyhow::Error::from)?;
            let w = tmp.path().join("w");
            git(None, &["clone", "-q", "-b", &head, &bare.to_string_lossy(), &w.to_string_lossy()]).await?;
            for (p, c) in a.files.clone().unwrap_or_default() {
                let fp = w.join(&p);
                if let Some(d) = fp.parent() {
                    std::fs::create_dir_all(d).map_err(anyhow::Error::from)?;
                }
                std::fs::write(fp, c).map_err(anyhow::Error::from)?;
            }
            git(Some(&w), &["add", "-A"]).await?;
            git(Some(&w), &["-c", &format!("user.name={author}"), "-c", "user.email=human@example.test", "commit", "-q", "-m", a.body.as_deref().unwrap_or("update")]).await?;
            git(Some(&w), &["push", "-q", "origin", &head]).await?;
            let sha = git(Some(&w), &["rev-parse", "HEAD"]).await?;
            let ev = crate::connectors::RepoEvent { id: format!("push-{}-{sha}", a.repo), repo: a.repo.clone(), pr_number: a.number, kind: "pushed".into(), author: Some(author.clone()), body: None, state: None, thread_id: None, branch: Some(head) };
            f.emit(ev.clone()).await?;
            Some(ev)
        }
        "check" => {
            let c = a.check.clone().ok_or_else(|| bad("check is required"))?;
            if let Some(l) = &a.logs {
                f.set_check_logs(&c.id, l).await?;
            }
            f.set_check(&a.repo, a.git_ref.as_deref().unwrap_or("main"), c).await?;
            None
        }
        x => return Err(bad(format!("unknown action {x}"))),
    };
    if let Some(ev) = &ev {
        state.jobs.push(Job::RepoEvent { connector_id: cid, event: ev.clone() }, Some(format!("rev:{cid}:{}", ev.id))).await?;
    }
    Ok(Json(json!({"event": ev})))
}

#[derive(Deserialize)]
struct PrQ {
    repo: String,
    number: i64,
}

async fn get_pr(State(state): State<AppState>, _u: User, Path(cid): Path<Uuid>, Json(q): Json<PrQ>) -> ApiResult<Json<Value>> {
    use crate::connectors::RepoHost;
    let f = fake_repo(&state, cid).await?;
    Ok(Json(serde_json::to_value(f.get_pr(&q.repo, q.number).await?).unwrap()))
}

async fn chat_messages(State(state): State<AppState>, _u: User, Path(cid): Path<Uuid>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"messages": FakeChat::messages(&state.pool, cid).await?})))
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
    user: String,
    #[serde(default)]
    text: String,
    /// Button press: [question id, approved|rejected]
    #[serde(default)]
    action: Option<(String, String)>,
}

async fn chat_reply(State(state): State<AppState>, _u: User, Path(cid): Path<Uuid>, Json(r): Json<Reply>) -> ApiResult<Json<Value>> {
    let channel = r.channel.clone().unwrap_or_else(|| "#factory".into());
    let m = FakeChat::human_post(&state.pool, cid, &channel, r.thread_ts.as_deref(), &r.user, &r.text).await?;
    let ev = ChatEvent { id: m.ts.clone(), channel, thread_ts: r.thread_ts, user: r.user, text: r.text, action: r.action };
    state.jobs.push(Job::ChatEvent { connector_id: cid, event: ev }, Some(format!("cev:{cid}:{}", m.ts))).await?;
    Ok(Json(json!({"ts": m.ts})))
}
