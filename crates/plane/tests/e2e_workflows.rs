//! Phase 3 acceptance: every workflow type in rehearsal, repository
//! monitoring, LiteLLM per-run keys with spend reconciliation, Entra sign-in
//! with group roles, stuck and loop detection, outcome metrics and retention.

mod common;
use common::*;
use serde_json::{Value, json};

fn find<'a>(v: &'a Value, k: &str, val: &str) -> Option<&'a Value> {
    v.as_array()?.iter().find(|x| x[k] == val)
}

#[tokio::test(flavor = "multi_thread")]
async fn pr_review_via_repo_monitoring_rereview_and_round_limit() {
    let env = Env::new(Opts { project_config: json!({"review_round_limit": 3}), ..Default::default() }).await;
    let repo = env.get(&format!("/api/projects/{}/repos", env.project)).await["repos"][0]["id"].as_str().unwrap().to_string();
    env.put(&format!("/api/repos/{repo}"), json!({"monitor_prs": true})).await;

    // A human opens a PR; monitoring starts a review run.
    let pr = env.pr_action(json!({"action": "open", "head": "feature/a", "base": "main", "title": "Add feature", "author": "dev", "files": {"src/feature.rs": "pub fn f() {}\n"}})).await;
    let n = pr["number"].as_i64().unwrap();
    let start = std::time::Instant::now();
    let id: uuid::Uuid = loop {
        let runs = env.get("/api/runs?state=all").await;
        if let Some(r) = runs["runs"].as_array().unwrap().iter().find(|r| r["workflow"] == "pr_review" && r["pr_number"] == n) {
            break r["id"].as_str().unwrap().parse().unwrap();
        }
        assert!(start.elapsed().as_secs() < 30, "no review run started");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    };
    env.wait_state(id, "waiting", Some("review"), 90).await;
    let d = env.pr(n).await;
    assert_eq!(d["threads"].as_array().unwrap().len(), 1, "one inline thread");
    assert!(d["comments"].as_array().unwrap().iter().any(|c| c["body"].as_str().unwrap().starts_with("Review:")));
    assert!(d["reviews"].as_array().unwrap().is_empty(), "the factory never casts a vote");
    // The review run never pushes.
    let token = env.run_token(id).await;
    let r = env.http.get(format!("{}/git/{repo}/info/refs?service=git-receive-pack", env.url())).bearer_auth(&token).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 403);

    // The author replies to the thread: re-review resolves it.
    let tid = d["threads"][0]["id"].as_str().unwrap().to_string();
    env.pr_action(json!({"action": "reply", "number": n, "author": "dev", "thread_id": tid, "body": "Added the header."})).await;
    let start = std::time::Instant::now();
    loop {
        let d = env.pr(n).await;
        if d["threads"][0]["status"] == "resolved" {
            break;
        }
        assert!(start.elapsed().as_secs() < 90, "thread never resolved");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    env.wait_state(id, "waiting", Some("review"), 60).await;
    // A push is another round; at the limit (3) the run posts a closing comment and ends.
    env.pr_action(json!({"action": "push", "number": n, "head": "feature/a", "author": "dev", "files": {"src/feature.rs": "// header\npub fn f() {}\n"}})).await;
    env.wait_run(id, "second re-review", 60, |v| v["review_rounds"].as_i64() == Some(2) && v["state"] == "waiting").await;
    env.pr_action(json!({"action": "push", "number": n, "head": "feature/a", "author": "dev", "files": {"src/more.rs": "pub fn g() {}\n"}})).await;
    let r = env.wait_state(id, "finished", None, 60).await;
    assert_eq!(r["outcome"], "review_round_limit");
    let d = env.pr(n).await;
    assert!(d["comments"].as_array().unwrap().iter().any(|c| c["body"].as_str().unwrap().contains("Review round limit (3) reached")));

    // A second PR approved by its human reviewer ends its review run.
    let pr2 = env.pr_action(json!({"action": "open", "head": "feature/b", "base": "main", "title": "B", "author": "dev", "files": {"b.txt": "b"}})).await;
    let n2 = pr2["number"].as_i64().unwrap();
    let start = std::time::Instant::now();
    let id2: uuid::Uuid = loop {
        let runs = env.get("/api/runs?state=all").await;
        if let Some(r) = runs["runs"].as_array().unwrap().iter().find(|r| r["pr_number"] == n2) {
            break r["id"].as_str().unwrap().parse().unwrap();
        }
        assert!(start.elapsed().as_secs() < 30);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    };
    env.wait_state(id2, "waiting", Some("review"), 90).await;
    env.pr_action(json!({"action": "review", "number": n2, "author": "lead", "state": "approved"})).await;
    let r = env.wait_state(id2, "finished", None, 60).await;
    assert_eq!(r["outcome"], "approved");

    // The factory does not review its own PRs by default.
    env.pr_action(json!({"action": "open", "head": "factory/own", "base": "main", "title": "own", "author": "factory-bot", "files": {"o.txt": "o"}})).await;
    let (s, _) = env.post_status("/api/runs", json!({"project_id": env.project, "workflow": "pr_review", "repo": env.repo_name, "pr_number": 3})).await;
    assert_eq!(s, 422);
}

