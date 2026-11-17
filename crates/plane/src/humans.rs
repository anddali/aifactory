//! Humans in the loop: the run's chat thread, questions and approvals, and
//! events arriving from chat, the repo host and the ticket source.

use crate::connectors::{self, ChatEvent, ChatMessage, ChatRef, RepoEvent, TicketEvent};
use crate::engine::{self, NewRun};
use crate::jobs::Job;
use crate::runs::{self, Run};
use crate::{AppState, events};
use anyhow::{Result, anyhow, bail};
use proto::{InputKind, RunState, WorkflowKind};
use serde_json::{Value, json};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Exactly-once side effects
// ---------------------------------------------------------------------------

/// Perform `f` once per key. A retry after success returns the stored result.
/// A retry after a crash mid-effect runs `f` again, so `f` must itself look
/// for its earlier effect (markers) before acting.
pub async fn once<F, Fut>(state: &AppState, key: &str, f: F) -> Result<Value>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    sqlx::query("INSERT INTO side_effects(key, status) VALUES ($1,'pending') ON CONFLICT (key) DO NOTHING")
        .bind(key)
        .execute(&state.pool)
        .await?;
    let (status, result): (String, Option<Value>) = sqlx::query_as("SELECT status, result FROM side_effects WHERE key=$1")
        .bind(key)
        .fetch_one(&state.pool)
        .await?;
    if status == "done" {
        return Ok(result.unwrap_or(Value::Null));
    }
    let r = f().await?;
    sqlx::query("UPDATE side_effects SET status='done', result=$2, updated_at=now() WHERE key=$1")
        .bind(key)
        .bind(&r)
        .execute(&state.pool)
        .await?;
    Ok(r)
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

pub fn role_rank(r: &str) -> i32 {
    match r {
        "view" => 0,
        "operate" => 1,
        "approve" => 2,
        "administer" => 3,
        _ => -1,
    }
}

pub async fn has_role(state: &AppState, user: Uuid, project: Option<Uuid>, role: &str) -> Result<bool> {
    let admin: Option<bool> = sqlx::query_scalar("SELECT is_global_admin FROM users WHERE id=$1")
        .bind(user)
        .fetch_optional(&state.pool)
        .await?;
    if admin == Some(true) {
        return Ok(true);
    }
    let grants: Vec<(Option<Uuid>, String)> = sqlx::query_as("SELECT project_id, role FROM role_grants WHERE user_id=$1")
        .bind(user)
        .fetch_all(&state.pool)
        .await?;
    let need = role_rank(role);
    Ok(grants.iter().any(|(p, r)| {
        // Roles are ordered: view < operate < approve < administer.
        (p.is_none() || *p == project) && role_rank(r) >= need
    }))
}

pub async fn user_for_identity(state: &AppState, provider: &str, external_id: &str) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar("SELECT user_id FROM identities WHERE provider=$1 AND external_id=$2")
        .bind(provider)
        .bind(external_id)
        .fetch_optional(&state.pool)
        .await?)
}

pub async fn identity_for_user(state: &AppState, user: Uuid, provider: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT external_id FROM identities WHERE user_id=$1 AND provider=$2")
        .bind(user)
        .bind(provider)
        .fetch_optional(&state.pool)
        .await?)
}

pub async fn user_label(state: &AppState, user: Uuid) -> String {
    sqlx::query_scalar::<_, String>("SELECT email FROM users WHERE id=$1")
        .bind(user)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| user.to_string())
}

// ---------------------------------------------------------------------------
// Chat
// ---------------------------------------------------------------------------

/// Queue a message to the assignee in the run's thread (exactly once per key).
pub async fn notify(state: &AppState, run: &Run, key: &str, text: &str) -> Result<()> {
    events::append(&state.pool, &*state.blobs, run.id, "human_message", Some("factory"), json!({"text": text, "direction": "to_human"})).await?;
    state
        .jobs
        .push(Job::ChatPost { run_id: run.id, key: key.into(), text: text.into(), question_id: None, ops: false }, Some(format!("chat:{key}")))
        .await
}

/// Alert the operations channel.
pub async fn notify_ops(state: &AppState, run: &Run, key: &str, text: &str) -> Result<()> {
    state
        .jobs
        .push(Job::ChatPost { run_id: run.id, key: key.into(), text: text.into(), question_id: None, ops: true }, Some(format!("chat:{key}")))
        .await
}

pub async fn ensure_thread(state: &AppState, run: &Run) -> Result<Option<ChatRef>> {
    if let Some(t) = &run.chat_thread {
        if let Ok(r) = serde_json::from_value::<ChatRef>(t.clone()) {
            return Ok(Some(r));
        }
    }
    let pc = engine::project_cfg(state, run.project_id).await?;
    let Some(cid) = pc.chat_connector_id else { return Ok(None) };
    let chat = connectors::chat(state, cid).await?;
    let channel = pc.chat_channel.clone();
    let marker = format!("thread:{}", run.id);
    let title = run.title.clone();
    let mention = mention_for(state, run).await;
    let wf = run.workflow.clone();
    let r = once(state, &marker.clone(), || async move {
        if let Some(r) = chat.find_by_marker(&channel, &marker).await? {
            return Ok(serde_json::to_value(r)?);
        }
        let r = chat
            .post(&channel, None, &ChatMessage { text: format!("Run started ({wf}): {title}"), marker: Some(marker), mention, ..Default::default() })
            .await?;
        Ok(serde_json::to_value(r)?)
    })
    .await?;
    sqlx::query("UPDATE runs SET chat_thread=$2 WHERE id=$1").bind(run.id).bind(&r).execute(&state.pool).await?;
    Ok(Some(serde_json::from_value(r)?))
}

