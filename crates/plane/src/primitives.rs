//! Primitive execution. Every call is authenticated (by the API layer with
//! the run token), authorised against the run's project and repositories,
//! made idempotent where it has side effects, and audited.

use crate::connectors::{self, RepoHost};
use crate::engine::{self, RepoRow};
use crate::runs::{self, Run};
use crate::{AppState, events, humans};
use anyhow::{Result, anyhow, bail};
use proto::WorkflowKind;
use proto::primitives::{self as cat, PrimitiveResponse};
use serde_json::{Value, json};
use std::sync::Arc;

/// A refusal by policy (as opposed to a failure).
#[derive(Debug, thiserror::Error)]
#[error("refused by policy ({rule}): {message}")]
pub struct Denied {
    pub rule: &'static str,
    pub message: String,
}

pub fn deny(rule: &'static str, message: impl Into<String>) -> anyhow::Error {
    Denied { rule, message: message.into() }.into()
}

struct Ctx<'a> {
    state: &'a AppState,
    run: Run,
    args: &'a Value,
    key: &'a str,
}

impl Ctx<'_> {
    fn s(&self, k: &str) -> Option<String> {
        self.args.get(k).and_then(|v| v.as_str()).map(String::from).filter(|s| !s.is_empty())
    }
    fn req(&self, k: &str) -> Result<String> {
        self.s(k).ok_or_else(|| anyhow!("missing argument {k}"))
    }
    fn i(&self, k: &str) -> Option<i64> {
        self.args.get(k).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
    }
    /// Resolve the repository argument against the project's allowlist.
    async fn repo(&self) -> Result<(RepoRow, Arc<dyn RepoHost>)> {
        let r = match self.s("repo") {
            Some(name) => engine::repo_by_name(self.state, self.run.project_id, &name)
                .await?
                .ok_or_else(|| deny("repo_allowlist", format!("repository {name} is not on this project's allowlist")))?,
            None => {
                let id = self.run.repo_id.ok_or_else(|| anyhow!("the run has no repository; pass repo"))?;
                engine::repo(self.state, id).await?
            }
        };
        let host = connectors::repo_host(self.state, r.connector_id).await?;
        crate::audit::record(&self.state.pool, &format!("run:{}", self.run.id), None, "connector.repo.use", Some(self.run.project_id), Some(self.run.id), json!({"connector": r.connector_id, "repo": r.full_name})).await;
        Ok((r, host))
    }
    fn pr_number(&self) -> Result<i64> {
        self.i("number").or(self.run.pr_number).ok_or_else(|| anyhow!("no PR number given and the run has no PR"))
    }
    async fn tickets(&self) -> Result<Arc<dyn connectors::TicketSource>> {
        let pc = engine::project_cfg(self.state, self.run.project_id).await?;
        let id = pc.ticket_connector_id.ok_or_else(|| anyhow!("the project has no ticket source"))?;
        crate::audit::record(&self.state.pool, &format!("run:{}", self.run.id), None, "connector.ticket.use", Some(self.run.project_id), Some(self.run.id), json!({"connector": id})).await;
        connectors::ticket_source(self.state, id).await
    }
    fn ticket_key(&self) -> Result<String> {
        self.s("key").or(self.run.ticket_key.clone()).ok_or_else(|| anyhow!("no ticket key"))
    }
    fn marker(&self) -> String {
        format!("<!-- factory:{}:{} -->", self.run.short(), self.key.chars().take(24).collect::<String>())
    }
    fn review_only(&self) -> bool {
        self.run.workflow() == WorkflowKind::PrReview
    }
}

