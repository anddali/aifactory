//! Run lifetime: idle grace, reminders, warning, expiry, revival; caps and
//! steering.

mod common;
use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn idle_run_checkpoints_reminds_warns_expires_and_revives() {
    let env = Env::new(Opts {
        project_config: json!({"idle_grace_secs": 2, "idle_reminder_secs": 6, "idle_warning_secs": 10, "idle_expiry_secs": 14}),
        ..Default::default()
    })
    .await;
    env.ticket("PROJ-4", "Idle").await;
    let id = env.start_ticket_run("PROJ-4").await;
    env.wait_state(id, "waiting", Some("human_answer"), 90).await;

    // After the grace period the runner checkpoints and exits; the run keeps waiting.
    let r = env.wait_run(id, "runner stopped after grace", 30, |v| v["runner"]["state"] == "stopped").await;
    assert_eq!(r["state"], "waiting");
    assert!(r["runner"]["has_session"] == true);
    let evs = env.events(id).await;
    assert!(evs.iter().any(|e| e["kind"] == "runner_exited" && e["payload"]["reason"].as_str().unwrap().contains("idle")));

    let r = env.wait_state(id, "expired", None, 60).await;
    let evs = env.events(id).await;
    let kinds: Vec<&str> = evs.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let rem = kinds.iter().position(|k| *k == "reminder_sent").expect("reminder");
    let warn = kinds.iter().position(|k| *k == "warning_sent").expect("warning");
    let exp = kinds.iter().position(|k| *k == "expired").expect("expired");
    assert!(rem < warn && warn < exp);
    let msgs = env.wait_chat("This run expired", 30).await;
    assert!(msgs.iter().any(|m| m["text"].as_str().unwrap_or("").contains("Reminder: this run is waiting for your answer")));
    assert!(msgs.iter().any(|m| m["text"].as_str().unwrap_or("").contains("will expire at")));
    assert!(msgs.iter().any(|m| m["text"].as_str().unwrap_or("").contains("This run expired")));
    // The final summary is also on the ticket.
    let t: serde_json::Value = sqlx::query_scalar("SELECT data FROM fake_store WHERE ns=$1 AND id='PROJ-4'").bind(format!("fake_ticket:{}", env.tickets)).fetch_one(&env.pool).await.unwrap();
    assert!(t["comments"].as_array().unwrap().iter().any(|c| c["body"].as_str().unwrap().contains("This run expired")));
    env.wait_run(id, "runner removed", 30, |v| v["runner"]["state"] == "removed").await;

    // Revive from Slack: the run relaunches from its checkpoint.
    env.put(&format!("/api/projects/{}", env.project), json!({"config": {"idle_reminder_secs": 3600, "idle_warning_secs": 7200, "idle_expiry_secs": 10800}})).await;
    env.reply(id, "U-olly", "revive").await;
    let r = env.wait_run(id, "revived run continues", 120, |v| v["state"] == "waiting" && v["pr_number"].is_i64()).await;
    assert!(r["expired_at"].is_null());
    let evs = env.events(id).await;
    assert!(evs.iter().any(|e| e["kind"] == "runner_hello" && e["payload"]["restoring"] == true));
    let audit = env.get(&format!("/api/audit?run_id={id}")).await;
    assert!(audit["entries"].as_array().unwrap().iter().any(|e| e["action"] == "run.revive" && e["actor"] == "olly@example.test"));
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_cap_stops_once_and_waits_for_a_human_and_steering_reaches_the_agent() {
    let env = Env::new(Opts { project_config: json!({"max_turns": 2}), ..Default::default() }).await;
    env.ticket("PROJ-5", "Caps").await;
    let id = env.start_ticket_run("PROJ-5").await;
    env.wait_state(id, "waiting", Some("human_answer"), 90).await;
    env.reply(id, "U-alice", "ok").await;
    let r = env.wait_state(id, "waiting", Some("cap_decision"), 120).await;
    let n = r["pr_number"].as_i64().unwrap();
    // More input arrives: held, no turn runs, no second report.
    env.pr_action(json!({"action": "thread", "number": n, "author": "reviewer", "body": "nit", "path": "README.md", "line": 1})).await;
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    let r = env.run(id).await;
    assert_eq!(r["waiting_for"], "cap_decision");
    assert_eq!(r["turns_used"], 2);
    assert_eq!(r["needs_you"], true);
    let caps = env.chat_messages().await.iter().filter(|m| m["text"].as_str().unwrap_or("").contains("hit its turn cap")).count();
    assert_eq!(caps, 1, "reported once");
    // Raise the cap: the held review input is delivered.
    env.post(&format!("/api/runs/{id}/cap"), json!({"action": "raise", "max_turns": 10})).await;
    env.wait_run(id, "held review delivered", 120, |v| v["turns_used"].as_i64().unwrap_or(0) >= 3 && v["state"] == "waiting").await;
    // Steering from the portal reaches the agent at the next turn boundary.
    env.post(&format!("/api/runs/{id}/steer"), json!({"text": "Use British spelling", "interrupt": false})).await;
    env.wait_run(id, "steer handled", 90, |v| v["turns_used"].as_i64().unwrap_or(0) >= 4 && v["state"] == "waiting").await;
    env.wait_chat("Noted: Use British spelling", 30).await;
    // End the run at a cap decision is also possible; here cancel instead.
    env.post(&format!("/api/runs/{id}/cancel"), json!({})).await;
    env.wait_state(id, "cancelled", None, 30).await;
}
