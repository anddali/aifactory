//! Rehearsal mode: a deterministic script per workflow type that calls the
//! real primitives and passes through the real gates. Needs no model.
//!
//! Every primitive call carries an idempotency key derived from the
//! instruction and step, and every commit has a deterministic date, so a
//! turn replayed after a crash produces the same commits and no duplicate
//! side effects.

use super::*;
use anyhow::{Context, anyhow, bail};
use proto::WorkflowKind;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Default, Serialize, Deserialize)]
struct State {
    turns: u32,
    phase: String,
    pr_number: Option<i64>,
    handled_threads: Vec<String>,
    notes: Vec<String>,
}

#[derive(Default)]
pub struct Scripted {
    state: State,
}

const FILE: &str = "scripted.json";

struct Step<'a, 'b> {
    ctx: &'a TurnCtx<'b>,
    input: &'a TurnInput,
    n: u32,
}

impl Step<'_, '_> {
    async fn call(&mut self, name: &str, args: serde_json::Value) -> Result<serde_json::Value> {
        self.n += 1;
        let key = format!("s:{}:{}:{}", self.input.instruction_id, self.n, name);
        let call_id = format!("c{}", self.n);
        let _ = self.ctx.events.send(HarnessEvent::ToolCall { call_id: call_id.clone(), name: name.into(), args: args.clone() });
        let r = self.ctx.ws.client.primitive(name, &args, Some(key), Some(self.input.instruction_id)).await?;
        let _ = self.ctx.events.send(HarnessEvent::ToolResult { call_id, ok: r.ok, output: if r.ok { r.result.clone() } else { json!({"error": r.error}) } });
        if !r.ok {
            bail!("{name} failed: {}", r.error.unwrap_or_default());
        }
        Ok(r.result)
    }

    fn say(&self, text: impl Into<String>) {
        let _ = self.ctx.events.send(HarnessEvent::AgentMessage { text: text.into() });
    }

    /// Commit all changes with a deterministic date and push the branch.
    async fn commit_push(&mut self, msg: &str) -> Result<()> {
        self.n += 1;
        let ws = self.ctx.ws;
        let branch = ws.spec.branch.clone().ok_or_else(|| anyhow!("no branch"))?;
        let date = format!("@{} +0000", 1_700_000_000i64 + self.input.instruction_id * 1000 + self.n as i64);
        let env = [("GIT_AUTHOR_DATE", date.clone()), ("GIT_COMMITTER_DATE", date)];
        let cmd = format!("git commit -m {msg:?}");
        if let Some(p) = self.ctx.deny.check(&cmd) {
            bail!("command refused by deny-list ({p})");
        }
        let _ = self.ctx.events.send(HarnessEvent::ToolCall { call_id: format!("g{}", self.n), name: "shell".into(), args: json!({"command": cmd}) });
        ws.git(&["add", "-A"]).await?;
        let status = ws.git(&["status", "--porcelain"]).await?;
        if !status.is_empty() {
            ws.git_env(&["commit", "-q", "-m", msg], &env).await?;
        }
        ws.git_net(&["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")]).await.context("push")?;
        let _ = self.ctx.events.send(HarnessEvent::ToolResult { call_id: format!("g{}", self.n), ok: true, output: json!({"pushed": branch}) });
        Ok(())
    }

    fn write(&self, rel: &str, content: &str) -> Result<()> {
        let p = self.ctx.ws.repo.join(rel);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, content)?;
        Ok(())
    }

    fn append(&self, rel: &str, line: &str) -> Result<()> {
        let p = self.ctx.ws.repo.join(rel);
        let mut s = std::fs::read_to_string(&p).unwrap_or_default();
        s.push_str(line);
        s.push('\n');
        self.write(rel, &s)
    }
}

fn fault(name: &str) -> bool {
    std::env::var("FACTORY_SCRIPTED_FAULT").ok().as_deref() == Some(name) && std::env::var("FACTORY_GENERATION").ok().as_deref() == Some("1")
}

fn slug(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect::<String>().trim_matches('-').to_string()
}

