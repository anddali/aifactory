//! Workflow types are defined in code; their instructions are Markdown
//! templates (with YAML front matter) versioned in the repository and
//! overridable per project.

use crate::AppState;
use crate::runs::Run;
use anyhow::Result;
use proto::WorkflowKind;
use serde_json::{Value, json};
use uuid::Uuid;

pub fn builtin(w: WorkflowKind) -> &'static str {
    match w {
        WorkflowKind::TicketBuild => include_str!("../../../templates/ticket_build.md"),
        WorkflowKind::PrReview => include_str!("../../../templates/pr_review.md"),
        WorkflowKind::AdHoc => include_str!("../../../templates/ad_hoc.md"),
        WorkflowKind::SecurityAudit => include_str!("../../../templates/security_audit.md"),
    }
}

pub fn version_of(body: &str) -> String {
    crate::crypto::sha256_hex(body.as_bytes())[..12].to_string()
}

/// The template a project uses for a workflow, and its version.
pub async fn template(state: &AppState, project_id: Uuid, w: WorkflowKind) -> Result<(String, String)> {
    let templates: Option<Value> = sqlx::query_scalar("SELECT templates FROM projects WHERE id=$1")
        .bind(project_id)
        .fetch_optional(&state.pool)
        .await?;
    let body = templates
        .and_then(|t| t.get(w.as_str()).and_then(|v| v.as_str()).map(String::from))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| builtin(w).to_string());
    let v = version_of(&body);
    let tag = if body == builtin(w) { "builtin" } else { "project" };
    Ok((body, format!("{tag}-{v}")))
}

/// Split YAML front matter from the Markdown body.
pub fn split_front_matter(t: &str) -> (Value, &str) {
    let t2 = t.trim_start();
    if let Some(rest) = t2.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            let body = &rest[end + 4..];
            let v: Value = serde_norway::from_str(fm).unwrap_or(json!({}));
            return (v, body.trim_start_matches(['\r', '\n']));
        }
    }
    (json!({}), t)
}

pub fn render(t: &str, vars: &[(&str, String)]) -> String {
    let (_, body) = split_front_matter(t);
    let mut out = body.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

/// The first input of a run: the rendered instructions and structured data.
pub async fn start_input(state: &AppState, run: &Run) -> Result<(String, Value)> {
    let w = run.workflow();
    let (body, _) = template(state, run.project_id, w).await?;
    let repo = match run.repo_id {
        Some(id) => Some(crate::engine::repo(state, id).await?),
        None => None,
    };
    let ticket = run.data.get("ticket").cloned().unwrap_or(Value::Null);
    let ticket_text = if ticket.is_null() {
        String::new()
    } else {
        format!(
            "**{}**\n\n{}\n\nStatus: {}",
            ticket.get("title").and_then(|v| v.as_str()).unwrap_or(""),
            ticket.get("description").and_then(|v| v.as_str()).unwrap_or(""),
            ticket.get("status").and_then(|v| v.as_str()).unwrap_or("")
        )
    };
    let vars = [
        ("title", run.title.clone()),
        ("ticket_key", run.ticket_key.clone().unwrap_or_default()),
        ("ticket", ticket_text),
        ("repo", repo.as_ref().map(|r| r.full_name.clone()).unwrap_or_default()),
        ("branch", run.branch.clone().unwrap_or_default()),
        ("base_branch", run.base_branch.clone().unwrap_or_default()),
        ("pr_number", run.pr_number.map(|n| n.to_string()).unwrap_or_default()),
        ("brief", run.brief.clone().unwrap_or_default()),
        ("scope", run.scope.clone().unwrap_or_else(|| "report first, fix on confirmation".into())),
    ];
    let text = render(&body, &vars);
    let data = json!({
        "workflow": w.as_str(),
        "ticket_key": run.ticket_key,
        "repo": repo.as_ref().map(|r| r.full_name.clone()),
        "branch": run.branch,
        "base_branch": run.base_branch,
        "pr_number": run.pr_number,
        "brief": run.brief,
        "scope": run.scope,
        "title": run.title,
    });
    Ok((text, data))
}

/// A handoff summary built from the run's event log, used to seed a fresh
/// session when a harness cannot resume, and in expiry notices.
pub async fn handoff_summary(state: &AppState, run: &Run) -> Result<String> {
    let evs = crate::events::all(&state.pool, run.id).await?;
    let mut lines = vec![
        format!("Run: {} ({})", run.title, run.workflow),
        format!("State: {}{}", run.state, run.waiting_for.as_ref().map(|w| format!(" (waiting for {w})")).unwrap_or_default()),
    ];
    if let Some(b) = &run.branch {
        lines.push(format!("Branch: {b}"));
    }
    if let Some(u) = &run.pr_url {
        lines.push(format!("PR: {u}"));
    }
    lines.push(format!("Turns used: {}", run.turns_used));
    lines.push("Recent history:".into());
    let mut hist = vec![];
    for e in &evs {
        let p = &e.payload;
        let s = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let line = match e.kind.as_str() {
            "input_received" => Some(format!("- input ({}): {}", s("kind"), truncate(&s("text"), 300))),
            "harness.agent_message" => Some(format!("- agent: {}", truncate(&s("text"), 300))),
            "question_asked" => Some(format!("- asked: {}", truncate(&s("text"), 300))),
            "question_answered" => Some(format!("- answer: {} {}", s("decision"), truncate(&s("answer"), 300))),
            "primitive_call" if p.get("ok").and_then(|v| v.as_bool()) == Some(true) => {
                let name = s("name");
                if name.starts_with("pr.") || name.starts_with("status.") || name.starts_with("ticket.transition") {
                    Some(format!("- did {name}"))
                } else {
                    None
                }
            }
            "status_report" => Some(format!("- progress: {}", truncate(&s("text"), 300))),
            _ => None,
        };
        if let Some(l) = line {
            hist.push(l);
        }
    }
    let start = hist.len().saturating_sub(30);
    lines.extend(hist[start..].iter().cloned());
    Ok(lines.join("\n"))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn front_matter_and_render() {
        let t = builtin(WorkflowKind::TicketBuild);
        let (fm, _) = split_front_matter(t);
        assert_eq!(fm["workflow"], "ticket_build");
        let out = render(t, &[("ticket_key", "PROJ-1".into()), ("branch", "factory/x".into())]);
        assert!(out.contains("PROJ-1"));
        assert!(!out.contains("workflow: ticket_build"));
        for w in WorkflowKind::ALL {
            assert!(!builtin(w).is_empty());
        }
    }
}