#[tokio::test(flavor = "multi_thread")]
async fn ad_hoc_and_security_audit_report_first_fix_on_confirmation() {
    let env = Env::new(Opts::default()).await;
    // Ad hoc: report, then the human says no; nothing is changed.
    let r = env.post("/api/runs", json!({"project_id": env.project, "workflow": "ad_hoc", "title": "Survey the repo", "brief": "Tell me what is in it", "scope": "report first, fix on confirmation"})).await;
    let a: uuid::Uuid = r["id"].as_str().unwrap().parse().unwrap();
    let r = env.wait_state(a, "waiting", Some("human_answer"), 90).await;
    assert!(find(&r["artifacts"], "kind", "report").is_some());
    env.reply(a, "U-alice", "no thanks").await;
    let r = env.wait_state(a, "finished", None, 90).await;
    assert_eq!(r["outcome"], "completed");
    assert!(r["pr_number"].is_null(), "no change without confirmation");

    // Security audit: ranked, verified findings; fix only what is confirmed.
    let r = env.post("/api/runs", json!({"project_id": env.project, "workflow": "security_audit", "title": "Audit", "brief": "Look for secrets"})).await;
    let s: uuid::Uuid = r["id"].as_str().unwrap().parse().unwrap();
    let r = env.wait_state(s, "waiting", Some("human_answer"), 90).await;
    let report = find(&r["artifacts"], "kind", "report").unwrap();
    let content = report["content"].as_str().unwrap();
    assert!(content.contains("1. [high]") && content.contains("2. [low]"), "ranked findings: {content}");
    env.reply(s, "U-alice", "2").await;
    let r = env.wait_state(s, "finished", None, 120).await;
    let n = r["pr_number"].as_i64().expect("a PR for the confirmed fix");
    let d = env.pr(n).await;
    assert_eq!(d["pr"]["title"], "Fix confirmed security findings");
    let branch = r["branch"].as_str().unwrap();
    assert!(env.git_upstream(&["show", &format!("refs/heads/{branch}:SECURITY.md")]).contains("Security policy"));
}