impl Scripted {
    fn load(&mut self, ws: &Workspace) {
        if let Ok(s) = std::fs::read_to_string(ws.session.join(FILE)) {
            if let Ok(st) = serde_json::from_str(&s) {
                self.state = st;
            }
        }
    }
    fn save(&self, ws: &Workspace) -> Result<()> {
        std::fs::create_dir_all(&ws.session)?;
        std::fs::write(ws.session.join(FILE), serde_json::to_string_pretty(&self.state)?)?;
        Ok(())
    }

    async fn ticket_build(&mut self, s: &mut Step<'_, '_>) -> Result<()> {
        let key = s.input.data.get("ticket_key").and_then(|v| v.as_str()).map(String::from);
        match s.input.kind {
            InputKind::Start => {
                let t = s.call("ticket.get", json!({})).await?;
                let files = s.call("repo.list_files", json!({})).await?;
                let n = files["files"].as_array().map(|a| a.len()).unwrap_or(0);
                let title = t["title"].as_str().unwrap_or("").to_string();
                s.say(format!("Researched {}: {title}. The repository has {n} files.", t["key"].as_str().unwrap_or("")));
                s.call("status.report", json!({"text": format!("Research done for {title}")})).await?;
                s.call("human.ask", json!({"question": format!("Plan for {}: add docs/changes/{}.md describing the change and wire it in. OK to proceed?", t["key"].as_str().unwrap_or(""), slug(t["key"].as_str().unwrap_or("change")))})).await?;
                self.state.phase = "asked".into();
            }
            InputKind::Answer | InputKind::Approval | InputKind::Steer | InputKind::Other | InputKind::CapDecision if self.state.pr_number.is_none() => {
                if s.input.kind == InputKind::Steer {
                    self.state.notes.push(s.input.text.clone());
                }
                let k = key.unwrap_or_else(|| "change".into());
                let rel = format!("docs/changes/{}.md", slug(&k));
                s.write(&rel, &format!("# {k}\n\nImplemented by the factory.\n\nDirection agreed: {}\n", s.input.text.lines().next().unwrap_or("")))?;
                for note in &self.state.notes {
                    s.append(&rel, &format!("- steering: {note}"))?;
                }
                s.commit_push(&format!("{k}: implement change")).await?;
                if fault("crash_after_push") {
                    eprintln!("scripted fault: crashing after push");
                    std::process::exit(137);
                }
                let pr = s.call("pr.open", json!({"title": format!("{k}: implement change"), "body": "Implements the ticket as agreed with the assignee."})).await?;
                let n = pr["number"].as_i64();
                self.state.pr_number = n;
                s.call("status.artifact", json!({"kind": "pr", "title": format!("PR #{}", n.unwrap_or(0)), "url": pr["url"]})).await?;
                s.say(format!("Opened PR #{}", n.unwrap_or(0)));
                self.state.phase = "pr_open".into();
            }
            InputKind::Review | InputKind::Push => {
                let pr = s.call("pr.get", json!({})).await?;
                let threads = pr["threads"].as_array().cloned().unwrap_or_default();
                let k = key.unwrap_or_else(|| "change".into());
                let rel = format!("docs/changes/{}.md", slug(&k));
                let mut addressed = vec![];
                for t in threads {
                    let id = t["id"].as_str().unwrap_or("").to_string();
                    if !t["status"].is_null() || self.state.handled_threads.contains(&id) {
                        continue;
                    }
                    let last = t["comments"].as_array().and_then(|c| c.last()).cloned().unwrap_or_default();
                    if last["body"].as_str().unwrap_or("").contains("<!-- factory:") {
                        continue;
                    }
                    s.append(&rel, &format!("- addressed review: {}", last["body"].as_str().unwrap_or("")))?;
                    addressed.push((id, last["body"].as_str().unwrap_or("").to_string()));
                }
                if s.input.data.get("kind").and_then(|v| v.as_str()) == Some("review") && addressed.is_empty() {
                    let body = s.input.data.get("body").and_then(|v| v.as_str()).unwrap_or("");
                    if !body.is_empty() {
                        s.append(&rel, &format!("- addressed review comment: {body}"))?;
                    }
                }
                s.commit_push("address review").await?;
                for (id, body) in &addressed {
                    s.call("pr.reply", json!({"thread_id": id, "body": format!("Done: {body}")})).await?;
                    s.call("pr.resolve_thread", json!({"thread_id": id, "status": "resolved"})).await?;
                    self.state.handled_threads.push(id.clone());
                }
                s.call("status.report", json!({"text": format!("Addressed {} review thread(s)", addressed.len())})).await?;
            }
            InputKind::Ci => {
                let st = s.call("ci.status", json!({})).await?;
                let failing: Vec<String> = st["checks"].as_array().cloned().unwrap_or_default().into_iter().filter(|c| c["conclusion"] == "failure").filter_map(|c| c["id"].as_str().map(String::from)).collect();
                for id in &failing {
                    let logs = s.call("ci.logs", json!({"check_id": id})).await?;
                    s.say(format!("CI {id} failed: {}", logs["logs"].as_str().unwrap_or("")));
                }
                let k = key.unwrap_or_else(|| "change".into());
                s.append(&format!("docs/changes/{}.md", slug(&k)), "- fixed CI failure")?;
                s.commit_push("fix CI").await?;
                s.call("pr.comment", json!({"body": format!("Fixed {} failing check(s).", failing.len())})).await?;
            }
            InputKind::Steer => {
                self.state.notes.push(s.input.text.clone());
                s.call("human.message", json!({"text": format!("Noted: {}", s.input.text)})).await?;
            }
            InputKind::Approval => {
                if s.input.data.get("decision").and_then(|v| v.as_str()) == Some("approved") {
                    s.call("pr.merge", json!({})).await?;
                }
            }
            _ => {
                s.call("status.report", json!({"text": "Continuing; waiting for review."})).await?;
            }
        }
        Ok(())
    }