async fn mention_for(state: &AppState, run: &Run) -> Option<String> {
    if let Some(u) = run.assignee_user_id {
        if let Ok(Some(id)) = identity_for_user(state, u, "slack").await {
            return Some(id);
        }
    }
    engine::project_cfg(state, run.project_id).await.ok().and_then(|p| p.fallback_contact)
}

pub async fn chat_post(state: &AppState, run_id: Uuid, key: &str, text: &str, question_id: Option<Uuid>, ops: bool) -> Result<()> {
    let run = runs::get(state, run_id).await?;
    let pc = engine::project_cfg(state, run.project_id).await?;
    let Some(cid) = pc.chat_connector_id else { return Ok(()) };
    let chat = connectors::chat(state, cid).await?;
    let global = crate::settings::get(&state.pool).await?;
    let (channel, thread) = if ops {
        let ch = pc.ops_channel.clone().or(global.ops_channel.clone()).unwrap_or_else(|| pc.chat_channel.clone());
        (ch, None)
    } else {
        (pc.chat_channel.clone(), ensure_thread(state, &run).await?)
    };
    let marker = key.to_string();
    let mention = mention_for(state, &run).await;
    let text = text.to_string();
    let qid = question_id.map(|q| q.to_string());
    let button_labels = match question_id {
        Some(q) => {
            let action: Option<String> = sqlx::query_scalar("SELECT action FROM questions WHERE id=$1").bind(q).fetch_optional(&state.pool).await?.flatten();
            (action.as_deref() == Some("plan")).then(|| ("Approve plan".to_string(), "Don't build".to_string()))
        }
        None => None,
    };
    let ch2 = channel.clone();
    let r = once(state, &format!("chatpost:{key}"), || async move {
        if let Some(r) = chat.find_by_marker(&ch2, &marker).await? {
            return Ok(serde_json::to_value(r)?);
        }
        let r = chat
            .post(&ch2, thread.as_ref(), &ChatMessage { text, approval_for: qid, marker: Some(marker), mention, button_labels })
            .await?;
        Ok(serde_json::to_value(r)?)
    })
    .await?;
    if let Some(q) = question_id {
        sqlx::query("UPDATE questions SET chat_ref=$2 WHERE id=$1").bind(q).bind(&r).execute(&state.pool).await?;
    }
    crate::audit::record(&state.pool, &format!("run:{run_id}"), None, "connector.chat.post", Some(run.project_id), Some(run_id), json!({"connector": cid, "channel": channel})).await;
    Ok(())
}

pub async fn chat_update(state: &AppState, question_id: Uuid, text: &str) -> Result<()> {
    let (run_id, chat_ref): (Uuid, Option<Value>) = sqlx::query_as("SELECT run_id, chat_ref FROM questions WHERE id=$1")
        .bind(question_id)
        .fetch_one(&state.pool)
        .await?;
    let Some(cr) = chat_ref else {
        // The question was never posted (no chat connector), nothing to update.
        return Ok(());
    };
    let run = runs::get(state, run_id).await?;
    let pc = engine::project_cfg(state, run.project_id).await?;
    let Some(cid) = pc.chat_connector_id else { return Ok(()) };
    let chat = connectors::chat(state, cid).await?;
    chat.update(&serde_json::from_value(cr)?, text).await
}

