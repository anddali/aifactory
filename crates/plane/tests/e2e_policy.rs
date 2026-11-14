//! Every policy rule in requirements section 8, each with an attempt that
//! must be refused.

mod common;
use common::*;
use serde_json::{Value, json};
use std::process::Command;

async fn prim(env: &Env, token: &str, name: &str, args: Value) -> Value {
    let r = env
        .http
        .post(format!("{}/api/primitives/{name}", env.url()))
        .bearer_auth(token)
        .header("idempotency-key", uuid::Uuid::new_v4().to_string())
        .json(&args)
        .send()
        .await
        .unwrap();
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(json!({"status": status.as_u16()}));
    if status.as_u16() == 401 {
        return json!({"ok": false, "error": "401"});
    }
    v
}

fn git(dir: &std::path::Path, token: &str, args: &[&str]) -> (bool, String) {
    let o = Command::new("git")
        .current_dir(dir)
        .args(["-c", "credential.helper=", "-c", &format!("http.extraHeader=Authorization: Bearer {token}"), "-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.autocrlf=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .output()
        .unwrap();
    (o.status.success(), String::from_utf8_lossy(&o.stderr).to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn each_policy_rule_refuses() {
    let env = Env::new(Opts::default()).await;
    env.ticket("PROJ-9", "Policy").await;
    let id = env.start_ticket_run("PROJ-9").await;
    env.wait_state(id, "waiting", Some("human_answer"), 90).await;
    // Park the runner so the test acts as the agent.
    env.post(&format!("/api/runs/{id}/pause"), json!({})).await;
    env.wait_run(id, "runner stopped", 30, |v| v["state"] == "paused" && v["runner"]["state"] != "alive").await;
    let token = env.run_token(id).await;
    let run = env.run(id).await;
    let branch = run["branch"].as_str().unwrap().to_string();
    let repo_id: uuid::Uuid = sqlx::query_scalar("SELECT repo_id FROM runs WHERE id=$1").bind(id).fetch_one(&env.pool).await.unwrap();

    // Authentication: a wrong token is refused.
    let r = prim(&env, "frt_wrong", "ticket.get", json!({})).await;
    assert_eq!(r["ok"], false);

    // Repository allowlist.
    let r = prim(&env, &token, "repo.read_file", json!({"repo": "acme/other", "path": "README.md"})).await;
    assert_eq!(r["ok"], false);
    assert!(r["error"].as_str().unwrap().contains("allowlist"), "{r}");
    let r = prim(&env, &token, "repo.read_file", json!({"path": "README.md"})).await;
    assert_eq!(r["ok"], true, "{r}");

    // Git endpoint: clone works with the run token, not without it.
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("{}/git/{repo_id}", env.url());
    let (ok, _) = git(tmp.path(), "frt_wrong", &["clone", "-q", &url, "nope"]);
    assert!(!ok, "clone with a bad token refused");
    let (ok, e) = git(tmp.path(), &token, &["clone", "-q", &url, "w"]);
    assert!(ok, "{e}");
    let w = tmp.path().join("w");
    std::fs::write(w.join("x.txt"), "x").unwrap();
    git(&w, &token, &["add", "-A"]);
    git(&w, &token, &["commit", "-q", "-m", "x"]);

    // Protected branch.
    let (ok, _) = git(&w, &token, &["push", "origin", "HEAD:refs/heads/main"]);
    assert!(!ok, "push to main refused");
    // Outside the run's namespace.
    let (ok, _) = git(&w, &token, &["push", "origin", "HEAD:refs/heads/feature/x"]);
    assert!(!ok, "push outside namespace refused");
    // Own branch accepted.
    let (ok, e) = git(&w, &token, &["push", "origin", &format!("HEAD:refs/heads/{branch}")]);
    assert!(ok, "own branch push: {e}");
    // Not a fast-forward.
    git(&w, &token, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let (ok, _) = git(&w, &token, &["push", "-f", "origin", &format!("HEAD:refs/heads/{branch}")]);
    assert!(!ok, "force push refused");
    // Deletion.
    let (ok, _) = git(&w, &token, &["push", "origin", &format!(":refs/heads/{branch}")]);
    assert!(!ok, "deletion refused");
    let upstream_main = env.git_upstream(&["rev-parse", "main"]);
    let (_, _) = git(&w, &token, &["fetch", "-q", "origin"]);
    assert_eq!(env.git_upstream(&["rev-parse", "main"]), upstream_main, "main untouched");
    assert!(!env.git_upstream(&["rev-parse", "--verify", "--quiet", "refs/heads/feature/x"]).contains(char::is_alphanumeric));

    // Branch creation outside the namespace is refused as a primitive too.
    let r = prim(&env, &token, "repo.create_branch", json!({"name": "hotfix"})).await;
    assert_eq!(r["ok"], false);

    // Merge: no merge while merge is a human action...
    let pr = prim(&env, &token, "pr.open", json!({"title": "t", "body": "b"})).await;
    assert_eq!(pr["ok"], true, "{pr}");
    let n = pr["result"]["number"].as_i64().unwrap();
    let r = prim(&env, &token, "pr.merge", json!({"number": n})).await;
    assert!(r["error"].as_str().unwrap().contains("merge_human_only"), "{r}");
    // ...nor without a recorded approval naming that PR when the project allows it.
    env.put(&format!("/api/projects/{}", env.project), json!({"config": {"merge_by_agent": true}})).await;
    let r = prim(&env, &token, "pr.merge", json!({"number": n})).await;
    assert!(r["error"].as_str().unwrap().contains("merge_needs_approval"), "{r}");
    // An approval request; a user without the approve role cannot approve.
    let a = prim(&env, &token, "human.request_approval", json!({"action": "merge", "summary": "ready", "pr": n})).await;
    assert_eq!(a["ends_turn"], true);
    let qid = a["result"]["question_id"].as_str().unwrap().to_string();
    let bob = env.login("bob@example.test").await;
    let (s, v) = env.req(reqwest::Method::POST, &format!("/api/questions/{qid}/answer"), Some(json!({"decision": "approved"})), &bob).await;
    assert!(s >= 400, "bob may not approve: {v}");
    // An approval for a different PR does not count either.
    let alice = env.login("alice@example.test").await;
    let (s, v) = env.req(reqwest::Method::POST, &format!("/api/questions/{qid}/answer"), Some(json!({"decision": "approved"})), &alice).await;
    assert!(s < 300, "{v}");
    let r = prim(&env, &token, "pr.merge", json!({"number": n + 100})).await;
    assert_eq!(r["ok"], false);
    let r = prim(&env, &token, "pr.merge", json!({"number": n})).await;
    assert_eq!(r["ok"], true, "merge with approval: {r}");

    // Events authored by the factory identity are ignored.
    let before = env.count_events(id, "input_received").await;
    env.pr_action(json!({"action": "thread", "number": n, "author": "factory-bot", "body": "self", "path": "README.md", "line": 1})).await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(env.count_events(id, "input_received").await, before);

    // Every refusal is audited.
    let audit = env.get(&format!("/api/audit?run_id={id}&limit=500")).await;
    let rules: Vec<String> = audit["entries"].as_array().unwrap().iter().filter(|e| e["action"] == "policy.denied").map(|e| e["details"]["rule"].as_str().unwrap_or("").to_string()).collect();
    for rule in ["repo_allowlist", "protected_branch", "branch_namespace", "no_delete", "fast_forward_only", "merge_human_only", "merge_needs_approval"] {
        assert!(rules.iter().any(|r| r == rule), "audit has {rule}: {rules:?}");
    }
    assert!(audit["entries"].as_array().unwrap().iter().any(|e| e["action"] == "approval.refused"));
}

#[tokio::test(flavor = "multi_thread")]
async fn secrets_are_encrypted_hidden_and_absent_from_runners() {
    let env = Env::new(Opts::default()).await;
    let c = env.connector("github", json!({"api_url": "http://127.0.0.1:1"})).await;
    env.put(&format!("/api/connectors/{c}/secrets"), json!({"secrets": {"token": "ghp_supersecretvalue1234567890abcdefgh"}})).await;
    let list = env.get("/api/connectors").await;
    let s = list.to_string();
    assert!(!s.contains("supersecret"), "never displayed in full");
    assert!(s.contains("…efgh"));
    let raw: Vec<u8> = sqlx::query_scalar("SELECT ciphertext FROM secrets WHERE connector_id=$1").bind(c).fetch_one(&env.pool).await.unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("supersecret"), "encrypted at rest");

    env.ticket("PROJ-8", "Env").await;
    let id = env.start_ticket_run("PROJ-8").await;
    env.wait_state(id, "waiting", Some("human_answer"), 90).await;
    // The runner's environment holds only its run token and plane URLs.
    let root = env.replicas[0].data_dir.join("runners");
    let mut found = false;
    for d in std::fs::read_dir(root).unwrap().flatten() {
        let log = std::fs::read_to_string(d.path().join("supervisor.log")).unwrap_or_default();
        assert!(!log.contains("supersecret"));
        found = true;
    }
    assert!(found);
    // The event log redacts secrets the agent might echo.
    let token = env.run_token(id).await;
    let r = prim(&env, &token, "status.report", json!({"text": "my key is ghp_abcdefghijklmnopqrstuvwxyz0123456789"})).await;
    assert_eq!(r["ok"], true);
    let evs = env.events(id).await;
    let reported = evs.iter().rev().find(|e| e["kind"] == "status_report").unwrap();
    assert!(!reported.to_string().contains("ghp_abcdef"), "redacted: {reported}");
}