    async fn pr_review(&mut self, s: &mut Step<'_, '_>) -> Result<()> {
        let pr = s.call("pr.get", json!({})).await?;
        let head = pr["pr"]["head"].as_str().unwrap_or("main").to_string();
        match s.input.kind {
            InputKind::Start => {
                let files = s.call("repo.list_files", json!({"ref": head})).await?;
                let first = files["files"].as_array().and_then(|a| a.iter().find(|f| !f.as_str().unwrap_or("").starts_with('.'))).and_then(|f| f.as_str()).unwrap_or("README.md").to_string();
                s.call("repo.read_file", json!({"path": first, "ref": head})).await?;
                s.call("pr.comment", json!({"path": first, "line": 1, "body": format!("Please add a short header comment to {first} explaining its purpose.")})).await?;
                s.call("pr.comment", json!({"body": "Review: one inline suggestion. Otherwise looks reasonable."})).await?;
                self.state.phase = "reviewed".into();
            }
            _ => {
                // Re-review: resolve our earlier threads that were addressed
                // (the author replied or pushed since).
                let mut resolved = 0;
                for t in pr["threads"].as_array().cloned().unwrap_or_default() {
                    if !t["status"].is_null() {
                        continue;
                    }
                    let comments = t["comments"].as_array().cloned().unwrap_or_default();
                    let ours = comments.first().map(|c| c["body"].as_str().unwrap_or("").contains("<!-- factory:")).unwrap_or(false);
                    let replied = comments.iter().skip(1).any(|c| !c["body"].as_str().unwrap_or("").contains("<!-- factory:"));
                    if ours && (replied || s.input.kind == InputKind::Push) {
                        s.call("pr.resolve_thread", json!({"thread_id": t["id"], "status": "resolved"})).await?;
                        resolved += 1;
                    }
                }
                s.call("pr.comment", json!({"body": format!("Re-review: resolved {resolved} thread(s) that were addressed.")})).await?;
            }
        }
        Ok(())
    }