/// Comment on the run's ticket and PR, once per key.
pub async fn external_comment(state: &AppState, run_id: Uuid, key: &str, text: &str) -> Result<()> {
    let run = runs::get(state, run_id).await?;
    let pc = engine::project_cfg(state, run.project_id).await?;
    if let (Some(k), Some(tc)) = (&run.ticket_key, pc.ticket_connector_id) {
        let ts = connectors::ticket_source(state, tc).await?;
        let marker = format!("factory:{key}");
        let (k2, t2) = (k.clone(), text.to_string());
        once(state, &format!("ticket-comment:{key}"), || async move {
            if let Some(c) = ts.find_comment(&k2, &marker).await? {
                return Ok(json!({"id": c.id}));
            }
            let c = ts.comment(&k2, &t2, &marker).await?;
            Ok(json!({"id": c.id}))
        })
        .await?;
    }
    if let (Some(repo_id), Some(n)) = (run.repo_id, run.pr_number) {
        let r = engine::repo(state, repo_id).await?;
        let host = connectors::repo_host(state, r.connector_id).await?;
        let marker = format!("<!-- factory:{key} -->");
        let body = format!("{text}\n\n{marker}");
        let name = r.full_name.clone();
        once(state, &format!("pr-comment:{key}"), || async move {
            let d = host.get_pr(&name, n).await?;
            if d.comments.iter().any(|c| c.body.contains(&marker)) {
                return Ok(json!({"existing": true}));
            }
            host.comment(&name, n, &body, None, None).await
        })
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Questions and approvals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct Question {
    pub id: Uuid,
    pub run_id: Uuid,
    pub kind: String,
    pub text: String,
    pub action: Option<String>,
    pub pr_number: Option<i64>,
    pub state: String,
    pub answer: Option<String>,
    pub decision: Option<String>,
    pub answered_by: Option<Uuid>,
    pub answered_via: Option<String>,
    pub chat_ref: Option<Value>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub answered_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Create a question (or approval request) and post it to the thread. The
/// id is derived from the idempotency key, so a retried call creates one.
pub async fn ask(state: &AppState, run: &Run, idem: &str, kind: &str, text: &str, action: Option<&str>, pr: Option<i64>) -> Result<Uuid> {
    let id = uuid_from(&format!("q:{}:{idem}", run.id));
    let inserted = sqlx::query("INSERT INTO questions(id, run_id, kind, text, action, pr_number) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (id) DO NOTHING")
        .bind(id)
        .bind(run.id)
        .bind(kind)
        .bind(text)
        .bind(action)
        .bind(pr)
        .execute(&state.pool)
        .await?
        .rows_affected()
        > 0;
    if inserted {
        events::append(&state.pool, &*state.blobs, run.id, "question_asked", Some("agent"), json!({"question_id": id, "kind": kind, "text": text, "action": action, "pr_number": pr})).await?;
        let (prefix, suffix) = match (kind, action) {
            ("approval", Some("plan")) => (
                "Plan for review",
                "\n\n*Approve plan* starts the build. To change it, reply in this thread with what you want changed. *Don't build* ends the run. The full plan is on the run's Artifacts tab in the portal.",
            ),
            ("approval", _) => ("Approval needed", "\nReply `approve` or `reject` in this thread, or use the portal."),
            _ => ("Question", "\nReply in this thread or in the portal."),
        };
        state
            .jobs
            .push(Job::ChatPost { run_id: run.id, key: format!("question:{id}"), text: format!("{prefix}: {text}{suffix}"), question_id: Some(id), ops: false }, Some(format!("chat:question:{id}")))
            .await?;
    }
    Ok(id)
}

/// Act on a reviewer's decision about a submitted plan.
async fn plan_decision(state: &AppState, run: &Run, q: &Question, who: &str) -> Result<()> {
    let note = q.answer.as_deref().map(str::trim).filter(|a| !a.is_empty() && !is_bare_decision(a));
    let data = json!({"question_id": q.id, "decision": q.decision, "answer": q.answer, "by": who, "plan": true});
    match q.decision.as_deref() {
        Some("approved") => {
            sqlx::query("UPDATE runs SET phase='build' WHERE id=$1 AND phase='plan'").bind(run.id).execute(&state.pool).await?;
            events::append(&state.pool, &*state.blobs, run.id, "plan_approved", Some(who), json!({"question_id": q.id, "note": note})).await?;
            let run = runs::get(state, run.id).await?;
            let build = crate::workflows::build_instructions(state, &run).await?;
            let note = note.map(|n| format!("\n\nReviewer's notes: {n}")).unwrap_or_default();
            let text = format!("{who} approved your plan.{note}\n\n{build}");
            engine::deliver_input(state, &run, InputKind::Approval, &text, data, &format!("answer:{}", q.id), true).await?;
        }
        Some("changes") => {
            events::append(&state.pool, &*state.blobs, run.id, "plan_changes_requested", Some(who), json!({"question_id": q.id, "note": note})).await?;
            let text = format!(
                "{who} asked for changes to your plan:\n\n{}\n\nRevise the plan and submit it again with plan.submit. You are still in the plan phase.",
                q.answer.as_deref().unwrap_or("").trim()
            );
            engine::deliver_input(state, run, InputKind::Answer, &text, data, &format!("answer:{}", q.id), true).await?;
        }
        _ => {
            events::append(&state.pool, &*state.blobs, run.id, "plan_declined", Some(who), json!({"question_id": q.id, "note": note})).await?;
            let why = note.map(|n| format!(": {n}")).unwrap_or_default();
            let summary = format!("{who} decided not to build the plan{why}");
            if run.ticket_key.is_some() {
                let text = format!("The factory proposed a plan for this ticket; {who} decided not to build it{why}. The plan is on the run page: {}", run_url(state, run));
                state.jobs.push(Job::ExternalComment { run_id: run.id, key: format!("declined:{}", run.id), text }, Some(format!("declined-comment:{}", run.id))).await?;
            }
            engine::finish(state, run.id, "declined", &summary).await?;
        }
    }
    Ok(())
}

/// Read a thread reply to a plan review as a decision.
pub fn plan_reply_decision(reply: &str) -> &'static str {
    let bare = reply.trim().trim_end_matches(['.', '!']).to_lowercase();
    let words: Vec<&str> = bare.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()).collect();
    let short = words.len() <= 4;
    let first = words.first().copied().unwrap_or("");
    if bare.starts_with("approve") || (short && matches!(first, "yes" | "yep" | "sure" | "ok" | "okay" | "lgtm" | "go" | "ship" | "proceed" | "approved" | "build")) {
        "approved"
    } else if bare.starts_with("reject") || bare.starts_with("don't build") || bare.starts_with("dont build") || bare.starts_with("decline") || (short && first == "no") {
        "rejected"
    } else {
        "changes"
    }
}

/// A reply that is only the decision word carries no note worth passing on.
fn is_bare_decision(a: &str) -> bool {
    matches!(
        a.trim().trim_end_matches(['.', '!']).to_lowercase().as_str(),
        "approve" | "approved" | "approve plan" | "yes" | "ok" | "lgtm" | "go" | "reject" | "rejected" | "no" | "don't build" | "dont build" | "decline"
    )
}

fn run_url(state: &AppState, run: &Run) -> String {
    let base = state.cfg.public_urls.first().cloned().unwrap_or_default();
    format!("{}/runs/{}", base.trim_end_matches('/'), run.id)
}

/// The first `n` characters of a long text, cut at a line break when possible.
pub fn excerpt(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let cut: String = s.chars().take(n).collect();
    let cut = match cut.rfind('\n') {
        Some(i) if i > n / 2 => cut[..i].to_string(),
        _ => cut,
    };
    format!("{cut}\n…")
}

pub fn uuid_from(s: &str) -> Uuid {
    let h = crate::crypto::sha256_hex(s.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&hex::decode(&h[..32]).unwrap());
    b[6] = (b[6] & 0x0f) | 0x80; // version 8 (custom)
    b[8] = (b[8] & 0x3f) | 0x80;
    Uuid::from_bytes(b)
}

pub async fn question(state: &AppState, id: Uuid) -> Result<Question> {
    sqlx::query_as("SELECT * FROM questions WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| anyhow!("question not found"))
}

/// Answer a question. The first answer wins; later answers are refused and
/// the other surface is updated. Approvals count only from users holding the
/// approve role for the project.
pub async fn answer(state: &AppState, qid: Uuid, user: Uuid, via: &str, answer: Option<&str>, decision: Option<&str>) -> Result<Question> {
    let q = question(state, qid).await?;
    let run = runs::get(state, q.run_id).await?;
    let is_plan = q.kind == "approval" && q.action.as_deref() == Some("plan");
    if is_plan && decision == Some("changes") {
        // Asking for changes to a plan is feedback, not an approval: the assignee or an operator may do it.
        if answer.map(str::trim).unwrap_or("").is_empty() {
            bail!("say what should change in the plan");
        }
        if !has_role(state, user, Some(run.project_id), "operate").await? && run.assignee_user_id != Some(user) {
            bail!("only the assignee or an operator can ask for changes");
        }
    } else if q.kind == "approval" {
        let d = decision.ok_or_else(|| anyhow!("an approval needs a decision: approved or rejected"))?;
        if d != "approved" && d != "rejected" {
            bail!(if is_plan { "decision must be approved, changes or rejected" } else { "decision must be approved or rejected" });
        }
        if !has_role(state, user, Some(run.project_id), "approve").await? {
            crate::metrics::POLICY_DENIALS.with_label_values(&["approve_role"]).inc();
            crate::audit::record(&state.pool, &user_label(state, user).await, Some(user), "approval.refused", Some(run.project_id), Some(run.id), json!({"question_id": qid, "reason": "no approve role"})).await;
            bail!("you do not hold the approve role for this project");
        }
    } else if !has_role(state, user, Some(run.project_id), "operate").await? && run.assignee_user_id != Some(user) {
        bail!("only the assignee or an operator can answer");
    }
    let updated: Option<Question> = sqlx::query_as(
        "UPDATE questions SET state='answered', answer=$2, decision=$3, answered_by=$4, answered_via=$5, answered_at=now()
         WHERE id=$1 AND state='open' RETURNING *",
    )
    .bind(qid)
    .bind(answer)
    .bind(decision)
    .bind(user)
    .bind(via)
    .fetch_optional(&state.pool)
    .await?;
    let Some(q) = updated else {
        bail!("this question was already answered");
    };
    let who = user_label(state, user).await;
    events::append(&state.pool, &*state.blobs, run.id, "question_answered", Some(&who), json!({"question_id": qid, "kind": q.kind, "answer": answer, "decision": decision, "via": via})).await?;
    let action = if q.kind == "approval" { format!("approval.{}", decision.unwrap_or("")) } else { "question.answer".into() };
    crate::audit::record(&state.pool, &who, Some(user), &action, Some(run.project_id), Some(run.id), json!({"question_id": qid, "via": via, "pr_number": q.pr_number, "action": q.action})).await;
    // Update the chat message so the other surface shows the answer.
    let summary = match (&q.decision, &q.answer) {
        (Some(d), a) if is_plan => {
            let what = match d.as_str() {
                "approved" => "approved, building",
                "changes" => "changes requested",
                _ => "not built",
            };
            let note = a.as_deref().filter(|a| !a.trim().is_empty()).map(|a| format!(": {a}")).unwrap_or_default();
            format!("{} — {what} by {who} via {via}{note}", q.text.lines().next().unwrap_or("Plan"))
        }
        (Some(d), _) => format!("{}: {} — {d} by {who} via {via}", if q.kind == "approval" { "Approval" } else { "Question" }, q.text),
        (None, Some(a)) => format!("Question: {} — answered by {who} via {via}: {a}", q.text),
        _ => format!("Question: {} — answered by {who}", q.text),
    };
    state.jobs.push(Job::ChatUpdate { question_id: qid, text: summary }, Some(format!("chat-update:{qid}"))).await?;
    let text = match (&q.decision, &q.answer) {
        (Some(d), Some(a)) => format!("Approval for '{}' was {d} by {who}. {a}", q.action.clone().unwrap_or_default()),
        (Some(d), None) => format!("Approval for '{}' was {d} by {who}.", q.action.clone().unwrap_or_default()),
        (None, Some(a)) => a.clone(),
        _ => String::new(),
    };
    let kind = if q.kind == "approval" { InputKind::Approval } else { InputKind::Answer };
    if run.blocked_reason.is_some() {
        sqlx::query("UPDATE runs SET blocked_reason=NULL WHERE id=$1").bind(run.id).execute(&state.pool).await?;
    }
    if is_plan {
        plan_decision(state, &run, &q, &who).await?;
        return Ok(q);
    }
    if run.waiting_for.as_deref() == Some("cap_decision") && q.action.as_deref() == Some("review_limit") {
        // Treat an answer to the review-limit question as the cap decision.
        let ends = answer.map(|a| a.to_lowercase().contains("end") || a.to_lowercase().contains("stop")).unwrap_or(false);
        engine::cap_decision(state, &run, if ends { "end" } else { "raise" }, None, None, None, &who, Some(user)).await?;
        return Ok(q);
    }
    engine::deliver_input(state, &run, kind, &text, json!({"question_id": qid, "decision": q.decision, "answer": q.answer, "by": who}), &format!("answer:{qid}"), true).await?;
    Ok(q)
}

/// Has a human approved this action on this PR for this run?
pub async fn approved(state: &AppState, run_id: Uuid, action: &str, pr: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM questions WHERE run_id=$1 AND kind='approval' AND action=$2 AND pr_number=$3 AND decision='approved')")
        .bind(run_id)
        .bind(action)
        .bind(pr)
        .fetch_one(&state.pool)
        .await?)
}