pub async fn call(state: &AppState, run: &Run, name: &str, args: Value, key: &str, instruction_id: Option<i64>) -> PrimitiveResponse {
    let Some(def) = cat::find(name) else {
        return PrimitiveResponse { ok: false, result: Value::Null, error: Some(format!("unknown primitive {name}")), ends_turn: false, replayed: false };
    };
    if run.state().is_terminal() {
        return PrimitiveResponse { ok: false, result: Value::Null, error: Some(format!("run is {}", run.state)), ends_turn: false, replayed: false };
    }
    // Idempotency for side effects.
    if def.side_effect {
        let existing: Option<(String, Option<Value>)> = match sqlx::query_as(
            "WITH ins AS (INSERT INTO primitive_calls(run_id, idem_key, name, args, status, instruction_id) VALUES ($1,$2,$3,$4,'pending',$5)
                          ON CONFLICT DO NOTHING RETURNING 1)
             SELECT status, response FROM primitive_calls WHERE run_id=$1 AND idem_key=$2 AND NOT EXISTS (SELECT 1 FROM ins)",
        )
        .bind(run.id)
        .bind(key)
        .bind(name)
        .bind(&args)
        .bind(instruction_id)
        .fetch_optional(&state.pool)
        .await
        {
            Ok(v) => v,
            Err(e) => return PrimitiveResponse { ok: false, result: Value::Null, error: Some(format!("database: {e}")), ends_turn: false, replayed: false },
        };
        if let Some((status, Some(resp))) = existing {
            if status == "done" {
                if let Ok(mut r) = serde_json::from_value::<PrimitiveResponse>(resp) {
                    r.replayed = true;
                    let _ = events::append(&state.pool, &*state.blobs, run.id, "primitive_call", Some("agent"), json!({"name": name, "args": args, "ok": r.ok, "replayed": true, "side_effect": true})).await;
                    crate::metrics::PRIMITIVES.with_label_values(&[name, "replayed"]).inc();
                    return r;
                }
            }
        }
    }
    let ctx = Ctx { state, run: run.clone(), args: &args, key };
    let out = exec(&ctx, name).await;
    let resp = match out {
        Ok((result, ends_turn)) => PrimitiveResponse { ok: true, result, error: None, ends_turn, replayed: false },
        Err(e) => {
            if let Some(d) = e.downcast_ref::<Denied>() {
                crate::metrics::POLICY_DENIALS.with_label_values(&[d.rule]).inc();
                crate::audit::record(&state.pool, &format!("run:{}", run.id), None, "policy.denied", Some(run.project_id), Some(run.id), json!({"primitive": name, "rule": d.rule, "message": d.message, "args": args})).await;
            }
            PrimitiveResponse { ok: false, result: Value::Null, error: Some(format!("{e:#}")), ends_turn: false, replayed: false }
        }
    };
    if def.side_effect {
        // Failures are not stored, so a retry can succeed.
        if resp.ok {
            let _ = sqlx::query("UPDATE primitive_calls SET status='done', response=$3 WHERE run_id=$1 AND idem_key=$2")
                .bind(run.id)
                .bind(key)
                .bind(serde_json::to_value(&resp).unwrap_or_default())
                .execute(&state.pool)
                .await;
        } else {
            let _ = sqlx::query("DELETE FROM primitive_calls WHERE run_id=$1 AND idem_key=$2 AND status='pending'")
                .bind(run.id)
                .bind(key)
                .execute(&state.pool)
                .await;
        }
    }
    crate::metrics::PRIMITIVES.with_label_values(&[name, if resp.ok { "ok" } else { "error" }]).inc();
    let _ = events::append(
        &state.pool,
        &*state.blobs,
        run.id,
        "primitive_call",
        Some("agent"),
        json!({"name": name, "args": args, "ok": resp.ok, "result": resp.result, "error": resp.error, "side_effect": def.side_effect, "ends_turn": resp.ends_turn, "instruction_id": instruction_id}),
    )
    .await;
    crate::audit::record(&state.pool, &format!("run:{}", run.id), None, &format!("primitive.{name}"), Some(run.project_id), Some(run.id), json!({"args": args, "ok": resp.ok, "error": resp.error})).await;
    if resp.ok && resp.ends_turn {
        if let Err(e) = engine::end_turn_now(state, run.id, name).await {
            tracing::warn!(run = %run.id, "could not stop the turn after {name}: {e:#}");
        }
    }
    resp
}

