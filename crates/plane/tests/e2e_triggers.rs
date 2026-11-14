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
    env.wait_state(id, "waiting", Some("human_answer"), 90).await;
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
    // Alice presses Approve in Slack.
    env.post(&format!("/api/fake/{}/chat/reply", env.chat), json!({"user": "U-alice", "action": [qid, "approved"]})).await;
    env.wait_run(id, "approved via slack", 30, |v| v["questions"].as_array().unwrap().iter().any(|q| q["id"] == qid.as_str() && q["state"] == "answered")).await;
    let q = env.run(id).await["questions"].as_array().unwrap().iter().find(|q| q["id"] == qid.as_str()).unwrap().clone();
    assert_eq!(q["answered_via"], "slack");
    assert_eq!(q["decision"], "approved");
    // The portal answer that comes second is refused.
    let alice = env.login("alice@example.test").await;
    let (s, _) = env.req(reqwest::Method::POST, &format!("/api/questions/{qid}/answer"), Some(json!({"decision": "rejected"})), &alice).await;
    assert_eq!(s, 409);
    // The Slack message shows the outcome.
    env.wait_chat("approved by alice@example.test via slack", 30).await;
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