// ---------------------------------------------------------------------------
// Incoming events
// ---------------------------------------------------------------------------

async fn first_delivery(state: &AppState, source: &str, id: &str) -> Result<bool> {
    Ok(sqlx::query("INSERT INTO webhook_deliveries(id, source) VALUES ($1,$2) ON CONFLICT DO NOTHING")
        .bind(format!("{source}:{id}"))
        .bind(source)
        .execute(&state.pool)
        .await?
        .rows_affected()
        > 0)
}

pub async fn active_run_for(state: &AppState, work_item_key: &str) -> Result<Option<Run>> {
    Ok(sqlx::query_as("SELECT * FROM runs WHERE work_item_key=$1 AND state NOT IN ('finished','failed','cancelled','expired')")
        .bind(work_item_key)
        .fetch_optional(&state.pool)
        .await?)
}

pub async fn handle_repo_event(state: &AppState, connector_id: Uuid, ev: RepoEvent) -> Result<()> {
    if !first_delivery(state, &format!("repo:{connector_id}"), &ev.id).await? {
        return Ok(());
    }
    let identity = connectors::service_identity(state, connector_id).await.unwrap_or_else(|| "factory-bot".into());
    if ev.author.as_deref() == Some(identity.as_str()) {
        // Never answer ourselves.
        return Ok(());
    }
    let repos: Vec<(Uuid, Uuid, bool)> = sqlx::query_as("SELECT id, project_id, monitor_prs FROM repositories WHERE connector_id=$1 AND full_name=$2")
        .bind(connector_id)
        .bind(&ev.repo)
        .fetch_all(&state.pool)
        .await?;
    let Some(n) = ev.pr_number else { return Ok(()) };
    for (repo_id, project_id, monitor) in repos {
        // The run whose PR this is: a ticket build that opened it, or a review of it.
        let run: Option<Run> = sqlx::query_as(
            "SELECT * FROM runs WHERE repo_id=$1 AND pr_number=$2 AND state NOT IN ('finished','failed','cancelled','expired') ORDER BY created_at DESC LIMIT 1",
        )
        .bind(repo_id)
        .bind(n)
        .fetch_optional(&state.pool)
        .await?;
        let Some(run) = run else {
            if monitor && ev.kind == "pr_opened" {
                let pc = engine::project_cfg(state, project_id).await?;
                if pc.enabled_workflows.iter().any(|w| w == "pr_review") {
                    let req = NewRun { project_id, workflow: "pr_review".into(), repo: Some(ev.repo.clone()), pr_number: Some(n), ..Default::default() };
                    match engine::create_run(state, req, "repo-monitor", None).await {
                        Ok(_) | Err(engine::CreateError::AlreadyActive(_)) => {}
                        Err(e) => tracing::info!("not starting review for {}#{n}: {e}", ev.repo),
                    }
                }
            }
            continue;
        };
        on_pr_event(state, &run, &ev).await?;
    }
    Ok(())
}