    async fn report_then_fix(&mut self, s: &mut Step<'_, '_>, audit: bool) -> Result<()> {
        match s.input.kind {
            InputKind::Start => {
                let report = if audit {
                    let hits = s.call("repo.search", json!({"query": "password"})).await?;
                    let n = hits["matches"].as_array().map(|a| a.len()).unwrap_or(0);
                    format!("# Security audit findings\n\n1. [high] Hard-coded credential pattern found ({n} location(s)). Verified by search.\n2. [low] No SECURITY.md policy file.\n")
                } else {
                    let files = s.call("repo.list_files", json!({})).await?;
                    format!("# Report\n\nThe repository has {} files. Proposed change: add a CONTRIBUTING note.\n", files["files"].as_array().map(|a| a.len()).unwrap_or(0))
                };
                s.call("status.artifact", json!({"kind": "report", "title": if audit { "Ranked findings" } else { "Report" }, "content": report})).await?;
                s.call("human.ask", json!({"question": if audit { "Which findings should I fix? (e.g. '2', 'all', or 'none')" } else { "Report attached. Shall I make the change? (yes/no)" }})).await?;
            }
            InputKind::Answer => {
                let a = s.input.text.to_lowercase();
                let fix = !(a.contains("none") || a.starts_with("no"));
                if fix && s.ctx.ws.spec.branch.is_some() {
                    if audit {
                        s.write("SECURITY.md", "# Security policy\n\nReport vulnerabilities to the security team.\n")?;
                        s.commit_push("Add security policy (confirmed finding)").await?;
                    } else {
                        s.write("CONTRIBUTING.md", "# Contributing\n\nOpen a pull request.\n")?;
                        s.commit_push("Add contributing note").await?;
                    }
                    s.call("pr.open", json!({"title": if audit { "Fix confirmed security findings" } else { "Ad hoc change" }, "body": "Confirmed by a human before the change."})).await?;
                }
                s.call("status.finish", json!({"summary": if fix { "Fixed the confirmed items" } else { "Report delivered; no fixes requested" }})).await?;
            }
            _ => {
                s.call("status.report", json!({"text": "Noted."})).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Harness for Scripted {
    fn name(&self) -> &'static str {
        "scripted"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { resume: true, usage: false, interrupt: true }
    }
    async fn prepare(&mut self, ws: &Workspace) -> Result<()> {
        self.load(ws);
        Ok(())
    }
    fn session_files(&self) -> Vec<PathBuf> {
        vec![PathBuf::from(FILE)]
    }
    fn has_session(&self, ws: &Workspace) -> bool {
        ws.session.join(FILE).exists()
    }

    async fn run_turn(&mut self, ctx: TurnCtx<'_>, input: TurnInput) -> Result<TurnOutcome> {
        self.load(ctx.ws);
        let _ = ctx.events.send(HarnessEvent::ModelTurn { model: Some("scripted".into()), text: Some(format!("{:?} input", input.kind)) });
        if fault("hang_turn") {
            let mut rx = ctx.interrupt.clone();
            let _ = rx.wait_for(|v| *v).await;
            return Ok(TurnOutcome::Interrupted);
        }
        let workflow = ctx.ws.spec.workflow;
        let mut s = Step { ctx: &ctx, input: &input, n: 0 };
        let r = match workflow {
            WorkflowKind::TicketBuild => self.ticket_build(&mut s).await,
            WorkflowKind::PrReview => self.pr_review(&mut s).await,
            WorkflowKind::AdHoc => self.report_then_fix(&mut s, false).await,
            WorkflowKind::SecurityAudit => self.report_then_fix(&mut s, true).await,
        };
        self.state.turns += 1;
        self.save(ctx.ws)?;
        match r {
            Ok(()) => Ok(if *ctx.interrupt.borrow() { TurnOutcome::Interrupted } else { TurnOutcome::Completed }),
            Err(e) => {
                let msg = format!("{e:#}");
                let _ = ctx.events.send(HarnessEvent::Error { message: msg.clone() });
                Ok(if msg.contains("deny-list") { TurnOutcome::Denied } else { TurnOutcome::Failed })
            }
        }
    }
}