#[tokio::test(flavor = "multi_thread")]
async fn litellm_per_run_key_budget_and_spend_reconciliation() {
    let env = Env::new(Opts::default()).await;
    let ll = env.connector("litellm", json!({"base_url": format!("{}/fake-litellm", env.url())})).await;
    env.put(&format!("/api/connectors/{ll}/secrets"), json!({"secrets": {"master_key": "sk-fake-master"}})).await;
    assert_eq!(env.post(&format!("/api/connectors/{ll}/test"), json!({})).await["ok"], true);
    env.put("/api/settings", json!({"model_connector_id": ll})).await;
    env.ticket("PROJ-50", "LiteLLM").await;
    let r = env.post("/api/runs", json!({"project_id": env.project, "workflow": "ticket_build", "ticket_key": "PROJ-50", "budget_usd": 0.025})).await;
    let id: uuid::Uuid = r["id"].as_str().unwrap().parse().unwrap();
    env.wait_state(id, "waiting", Some("human_answer"), 90).await;
    // The virtual key: the run's budget, the run's model only.
    let keys: Vec<(String, Value)> = sqlx::query_as("SELECT id, data FROM fake_store WHERE ns='fake_litellm'").fetch_all(&env.pool).await.unwrap();
    assert_eq!(keys.len(), 1);
    let (key, data) = &keys[0];
    assert_eq!(data["max_budget"], 0.025);
    assert_eq!(data["models"], json!(["gpt-4.1"]));
    assert_eq!(data["metadata"]["factory_run_id"], id.to_string());
    // Agent traffic straight to LiteLLM with the virtual key (what the harness does).
    let call = |model: &str| env.http.post(format!("{}/fake-litellm/v1/chat/completions", env.url())).bearer_auth(key).json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})).send();
    assert_eq!(call("gpt-4.1").await.unwrap().status().as_u16(), 200);
    assert_eq!(call("other-model").await.unwrap().status().as_u16(), 401, "model allowlist");
    assert_eq!(call("gpt-4.1").await.unwrap().status().as_u16(), 200);
    assert_eq!(call("gpt-4.1").await.unwrap().status().as_u16(), 200);
    assert_eq!(call("gpt-4.1").await.unwrap().status().as_u16(), 400, "budget enforced by LiteLLM");
    // End the run: spend is read, reconciled and recorded; the key is deleted.
    env.post(&format!("/api/runs/{id}/cancel"), json!({})).await;
    env.wait_state(id, "cancelled", None, 30).await;
    let r = env.wait_run(id, "spend reconciled", 60, |v| v["spent_usd"].as_f64().unwrap_or(0.0) > 0.0).await;
    assert!((r["spent_usd"].as_f64().unwrap() - 0.03).abs() < 1e-9, "{}", r["spent_usd"]);
    let evs = env.events(id).await;
    assert!(evs.iter().any(|e| e["kind"] == "litellm_reconciled" && (e["payload"]["litellm_spend"].as_f64().unwrap() - 0.03).abs() < 1e-9));
    let deleted: Value = sqlx::query_scalar("SELECT data FROM fake_store WHERE ns='fake_litellm' AND id=$1").bind(key).fetch_one(&env.pool).await.unwrap();
    assert_eq!(deleted["deleted"], true);
    assert_eq!(call("gpt-4.1").await.unwrap().status().as_u16(), 401, "key no longer works");
}