async fn on_pr_event(state: &AppState, run: &Run, ev: &RepoEvent) -> Result<()> {
    let workflow = run.workflow();
    let author = ev.author.clone().unwrap_or_default();
    let limit = run.data.get("review_round_limit").and_then(|v| v.as_i64()).unwrap_or(5) as i32;
    match (ev.kind.as_str(), ev.state.as_deref()) {
        ("merged", _) => {
            engine::touch_activity(state, run.id).await?;
            engine::finish(state, run.id, "merged", "the PR was merged").await?;
        }
        ("closed", _) => {
            engine::touch_activity(state, run.id).await?;
            engine::cancel(state, run, "pr_closed", &author, None).await?;
        }
        ("review", Some("approved")) => {
            engine::touch_activity(state, run.id).await?;
            events::append(&state.pool, &*state.blobs, run.id, "pr_approved", Some(&author), json!({"pr_number": ev.pr_number})).await?;
            engine::finish(state, run.id, "approved", &format!("the PR was approved by {author}")).await?;
        }
        ("review", _) | ("comment", _) | ("thread_reply", _) if workflow == WorkflowKind::TicketBuild => {
            let is_round = ev.kind == "review";
            if is_round {
                let r: i32 = sqlx::query_scalar("UPDATE runs SET review_rounds = review_rounds + 1 WHERE id=$1 RETURNING review_rounds")
                    .bind(run.id)
                    .fetch_one(&state.pool)
                    .await?;
                if r > limit {
                    let run = runs::get(state, run.id).await?;
                    review_limit_reached(state, &run, limit).await?;
                }
            }
            let run = runs::get(state, run.id).await?;
            let text = format!("{} on PR #{} by {author}: {}", ev.kind, ev.pr_number.unwrap_or(0), ev.body.clone().unwrap_or_default());
            engine::deliver_input(state, &run, InputKind::Review, &text, serde_json::to_value(ev)?, &format!("repo:{}", ev.id), true).await?;
        }
        ("ci_completed", Some(c)) if workflow == WorkflowKind::TicketBuild && c != "success" => {
            let text = format!("CI check {} finished with {c} on PR #{}", ev.body.clone().unwrap_or_default(), ev.pr_number.unwrap_or(0));
            engine::deliver_input(state, run, InputKind::Ci, &text, serde_json::to_value(ev)?, &format!("repo:{}", ev.id), false).await?;
        }
        ("pushed", _) | ("thread_reply", _) | ("comment", _) if workflow == WorkflowKind::PrReview => {
            engine::touch_activity(state, run.id).await?;
            let r: i32 = sqlx::query_scalar("UPDATE runs SET review_rounds = review_rounds + 1 WHERE id=$1 RETURNING review_rounds")
                .bind(run.id)
                .fetch_one(&state.pool)
                .await?;
            let run = runs::get(state, run.id).await?;
            if r >= limit {
                pr_review_limit(state, &run, limit).await?;
                return Ok(());
            }
            let kind = if ev.kind == "pushed" { InputKind::Push } else { InputKind::Review };
            let text = format!("{} on PR #{} by {author}. Re-review: check earlier threads and resolve those addressed.", ev.kind, ev.pr_number.unwrap_or(0));
            engine::deliver_input(state, &run, kind, &text, serde_json::to_value(ev)?, &format!("repo:{}", ev.id), true).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn review_limit_reached(state: &AppState, run: &Run, limit: i32) -> Result<()> {
    runs::transition(state, run.id, RunState::Waiting, Some("cap_decision"), "review round limit", None).await?;
    ask(state, run, &format!("review-limit:{}", run.review_rounds), "question", &format!(
        "The PR has gone past {limit} review rounds. How should I proceed? Reply 'continue' to keep going or 'end' to stop the run."
    ), Some("review_limit"), run.pr_number).await?;
    Ok(())
}

async fn pr_review_limit(state: &AppState, run: &Run, limit: i32) -> Result<()> {
    let r = engine::repo(state, run.repo_id.unwrap()).await?;
    let host = connectors::repo_host(state, r.connector_id).await?;
    let d = host.get_pr(&r.full_name, run.pr_number.unwrap()).await?;
    let open: Vec<String> = d
        .threads
        .iter()
        .filter(|t| t.status.is_none())
        .map(|t| format!("- {}:{} {}", t.path.clone().unwrap_or_default(), t.line.unwrap_or(0), t.comments.first().map(|c| c.body.clone()).unwrap_or_default()))
        .collect();
    let text = format!(
        "Review round limit ({limit}) reached; the factory stops reviewing this PR.\nStill open:\n{}",
        if open.is_empty() { "- nothing".to_string() } else { open.join("\n") }
    );
    let n = run.pr_number.unwrap();
    let name = r.full_name.clone();
    let marker = format!("<!-- factory:review-limit:{} -->", run.id);
    let body = format!("{text}\n\n{marker}");
    once(state, &format!("review-limit-comment:{}", run.id), || async move {
        let d = host.get_pr(&name, n).await?;
        if d.comments.iter().any(|c| c.body.contains(&marker)) {
            return Ok(json!({}));
        }
        host.comment(&name, n, &body, None, None).await
    })
    .await?;
    engine::finish(state, run.id, "review_round_limit", &text).await
}

pub async fn handle_ticket_event(state: &AppState, connector_id: Uuid, ev: TicketEvent) -> Result<()> {
    if !first_delivery(state, &format!("ticket:{connector_id}"), &ev.id).await? {
        return Ok(());
    }
    let identity = connectors::service_identity(state, connector_id).await.unwrap_or_else(|| "factory-bot".into());
    if ev.author.as_deref() == Some(identity.as_str()) {
        return Ok(());
    }
    let key = format!("ticket:{connector_id}:{}", ev.key);
    if let Some(run) = active_run_for(state, &key).await? {
        match ev.kind.as_str() {
            "cancelled" => engine::cancel(state, &run, "ticket_cancelled", ev.author.as_deref().unwrap_or("ticket"), None).await?,
            "transitioned" if matches!(ev.status.as_deref().map(|s| s.to_lowercase()), Some(ref s) if s == "cancelled" || s == "won't do" || s == "wont_do") => {
                engine::cancel(state, &run, "ticket_cancelled", ev.author.as_deref().unwrap_or("ticket"), None).await?
            }
            "commented" => {
                let text = format!("Comment on {} by {}: {}", ev.key, ev.author.clone().unwrap_or_default(), ev.body.clone().unwrap_or_default());
                engine::deliver_input(state, &run, InputKind::Other, &text, serde_json::to_value(&ev)?, &format!("ticket:{}", ev.id), true).await?;
            }
            _ => {}
        }
        return Ok(());
    }
    // Trigger: a label or a transition, per project configuration.
    let projects: Vec<(Uuid, Value)> = sqlx::query_as("SELECT id, config FROM projects").fetch_all(&state.pool).await?;
    for (pid, cfg) in projects {
        let pc = crate::settings::project_config(&cfg);
        if pc.ticket_connector_id != Some(connector_id) {
            continue;
        }
        let hit = match ev.kind.as_str() {
            "labeled" => pc.jira_trigger.label.is_some() && ev.label == pc.jira_trigger.label,
            "transitioned" => pc.jira_trigger.transition.is_some() && ev.status == pc.jira_trigger.transition,
            _ => false,
        };
        if hit {
            let req = NewRun { project_id: pid, workflow: "ticket_build".into(), ticket_key: Some(ev.key.clone()), ..Default::default() };
            match engine::create_run(state, req, &format!("ticket-trigger:{}", ev.author.clone().unwrap_or_default()), None).await {
                Ok(_) | Err(engine::CreateError::AlreadyActive(_)) => {}
                Err(e) => tracing::info!("ticket trigger for {} refused: {e}", ev.key),
            }
        }
    }
    Ok(())
}

/// The factory user behind a chat user: a linked identity, or else the factory user with the
/// same email address, which is then linked so the lookup is quick next time.
async fn resolve_chat_user(state: &AppState, connector_id: Uuid, provider: &str, external_id: &str) -> Result<Option<Uuid>> {
    if let Some(u) = user_for_identity(state, provider, external_id).await? {
        return Ok(Some(u));
    }
    let chat = connectors::chat(state, connector_id).await?;
    let email = match chat.user_email(external_id).await {
        Ok(Some(e)) => e,
        Ok(None) => return Ok(None),
        Err(e) => {
            tracing::info!("could not look up the email of chat user {external_id}: {e:#}");
            return Ok(None);
        }
    };
    let Some((user, user_email)) = sqlx::query_as::<_, (Uuid, String)>("SELECT id, email FROM users WHERE lower(email)=lower($1)").bind(&email).fetch_optional(&state.pool).await? else {
        return Ok(None);
    };
    sqlx::query("INSERT INTO identities(user_id, provider, external_id) VALUES ($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(user)
        .bind(provider)
        .bind(external_id)
        .execute(&state.pool)
        .await?;
    crate::audit::record(&state.pool, "factory", None, "identity.auto_linked", None, None, json!({"user": user_email, "provider": provider, "external_id": external_id})).await;
    tracing::info!("linked {provider} user {external_id} to {user_email} by email");
    Ok(Some(user))
}

/// Tell a chat user, in the run's thread, why their action had no effect.
async fn chat_feedback(state: &AppState, run_id: Uuid, event_id: &str, chat_user: &str, text: &str) -> Result<()> {
    let text = format!("<@{chat_user}> {text}");
    state
        .jobs
        .push(Job::ChatPost { run_id, key: format!("feedback:{event_id}"), text, question_id: None, ops: false }, Some(format!("chat:feedback:{event_id}")))
        .await?;
    Ok(())
}

const UNLINKED: &str = "I couldn't match your Slack account to a factory user, so nothing was recorded. Ask an admin to add your Slack member ID";

pub async fn handle_chat_event(state: &AppState, connector_id: Uuid, ev: ChatEvent) -> Result<()> {
    if !first_delivery(state, &format!("chat:{connector_id}"), &ev.id).await? {
        return Ok(());
    }
    if ev.user == "factory-bot" {
        return Ok(());
    }
    let user = resolve_chat_user(state, connector_id, "slack", &ev.user).await?;
    // Button press on an approval.
    if let Some((qid, decision)) = &ev.action {
        let Ok(q) = qid.parse::<Uuid>() else { return Ok(()) };
        let run_id: Option<Uuid> = sqlx::query_scalar("SELECT run_id FROM questions WHERE id=$1").bind(q).fetch_optional(&state.pool).await?;
        let Some(run_id) = run_id else { return Ok(()) };
        let Some(u) = user else {
            tracing::info!("button press from unlinked chat user {}", ev.user);
            return chat_feedback(state, run_id, &ev.id, &ev.user, &format!("{UNLINKED} ({}) under Users → Identities, then press the button again.", ev.user)).await;
        };
        if let Err(e) = answer(state, q, u, "slack", None, Some(decision)).await {
            tracing::info!("approval from chat refused: {e:#}");
            return chat_feedback(state, run_id, &ev.id, &ev.user, &format!("Your decision wasn't recorded: {e:#}.")).await;
        }
        return Ok(());
    }
    let text = ev.text.trim().to_string();
    let Some(thread) = &ev.thread_ts else {
        // Top-level message: manual trigger, e.g. "start ticket_build PROJ-1 [in PROJECT]".
        return chat_command(state, user, &text).await;
    };
    let run: Option<Run> = sqlx::query_as("SELECT * FROM runs WHERE chat_thread->>'channel'=$1 AND (chat_thread->>'ts'=$2 OR chat_thread->>'thread_ts'=$2) ORDER BY created_at DESC LIMIT 1")
        .bind(&ev.channel)
        .bind(thread)
        .fetch_optional(&state.pool)
        .await?;
    let Some(run) = run else { return Ok(()) };
    let Some(u) = user else {
        tracing::info!("chat message from unmapped user {}", ev.user);
        if !run.state().is_terminal() {
            return chat_feedback(state, run.id, &ev.id, &ev.user, &format!("{UNLINKED} ({}) under Users → Identities, then send it again.", ev.user)).await;
        }
        return Ok(());
    };
    let lower = text.to_lowercase();
    if lower == "revive" && run.state() == RunState::Expired {
        if has_role(state, u, Some(run.project_id), "operate").await? {
            let who = user_label(state, u).await;
            engine::revive(state, &run, &who, Some(u)).await?;
        }
        return Ok(());
    }
    let open: Option<Question> = sqlx::query_as("SELECT * FROM questions WHERE run_id=$1 AND state='open' ORDER BY created_at DESC LIMIT 1")
        .bind(run.id)
        .fetch_optional(&state.pool)
        .await?;
    if let Some(q) = open {
        let r = if q.kind == "approval" && q.action.as_deref() == Some("plan") {
            // Any reply decides. A short yes ("ok", "Yes, go ahead") approves, a short no ("no
            // thanks", "don't build") declines, and anything else is the changes wanted.
            let d = plan_reply_decision(&lower);
            answer(state, q.id, u, "slack", Some(&text), Some(d)).await.map(|_| ())
        } else if q.kind == "approval" {
            let d = if lower.starts_with("approve") || lower == "yes" || lower == "lgtm" { Some("approved") } else if lower.starts_with("reject") || lower == "no" { Some("rejected") } else { None };
            match d {
                Some(d) => answer(state, q.id, u, "slack", Some(&text), Some(d)).await.map(|_| ()),
                None => Ok(()),
            }
        } else {
            answer(state, q.id, u, "slack", Some(&text), None).await.map(|_| ())
        };
        if let Err(e) = r {
            tracing::info!("answer from chat refused: {e:#}");
            return chat_feedback(state, run.id, &ev.id, &ev.user, &format!("Your reply wasn't recorded: {e:#}.")).await;
        }
        return Ok(());
    }
    if run.state().is_terminal() {
        return Ok(());
    }
    if has_role(state, u, Some(run.project_id), "operate").await? || run.assignee_user_id == Some(u) {
        let interrupt = lower.starts_with("!interrupt") || lower.starts_with("interrupt now");
        let who = user_label(state, u).await;
        let msg = text.trim_start_matches("!interrupt").trim().to_string();
        engine::steer(state, &run, &msg, interrupt, &who, Some(u), "slack").await?;
    }
    Ok(())
}

async fn chat_command(state: &AppState, user: Option<Uuid>, text: &str) -> Result<()> {
    let mut parts = text.split_whitespace();
    if parts.next().map(|s| s.to_lowercase()) != Some("start".into()) {
        return Ok(());
    }
    let Some(u) = user else { return Ok(()) };
    let workflow = parts.next().unwrap_or("").to_string();
    let target = parts.next().map(String::from);
    let mut project_key = None;
    let rest: Vec<&str> = parts.collect();
    if let Some(i) = rest.iter().position(|w| *w == "in") {
        project_key = rest.get(i + 1).map(|s| s.to_string());
    }
    let project: Option<Uuid> = match project_key {
        Some(k) => sqlx::query_scalar("SELECT id FROM projects WHERE key=$1").bind(k).fetch_optional(&state.pool).await?,
        None => sqlx::query_scalar("SELECT id FROM projects ORDER BY created_at LIMIT 1").fetch_optional(&state.pool).await?,
    };
    let Some(pid) = project else { return Ok(()) };
    if !has_role(state, u, Some(pid), "operate").await? {
        return Ok(());
    }
    let mut req = NewRun { project_id: pid, workflow: workflow.clone(), ..Default::default() };
    match workflow.as_str() {
        "ticket_build" => req.ticket_key = target,
        "pr_review" => {
            if let Some(t) = target {
                if let Some((r, n)) = t.rsplit_once('#') {
                    req.repo = Some(r.to_string());
                    req.pr_number = n.parse().ok();
                }
            }
        }
        _ => req.title = target,
    }
    let who = user_label(state, u).await;
    if let Err(e) = engine::create_run(state, req, &format!("slack:{who}"), Some(u)).await {
        tracing::info!("chat start refused: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod plan_reply_tests {
    use super::plan_reply_decision as d;

    #[test]
    fn short_yes_approves_short_no_declines_anything_else_is_changes() {
        for a in ["ok", "OK", "Yes, go ahead", "go", "lgtm!", "approve", "Approve plan", "ship it", "Proceed", "build it"] {
            assert_eq!(d(a), "approved", "{a}");
        }
        for r in ["no", "no thanks", "Don't build", "dont build it", "reject", "decline"] {
            assert_eq!(d(r), "rejected", "{r}");
        }
        for c in ["ok but please store the zips in S3 instead", "Use the existing job queue", "no need for a UI, an API endpoint is enough for now"] {
            assert_eq!(d(c), "changes", "{c}");
        }
    }
}