async fn exec(c: &Ctx<'_>, name: &str) -> Result<(Value, bool)> {
    let st = c.state;
    let run = &c.run;
    let v = match name {
        // ---- repos ----
        "repo.search" => {
            let (r, h) = c.repo().await?;
            json!({"matches": h.search(&r.full_name, &c.req("query")?).await?})
        }
        "repo.read_file" => {
            let (r, h) = c.repo().await?;
            let git_ref = c.s("ref").unwrap_or(r.default_branch.clone());
            let path = c.req("path")?;
            match h.read_file(&r.full_name, &git_ref, &path).await? {
                Some(content) => json!({"path": path, "ref": git_ref, "content": content}),
                None => return Err(anyhow!("{path} not found at {git_ref}")),
            }
        }
        "repo.list_files" => {
            let (r, h) = c.repo().await?;
            let git_ref = c.s("ref").unwrap_or(r.default_branch.clone());
            json!({"ref": git_ref, "files": h.list_files(&r.full_name, &git_ref).await?})
        }
        "repo.clone_info" => {
            let (r, _) = c.repo().await?;
            json!({"clone_path": format!("/git/{}", r.id), "branch": run.branch, "namespace": run.namespace(), "base_branch": r.default_branch, "wip_ref": run.wip_ref()})
        }
        "repo.create_branch" => {
            if c.review_only() {
                return Err(deny("review_no_push", "the PR review workflow never pushes"));
            }
            let (r, _) = c.repo().await?;
            let name = c.req("name")?;
            crate::git::check_ref(run, &r, &format!("refs/heads/{name}"), false)?;
            let from = c.s("from").unwrap_or(r.default_branch.clone());
            crate::git::create_branch(st, &r, &name, &from).await?;
            json!({"branch": name})
        }
        // ---- tickets ----
        "ticket.search" => json!({"tickets": c.tickets().await?.search(&c.req("query")?).await?}),
        "ticket.get" => serde_json::to_value(c.tickets().await?.get(&c.ticket_key()?).await?)?,
        "ticket.transition" => {
            let key = c.ticket_key()?;
            c.tickets().await?.transition(&key, &c.req("to")?).await?;
            json!({"key": key, "status": c.req("to")?})
        }
        "ticket.comment" => {
            let key = c.ticket_key()?;
            let t = c.tickets().await?;
            let marker = format!("factory:{}", c.key);
            let cm = match t.find_comment(&key, &marker).await? {
                Some(cm) => cm,
                None => t.comment(&key, &c.req("body")?, &marker).await?,
            };
            json!({"key": key, "comment_id": cm.id})
        }
        // ---- pull requests ----
        "pr.open" => {
            if c.review_only() {
                return Err(deny("review_no_push", "the PR review workflow cannot open PRs"));
            }
            let (r, h) = c.repo().await?;
            let head = c.s("head").or(run.branch.clone()).ok_or_else(|| anyhow!("no head branch"))?;
            crate::git::check_ref(run, &r, &format!("refs/heads/{head}"), false)?;
            let base = c.s("base").unwrap_or(r.default_branch.clone());
            let pr = match h.find_open_pr(&r.full_name, &head).await? {
                Some(pr) => pr,
                None => h.open_pr(&r.full_name, &head, &base, &c.req("title")?, &format!("{}\n\n{}", c.req("body")?, c.marker())).await?,
            };
            let updated = sqlx::query("UPDATE runs SET pr_number=$2, pr_url=$3 WHERE id=$1 AND (pr_number IS DISTINCT FROM $2)")
                .bind(run.id)
                .bind(pr.number)
                .bind(&pr.url)
                .execute(&st.pool)
                .await?;
            if updated.rows_affected() > 0 {
                add_artifact(st, run, "pr", &format!("PR #{}: {}", pr.number, pr.title), Some(&pr.url), None).await?;
                humans::notify(st, run, &format!("pr-opened:{}:{}", run.id, pr.number), &format!("Opened PR #{}: {}", pr.number, pr.url)).await?;
            }
            serde_json::to_value(&pr)?
        }
        "pr.get" => {
            let (r, h) = c.repo().await?;
            let d = h.get_pr(&r.full_name, c.pr_number()?).await?;
            json!({"pr": d.pr, "threads": d.threads, "reviews": d.reviews, "comments": d.comments, "review_decision": d.review_decision()})
        }
        "pr.list" => {
            let (r, h) = c.repo().await?;
            json!({"prs": h.list_prs(&r.full_name, &c.s("state").unwrap_or("open".into())).await?})
        }
        "pr.comment" => {
            let (r, h) = c.repo().await?;
            let n = c.pr_number()?;
            let marker = c.marker();
            let d = h.get_pr(&r.full_name, n).await?;
            let existing = d.comments.iter().find(|cm| cm.body.contains(&marker)).map(|cm| json!({"comment_id": cm.id}))
                .or_else(|| d.threads.iter().find(|t| t.comments.iter().any(|cm| cm.body.contains(&marker))).map(|t| json!({"thread_id": t.id})));
            match existing {
                Some(v) => v,
                None => h.comment(&r.full_name, n, &format!("{}\n\n{marker}", c.req("body")?), c.s("path").as_deref(), c.i("line")).await?,
            }
        }
        "pr.reply" => {
            let (r, h) = c.repo().await?;
            let n = c.pr_number()?;
            let tid = c.req("thread_id")?;
            let marker = c.marker();
            let d = h.get_pr(&r.full_name, n).await?;
            let t = d.threads.iter().find(|t| t.id == tid).ok_or_else(|| anyhow!("thread {tid} not found"))?;
            match t.comments.iter().find(|cm| cm.body.contains(&marker)) {
                Some(cm) => json!({"comment_id": cm.id}),
                None => h.reply(&r.full_name, n, &tid, &format!("{}\n\n{marker}", c.req("body")?)).await?,
            }
        }
        "pr.resolve_thread" => {
            let (r, h) = c.repo().await?;
            let status = c.s("status").unwrap_or("resolved".into());
            if status != "resolved" && status != "wont_fix" {
                return Err(anyhow!("status must be resolved or wont_fix"));
            }
            h.resolve_thread(&r.full_name, c.pr_number()?, &c.req("thread_id")?, &status).await?;
            json!({"thread_id": c.req("thread_id")?, "status": status})
        }
        "pr.review_status" => {
            let (r, h) = c.repo().await?;
            let d = h.get_pr(&r.full_name, c.pr_number()?).await?;
            json!({"state": d.pr.state, "review_decision": d.review_decision(), "reviews": d.reviews, "open_threads": d.threads.iter().filter(|t| t.status.is_none()).count()})
        }
        "pr.merge_status" => {
            let (r, h) = c.repo().await?;
            let d = h.get_pr(&r.full_name, c.pr_number()?).await?;
            let checks = h.checks(&r.full_name, d.pr.head_sha.as_deref().unwrap_or(&d.pr.head)).await?;
            let ci_ok = checks.iter().all(|c| c.conclusion.as_deref() == Some("success"));
            json!({"state": d.pr.state, "review_decision": d.review_decision(), "ci_ok": ci_ok, "mergeable": d.pr.state == "open" && d.review_decision() == "approved" && ci_ok})
        }
        "pr.merge" => {
            if c.review_only() {
                return Err(deny("review_no_merge", "the PR review workflow never merges"));
            }
            let (r, h) = c.repo().await?;
            let n = c.pr_number()?;
            let pc = engine::project_cfg(st, run.project_id).await?;
            if !pc.merge_by_agent {
                return Err(deny("merge_human_only", "merge is a human action for this project"));
            }
            if !humans::approved(st, run.id, "merge", n).await? {
                return Err(deny("merge_needs_approval", format!("no recorded human approval names PR #{n}")));
            }
            h.merge(&r.full_name, n).await?;
            json!({"merged": n})
        }
        // ---- CI ----
        "ci.status" => {
            let (r, h) = c.repo().await?;
            let git_ref = match (c.s("branch"), c.i("number").or(run.pr_number)) {
                (Some(b), _) => b,
                (None, Some(n)) => {
                    let d = h.get_pr(&r.full_name, n).await?;
                    d.pr.head_sha.unwrap_or(d.pr.head)
                }
                (None, None) => run.branch.clone().ok_or_else(|| anyhow!("give a branch or PR"))?,
            };
            json!({"ref": git_ref, "checks": h.checks(&r.full_name, &git_ref).await?})
        }
        "ci.logs" => {
            let (r, h) = c.repo().await?;
            json!({"check_id": c.req("check_id")?, "logs": h.check_logs(&r.full_name, &c.req("check_id")?).await?})
        }
        // ---- humans ----
        "human.message" => {
            humans::notify(st, run, &format!("msg:{}:{}", run.id, c.key), &c.req("text")?).await?;
            json!({"sent": true})
        }
        "human.ask" => {
            let q = humans::ask(st, run, c.key, "question", &c.req("question")?, None, None).await?;
            return Ok((json!({"question_id": q, "note": "End your turn now. The answer will start your next turn."}), true));
        }
        "human.request_approval" => {
            let action = c.req("action")?;
            let pr = c.i("pr").or(run.pr_number);
            let text = format!("{} — {}", action, c.req("summary")?);
            let q = humans::ask(st, run, c.key, "approval", &text, Some(&action), pr).await?;
            return Ok((json!({"question_id": q, "note": "End your turn now. The decision will start your next turn."}), true));
        }
        // ---- plan ----
        "plan.submit" => {
            if !run.in_plan_phase() {
                bail!(if run.phase.as_deref() == Some("build") {
                    "the plan is already approved; you are in the build phase. If the plan needs a material change, ask with human.ask"
                } else {
                    "this workflow has no plan phase"
                });
            }
            let plan = c.req("plan")?;
            let n: i64 = sqlx::query_scalar("SELECT count(*) FROM artifacts WHERE run_id=$1 AND kind='plan'").bind(run.id).fetch_one(&st.pool).await?;
            let version = n + 1;
            let title = format!("Plan v{version}");
            let artifact = add_artifact(st, run, "plan", &title, None, Some(&plan)).await?;
            events::append(&st.pool, &*st.blobs, run.id, "plan_submitted", Some("agent"), json!({"version": version, "artifact_id": artifact, "summary": c.s("summary")})).await?;
            let summary = c.s("summary").map(|s| format!("{s}\n\n")).unwrap_or_default();
            // Earlier versions still open for review are superseded by this one.
            let superseded: Vec<uuid::Uuid> = sqlx::query_scalar(
                "UPDATE questions SET state='answered', decision='superseded', answered_at=now() WHERE run_id=$1 AND action='plan' AND state='open' RETURNING id",
            )
            .bind(run.id)
            .fetch_all(&st.pool)
            .await?;
            for old in superseded {
                st.jobs.push(crate::jobs::Job::ChatUpdate { question_id: old, text: format!("Superseded by {title}.") }, Some(format!("chat-update:{old}"))).await?;
            }
            let text = format!("{title} for {}\n\n{summary}{}", run.title, humans::excerpt(&plan, 2500));
            let q = humans::ask(st, run, c.key, "approval", &text, Some("plan"), None).await?;
            return Ok((json!({"question_id": q, "version": version, "note": "Plan submitted. End your turn now. The reviewer's decision will start your next turn."}), true));
        }
        // ---- status ----
        "status.report" => {
            events::append(&st.pool, &*st.blobs, run.id, "status_report", Some("agent"), json!({"text": c.req("text")?})).await?;
            json!({"recorded": true})
        }
        "status.artifact" => {
            let id = add_artifact(st, run, &c.req("kind")?, &c.req("title")?, c.s("url").as_deref(), c.s("content").as_deref()).await?;
            json!({"artifact_id": id})
        }
        "status.finish" => {
            sqlx::query("UPDATE runs SET finish_declared=true WHERE id=$1").bind(run.id).execute(&st.pool).await?;
            events::append(&st.pool, &*st.blobs, run.id, "finish_declared", Some("agent"), json!({"summary": c.req("summary")?})).await?;
            json!({"recorded": true, "note": "The run ends at its workflow's end state."})
        }
        "status.blocked" => {
            let reason = c.req("reason")?;
            sqlx::query("UPDATE runs SET blocked_reason=$2 WHERE id=$1").bind(run.id).bind(&reason).execute(&st.pool).await?;
            humans::notify(st, run, &format!("blocked:{}:{}", run.id, c.key), &format!("The agent is blocked: {reason}")).await?;
            return Ok((json!({"recorded": true}), true));
        }
        other => return Err(anyhow!("primitive {other} is not implemented")),
    };
    Ok((v, false))
}

async fn add_artifact(st: &AppState, run: &Run, kind: &str, title: &str, url: Option<&str>, content: Option<&str>) -> Result<uuid::Uuid> {
    let id = humans::uuid_from(&format!("artifact:{}:{kind}:{title}:{}", run.id, url.unwrap_or("")));
    let n = sqlx::query("INSERT INTO artifacts(id, run_id, kind, title, url, content) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (id) DO NOTHING")
        .bind(id)
        .bind(run.id)
        .bind(kind)
        .bind(title)
        .bind(url)
        .bind(content)
        .execute(&st.pool)
        .await?;
    if n.rows_affected() > 0 {
        events::append(&st.pool, &*st.blobs, run.id, "artifact", Some("agent"), json!({"artifact_id": id, "kind": kind, "title": title, "url": url})).await?;
    }
    Ok(id)
}

pub async fn _reload(state: &AppState, run: &Run) -> Result<Run> {
    runs::get(state, run.id).await
}