#[tokio::test(flavor = "multi_thread")]
async fn entra_sign_in_maps_groups_to_roles() {
    let env = Env::new(Opts::default()).await;
    assert_eq!(env.get("/auth/config").await["entra"], true);
    env.put("/api/settings", json!({"entra_group_roles": [{"group": "g-approvers", "project_id": env.project, "role": "approve"}, {"group": "g-admins", "role": "administer"}]})).await;
    env.post("/api/fake/entra/user", json!({"email": "erin@example.test", "oid": "oid-erin", "name": "Erin", "groups": ["g-approvers", "g-other"]})).await;
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let r = http.get(format!("{}/auth/entra/login", env.url())).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 303);
    let cookie = r.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    let r = http.get(&loc).send().await.unwrap();
    let cb = r.headers().get("location").unwrap().to_str().unwrap().to_string();
    let r = http.get(&cb).header("cookie", &cookie).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 302, "{:?}", r.text().await);
    let session = r.headers().get("x-factory-session").unwrap().to_str().unwrap().to_string();
    let (s, me) = env.req(reqwest::Method::GET, "/auth/me", None, &session).await;
    assert_eq!(s, 200);
    assert_eq!(me["email"], "erin@example.test");
    assert!(me["grants"].as_array().unwrap().iter().any(|g| g["role"] == "approve" && g["project_id"] == env.project.to_string()));
    assert_eq!(me["is_global_admin"], false);
    // A forged callback without the state cookie is refused.
    let r = http.get(&cb).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 401);
    // Development sign-in exists only because the environment enables it.
    assert_eq!(env.get("/auth/config").await["dev_login"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn loop_and_no_progress_detection_pause_and_alert() {
    let env = Env::new(Opts { fault: Some("loop_calls"), project_config: json!({"ops_channel": "#factory-ops"}), ..Default::default() }).await;
    env.put("/api/settings", json!({"stuck_repeat_threshold": 5, "stuck_noprogress_turns": 3})).await;
    env.ticket("PROJ-60", "Loop").await;
    let id = env.start_ticket_run("PROJ-60").await;
    env.wait_state(id, "waiting", Some("human_answer"), 90).await;
    env.reply(id, "U-alice", "go").await;
    let r = env.wait_state(id, "paused", None, 120).await;
    assert_eq!(r["pause_reason"], "looping");
    env.wait_chat("[ops]", 30).await;
    let msgs = env.wait_chat("detected as looping", 30).await;
    assert!(msgs.iter().any(|m| m["channel"] == "#factory-ops" && m["text"].as_str().unwrap().contains("looping")));
    assert!(msgs.iter().any(|m| m["channel"] == "#factory" && m["text"].as_str().unwrap().contains("detected as looping")));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_progress_detection_metrics_and_retention() {
    let env = Env::new(Opts::default()).await;
    env.put("/api/settings", json!({"stuck_noprogress_turns": 3})).await;
    env.ticket("PROJ-61", "Spin").await;
    let id = env.start_ticket_run("PROJ-61").await;
    env.wait_state(id, "waiting", Some("human_answer"), 90).await;
    env.reply(id, "U-alice", "go").await;
    let r = env.wait_state(id, "waiting", Some("review"), 90).await;
    let n = r["pr_number"].as_i64().unwrap();
    // Three steering turns that change nothing.
    for i in 0..3 {
        env.post(&format!("/api/runs/{id}/steer"), json!({"text": format!("note {i}")})).await;
        let want = 2 + i + 1;
        env.wait_run(id, "steer turn", 60, move |v| v["turns_used"].as_i64().unwrap_or(0) >= want && v["runner"]["busy"] == false).await;
    }
    let r = env.wait_state(id, "paused", None, 30).await;
    assert_eq!(r["pause_reason"], "no_progress");
    env.post(&format!("/api/runs/{id}/resume"), json!({})).await;
    env.pr_action(json!({"action": "review", "number": n, "author": "reviewer", "state": "approved"})).await;
    env.wait_state(id, "finished", None, 60).await;

    // Outcome metrics per project, model and harness.
    let m = env.get("/api/metrics/outcomes").await;
    let row = &m["rows"][0];
    assert_eq!(row["project_key"], "PROJ");
    assert_eq!(row["harness"], "scripted");
    assert_eq!(row["approved_prs"], 1);
    assert!(row["approval_rate"].as_f64().unwrap() > 0.99);
    assert!(!row["avg_hours_to_approval"].is_null());
    // Prometheus metrics.
    let t = env.http.get(format!("{}/metrics", env.url())).send().await.unwrap().text().await.unwrap();
    assert!(t.contains("factory_events_total") && t.contains("factory_primitive_calls_total"));

    // Retention: events older than the retention period are dropped.
    env.put("/api/settings", json!({"event_retention_days": 30})).await;
    sqlx::query("INSERT INTO events(run_id, seq, ts, kind, payload) VALUES ($1, 999999, now() - interval '45 days', 'old', '{}')").bind(id).execute(&env.pool).await.unwrap();
    env.post("/api/_housekeeping", json!({})).await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE run_id=$1 AND kind='old'").bind(id).fetch_one(&env.pool).await.unwrap();
    assert_eq!(left, 0);
    let parts: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_inherits i JOIN pg_class p ON p.oid=i.inhparent WHERE p.relname='events'").fetch_one(&env.pool).await.unwrap();
    assert!(parts >= 3, "monthly partitions exist");
    let recent: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE run_id=$1").bind(id).fetch_one(&env.pool).await.unwrap();
    assert!(recent > 10, "recent events kept");
}
