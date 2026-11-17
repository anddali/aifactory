//! Triggers (webhook fast path, polling reconciliation, Slack), approvals in
//! Slack and the portal, interrupt-now steering and the kill switch.

mod common;
use common::*;
use serde_json::json;

async fn wait_for_run(env: &Env, ticket: &str, secs: u64) -> uuid::Uuid {
    let start = std::time::Instant::now();
    loop {
        let runs = env.get("/api/runs?state=all").await;
        if let Some(r) = runs["runs"].as_array().unwrap().iter().find(|r| r["ticket_key"] == ticket) {
            return r["id"].as_str().unwrap().parse().unwrap();
        }
        assert!(start.elapsed().as_secs() < secs, "no run for {ticket}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ticket_label_webhook_polling_and_slack_triggers() {
    let env = Env::new(Opts::default()).await;
    // Webhook fast path: the trigger label is added.
    env.ticket("PROJ-30", "Labelled").await;
    env.post(&format!("/api/fake/{}/ticket-event", env.tickets), json!({"id": "", "key": "PROJ-30", "kind": "labeled", "label": "factory", "author": "pm"})).await;
    let id = wait_for_run(&env, "PROJ-30", 30).await;
    let audit = env.get(&format!("/api/audit?run_id={id}")).await;
    assert!(audit["entries"].as_array().unwrap().iter().any(|e| e["action"] == "run.trigger" && e["actor"].as_str().unwrap().starts_with("ticket-trigger")));
    // The label added again does not start a second run.
    env.post(&format!("/api/fake/{}/ticket-event", env.tickets), json!({"id": "", "key": "PROJ-30", "kind": "labeled", "label": "factory", "author": "pm"})).await;

    // A webhook that never arrived: only the fake's event record exists, and
    // the reconciliation poller finds it.
    env.ticket("PROJ-31", "Missed webhook").await;
    sqlx::query("INSERT INTO fake_store(ns,id,data) VALUES ($1,'missed-1',$2)")
        .bind(format!("fake_events:{}", env.tickets))
        .bind(json!({"id": "missed-1", "key": "PROJ-31", "kind": "labeled", "label": "factory", "author": "pm"}))
        .execute(&env.pool)
        .await
        .unwrap();
    wait_for_run(&env, "PROJ-31", 30).await;

    // Manual trigger from Slack, by someone with the operate role.
    env.ticket("PROJ-32", "From Slack").await;
    env.post(&format!("/api/fake/{}/chat/reply", env.chat), json!({"channel": "#factory", "user": "U-olly", "text": "start ticket_build PROJ-32 in PROJ"})).await;
    let sid = wait_for_run(&env, "PROJ-32", 30).await;
    let audit = env.get(&format!("/api/audit?run_id={sid}")).await;
    assert!(audit["entries"].as_array().unwrap().iter().any(|e| e["action"] == "run.trigger" && e["actor"] == "slack:olly@example.test"));
    // Someone with only the view role cannot.
    env.ticket("PROJ-33", "Not allowed").await;
    env.post(&format!("/api/fake/{}/chat/reply", env.chat), json!({"channel": "#factory", "user": "U-bob", "text": "start ticket_build PROJ-33"})).await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let runs = env.get("/api/runs?state=all").await;
    assert!(!runs["runs"].as_array().unwrap().iter().any(|r| r["ticket_key"] == "PROJ-33"));
    let n = runs["runs"].as_array().unwrap().iter().filter(|r| r["ticket_key"] == "PROJ-30").count();
    assert_eq!(n, 1, "one run per ticket");

    // A ticket cancelled in the ticket source ends its run.
    env.post(&format!("/api/fake/{}/ticket-event", env.tickets), json!({"id": "", "key": "PROJ-30", "kind": "transitioned", "status": "Cancelled", "author": "pm"})).await;
    env.wait_state(id, "cancelled", None, 60).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_by_slack_button_first_answer_wins_and_portal_updates() {
    let env = Env::new(Opts::default()).await;
    env.ticket("PROJ-34", "Approve").await;
    let id = env.start_ticket_run("PROJ-34").await;
    env.wait_state(id, "waiting", Some("approval"), 90).await;
    env.post(&format!("/api/runs/{id}/pause"), json!({})).await;
    env.wait_run(id, "paused", 30, |v| v["state"] == "paused" && v["runner"]["state"] != "alive").await;
    let token = env.run_token(id).await;
    let r: serde_json::Value = env
        .http
        .post(format!("{}/api/primitives/human.request_approval", env.url()))
        .bearer_auth(&token)
        .json(&json!({"action": "deploy", "summary": "ship it"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let qid = r["result"]["question_id"].as_str().unwrap().to_string();
    let msgs = env.wait_chat("Approval needed: deploy", 30).await;
    let m = msgs.iter().find(|m| m["text"].as_str().unwrap_or("").starts_with("Approval needed")).unwrap();
    assert_eq!(m["approval_for"], qid.as_str(), "approval buttons carry the question");
    // Bob (view only) presses Approve: it does not count.
    env.post(&format!("/api/fake/{}/chat/reply", env.chat), json!({"user": "U-bob", "action": [qid, "approved"]})).await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let q = env.run(id).await["questions"].as_array().unwrap().iter().find(|q| q["id"] == qid.as_str()).unwrap().clone();
    assert_eq!(q["state"], "open");
    // Bob is told why it did not count.
    env.wait_chat("<@U-bob> Your decision wasn't recorded: you do not hold the approve role", 30).await;
    // Someone with no factory user at all is told how to get linked.
    env.post(&format!("/api/fake/{}/chat/reply", env.chat), json!({"user": "U-zed", "action": [qid, "approved"]})).await;
    env.wait_chat("<@U-zed> I couldn't match your Slack account to a factory user", 30).await;
    // Carol has a factory user (with the approve role) but no Slack identity: her first press
    // links her by email and counts.
    let carol = env.post("/api/users", json!({"email": "carol@example.test"})).await;
    let carol_id = carol["id"].as_str().unwrap().to_string();
    env.put(&format!("/api/users/{carol_id}/grants"), json!({"grants": [{"project_id": env.project, "role": "approve"}]})).await;
    env.post(&format!("/api/fake/{}/chat/reply", env.chat), json!({"user": "U-carol", "action": [qid, "approved"]})).await;
    env.wait_run(id, "approved via slack", 30, |v| v["questions"].as_array().unwrap().iter().any(|q| q["id"] == qid.as_str() && q["state"] == "answered")).await;
    let linked: Option<String> = sqlx::query_scalar("SELECT u.email FROM identities i JOIN users u ON u.id=i.user_id WHERE i.provider='slack' AND i.external_id='U-carol'").fetch_optional(&env.pool).await.unwrap();
    assert_eq!(linked.as_deref(), Some("carol@example.test"), "linked by email");
    let q = env.run(id).await["questions"].as_array().unwrap().iter().find(|q| q["id"] == qid.as_str()).unwrap().clone();
    assert_eq!(q["answered_by"], "carol@example.test");
    // Alice presses Approve in Slack too late: already decided, and she is told.
    env.post(&format!("/api/fake/{}/chat/reply", env.chat), json!({"user": "U-alice", "action": [qid, "approved"]})).await;
    env.wait_chat("<@U-alice> Your decision wasn't recorded: this question was already answered", 30).await;
    env.wait_run(id, "approved via slack", 30, |v| v["questions"].as_array().unwrap().iter().any(|q| q["id"] == qid.as_str() && q["state"] == "answered")).await;
    assert_eq!(q["answered_via"], "slack");
    assert_eq!(q["decision"], "approved");
    // The portal answer that comes second is refused.
    let alice = env.login("alice@example.test").await;
    let (s, _) = env.req(reqwest::Method::POST, &format!("/api/questions/{qid}/answer"), Some(json!({"decision": "rejected"})), &alice).await;
    assert_eq!(s, 409);
    // The Slack message shows the outcome.
    env.wait_chat("approved by carol@example.test via slack", 30).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupt_now_stops_the_turn_and_kill_switch_pauses_everything() {
    let env = Env::new(Opts { fault: Some("hang_start"), ..Default::default() }).await;
    env.ticket("PROJ-35", "Interrupt").await;
    let id = env.start_ticket_run("PROJ-35").await;
    env.wait_run(id, "turn running", 60, |v| v["state"] == "running" && v["runner"]["busy"] == true).await;
    env.post(&format!("/api/runs/{id}/steer"), json!({"text": "Stop and do X instead", "interrupt": true})).await;
    env.wait_run(id, "steer turn done", 90, |v| v["turns_used"].as_i64().unwrap_or(0) >= 2 && v["state"] == "waiting").await;
    let evs = env.events(id).await;
    assert!(evs.iter().any(|e| e["kind"] == "turn_ended" && e["payload"]["outcome"] == "interrupted"));
    assert!(evs.iter().any(|e| e["kind"] == "turn_started" && e["payload"]["kind"] == "steer"));

    // Kill switch: active runs pause, new runs are refused.
    env.put("/api/settings", json!({"kill_switch": true})).await;
    env.wait_state(id, "paused", None, 30).await;
    assert_eq!(env.run(id).await["pause_reason"], "kill_switch");
    env.ticket("PROJ-36", "Refused").await;
    let (s, v) = env.post_status("/api/runs", json!({"project_id": env.project, "workflow": "ticket_build", "ticket_key": "PROJ-36"})).await;
    assert_eq!(s, 422, "{v}");
    let (s, _) = env.post_status(&format!("/api/runs/{id}/resume"), json!({})).await;
    assert!(s >= 400, "cannot resume while the kill switch is on");
    env.put("/api/settings", json!({"kill_switch": false})).await;
    env.post(&format!("/api/runs/{id}/resume"), json!({})).await;
    env.wait_state(id, "waiting", None, 30).await;
}

fn hmac_hex(key: &str, msg: &[u8]) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut m = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(key.as_bytes()).unwrap();
    m.update(msg);
    hex::encode(m.finalize().into_bytes())
}

#[tokio::test(flavor = "multi_thread")]
async fn webhooks_verify_signatures() {
    let env = Env::new(Opts::default()).await;
    // GitHub
    let gh = env.connector("github", json!({"api_url": "http://127.0.0.1:1"})).await;
    env.put(&format!("/api/connectors/{gh}/secrets"), json!({"secrets": {"webhook_secret": "whsec-test", "token": "ghp_x"}})).await;
    let body = json!({"action": "opened", "number": 7, "pull_request": {"number": 7, "title": "t", "state": "open", "html_url": "https://github.com/acme/w/pull/7", "user": {"login": "dev"}, "head": {"ref": "feature", "sha": "abc"}, "base": {"ref": "main"}}, "repository": {"full_name": "acme/w"}, "sender": {"login": "dev"}}).to_string();
    let url = format!("{}/webhooks/github/{gh}", env.url());
    let ok = env.http.post(&url).header("x-github-event", "pull_request").header("x-github-delivery", "d-1")
        .header("x-hub-signature-256", format!("sha256={}", hmac_hex("whsec-test", body.as_bytes()))).body(body.clone()).send().await.unwrap();
    assert_eq!(ok.status().as_u16(), 200);
    let v: serde_json::Value = ok.json().await.unwrap();
    assert!(v["accepted"].as_u64().unwrap() >= 1, "{v}");
    let bad = env.http.post(&url).header("x-github-event", "pull_request").header("x-hub-signature-256", "sha256=00").body(body.clone()).send().await.unwrap();
    assert_eq!(bad.status().as_u16(), 401);
    let none = env.http.post(&url).header("x-github-event", "pull_request").body(body).send().await.unwrap();
    assert_eq!(none.status().as_u16(), 401);
    // Slack: signed URL verification is answered, an unsigned one is refused.
    let sl = env.connector("slack", json!({"api_url": "http://127.0.0.1:1"})).await;
    env.put(&format!("/api/connectors/{sl}/secrets"), json!({"secrets": {"signing_secret": "slack-signing", "bot_token": "xoxb-test-token-1234"}})).await;
    let body = json!({"type": "url_verification", "challenge": "abc123"}).to_string();
    let ts = chrono::Utc::now().timestamp().to_string();
    let sig = format!("v0={}", hmac_hex("slack-signing", format!("v0:{ts}:{body}").as_bytes()));
    let url = format!("{}/webhooks/slack/{sl}", env.url());
    let r = env.http.post(&url).header("x-slack-request-timestamp", &ts).header("x-slack-signature", sig).header("content-type", "application/json").body(body.clone()).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.json::<serde_json::Value>().await.unwrap()["challenge"], "abc123");
    let r = env.http.post(&url).header("x-slack-request-timestamp", &ts).header("x-slack-signature", "v0=bad").body(body).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 401);
    let m = env.http.get(format!("{}/metrics", env.url())).send().await.unwrap().text().await.unwrap();
    assert!(m.contains("webhook_signature"));
}

/// Ticket keys are normalised to the ticket system's own key, so a differently spelled key
/// cannot start a second run for the same ticket.
#[tokio::test(flavor = "multi_thread")]
async fn ticket_keys_are_canonical_and_spellings_do_not_duplicate_runs() {
    let env = Env::new(Opts::default()).await;
    env.ticket("PROJ-33", "Key casing").await;
    let id = env.start_ticket_run(" proj-33 ").await;
    let r = env.run(id).await;
    assert_eq!(r["ticket_key"], "PROJ-33");
    assert!(r["title"].as_str().unwrap().starts_with("PROJ-33: "), "{}", r["title"]);
    let (status, body) = env.post_status("/api/runs", json!({"project_id": env.project, "workflow": "ticket_build", "ticket_key": "PROJ-33"})).await;
    assert_eq!(status, 409, "{body}");
    let (status, _) = env.post_status("/api/runs", json!({"project_id": env.project, "workflow": "ticket_build", "ticket_key": "Proj-33"})).await;
    assert_eq!(status, 409);
    // The start input is written by the run's start job, shortly after creation.
    let mut input = None;
    for _ in 0..60 {
        input = sqlx::query_scalar::<_, String>("SELECT text FROM inputs WHERE run_id=$1 AND kind='start'").bind(id).fetch_optional(&env.pool).await.unwrap();
        if input.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let input = input.expect("start input");
    assert!(input.contains("**PROJ-33**") && !input.contains("proj-33"), "prompt uses the canonical key");
}
