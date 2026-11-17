//! The primitive catalogue. The plane executes these; the `factory` binary
//! exposes them as MCP tools and CLI subcommands from this one list.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrimitiveResponse {
    pub ok: bool,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub error: Option<String>,
    /// The agent must end its turn now (ask / approval).
    #[serde(default)]
    pub ends_turn: bool,
    /// The plane returned a stored result for a repeated call.
    #[serde(default)]
    pub replayed: bool,
}

pub struct PrimitiveDef {
    pub name: &'static str,
    pub description: &'static str,
    /// (arg name, json type, required, description)
    pub args: &'static [(&'static str, &'static str, bool, &'static str)],
    pub side_effect: bool,
}

impl PrimitiveDef {
    pub fn input_schema(&self) -> Value {
        let mut props = serde_json::Map::new();
        let mut required = vec![];
        for (name, ty, req, desc) in self.args {
            let schema = match *ty {
                "string[]" => json!({"type":"array","items":{"type":"string"},"description":desc}),
                "object" => json!({"type":"object","description":desc}),
                t => json!({"type": t, "description": desc}),
            };
            props.insert(name.to_string(), schema);
            if *req {
                required.push(json!(name));
            }
        }
        json!({"type":"object","properties":props,"required":required})
    }
}

macro_rules! p {
    ($name:literal, $se:expr, $desc:literal, [$( ($a:literal, $t:literal, $r:expr, $d:literal) ),* $(,)?]) => {
        PrimitiveDef { name: $name, description: $desc, side_effect: $se, args: &[$(($a, $t, $r, $d)),*] }
    };
}

pub const CATALOGUE: &[PrimitiveDef] = &[
    // Repos
    p!("repo.search", false, "Search code in an allowed repository.", [
        ("repo","string",false,"Repository (defaults to the run's repository)"),
        ("query","string",true,"Text to search for")]),
    p!("repo.read_file", false, "Read a file from a repository at a ref.", [
        ("repo","string",false,"Repository"), ("path","string",true,"File path"),
        ("ref","string",false,"Branch, tag or commit")]),
    p!("repo.list_files", false, "List files in a repository at a ref.", [
        ("repo","string",false,"Repository"), ("ref","string",false,"Branch, tag or commit")]),
    p!("repo.clone_info", false, "Get the plane git URL and the run's branch for clone, fetch and push.", [
        ("repo","string",false,"Repository")]),
    p!("repo.create_branch", true, "Create a branch inside the run's branch namespace.", [
        ("repo","string",false,"Repository"), ("name","string",true,"Branch name (must start with the run namespace)"),
        ("from","string",false,"Start point")]),
    // Tickets
    p!("ticket.search", false, "Search tickets.", [("query","string",true,"Search text")]),
    p!("ticket.get", false, "Fetch a ticket with its children and comments.", [("key","string",false,"Ticket key (defaults to the run's ticket)")]),
    p!("ticket.transition", true, "Move a ticket to a new status.", [("key","string",false,"Ticket key"), ("to","string",true,"Target status")]),
    p!("ticket.comment", true, "Comment on a ticket.", [("key","string",false,"Ticket key"), ("body","string",true,"Comment text")]),
    // Pull requests
    p!("pr.open", true, "Open a pull request from the run's branch.", [
        ("repo","string",false,"Repository"), ("title","string",true,"Title"), ("body","string",true,"Description"),
        ("head","string",false,"Head branch (defaults to the run's branch)"), ("base","string",false,"Base branch")]),
    p!("pr.get", false, "Read a pull request with its threads.", [("repo","string",false,"Repository"), ("number","integer",false,"PR number (defaults to the run's PR)")]),
    p!("pr.list", false, "List pull requests.", [("repo","string",false,"Repository"), ("state","string",false,"open|closed|all")]),
    p!("pr.comment", true, "Comment on a pull request. With path and line, opens a review thread.", [
        ("repo","string",false,"Repository"), ("number","integer",false,"PR number"), ("body","string",true,"Comment"),
        ("path","string",false,"File path for an inline thread"), ("line","integer",false,"Line for an inline thread")]),
    p!("pr.reply", true, "Reply to a review thread.", [("repo","string",false,"Repository"), ("number","integer",false,"PR number"),
        ("thread_id","string",true,"Thread id"), ("body","string",true,"Reply")]),
    p!("pr.resolve_thread", true, "Mark a review thread resolved or won't-fix.", [("repo","string",false,"Repository"), ("number","integer",false,"PR number"),
        ("thread_id","string",true,"Thread id"), ("status","string",false,"resolved|wont_fix")]),
    p!("pr.review_status", false, "Read the PR's review status.", [("repo","string",false,"Repository"), ("number","integer",false,"PR number")]),
    p!("pr.merge_status", false, "Read whether the PR can merge.", [("repo","string",false,"Repository"), ("number","integer",false,"PR number")]),
    p!("pr.merge", true, "Merge the PR. Refused without a recorded human approval naming this PR.", [("repo","string",false,"Repository"), ("number","integer",false,"PR number")]),
    // CI
    p!("ci.status", false, "Read check and build status for a PR or branch.", [("repo","string",false,"Repository"), ("number","integer",false,"PR number"), ("branch","string",false,"Branch")]),
    p!("ci.logs", false, "Read failing logs for a check.", [("repo","string",false,"Repository"), ("check_id","string",true,"Check id")]),
    // Humans
    p!("human.message", true, "Send a message to the assignee in the run's thread.", [("text","string",true,"Message")]),
    p!("human.ask", true, "Ask the assignee a question. Ends the turn; the answer starts the next turn.", [("question","string",true,"Question")]),
    p!("human.request_approval", true, "Request an approval. Ends the turn; the decision starts the next turn.", [
        ("action","string",true,"What will happen on approval, e.g. merge"), ("summary","string",true,"Why"),
        ("pr","integer",false,"PR number the approval names")]),
    // Plan (two-phase workflows)
    p!("plan.submit", true, "Submit your plan for review (plan phase only). Ends the turn; the reviewer's decision starts the next turn: approval starts the build phase, requested changes come back to revise.", [
        ("plan","string",true,"The plan, in Markdown"), ("summary","string",false,"One line summarising the plan")]),
    // Status
    p!("status.report", true, "Report progress.", [("text","string",true,"Progress note")]),
    p!("status.artifact", true, "Attach an artifact (PR link, report).", [("kind","string",true,"pr|report|link"), ("title","string",true,"Title"),
        ("url","string",false,"Link"), ("content","string",false,"Inline content")]),
    p!("status.finish", true, "Declare the work finished.", [("summary","string",true,"Summary")]),
    p!("status.blocked", true, "Declare the work blocked. Ends the turn.", [("reason","string",true,"Why")]),
];

pub fn find(name: &str) -> Option<&'static PrimitiveDef> {
    CATALOGUE.iter().find(|p| p.name == name)
}
