//! Delivery guarantee: killing a plane replica or a runner mid-run loses no
//! work and duplicates no side effect.

mod common;
use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn runner_crash_mid_turn_and_replica_kill_cause_no_duplicates() {
    let mut env = Env::new(Opts { replicas: 2, fault: Some("crash_after_push"), ..Default::default() }).await;
    env.ticket("PROJ-2", "Crash test").await;
    let id = env.start_ticket_run("PROJ-2").await;
    env.wait_state(id, "waiting", Some("human_answer"), 90).await;

    // Kill one replica; the other carries on (singleton duties fail over via
    // the advisory lock, the runner fails over via its URL list).
    env.replicas[0].kill();

    // The answer starts a turn in which the runner crashes right after its
    // push and before opening the PR.
    env.reply(id, "U-alice", "Go").await;
    let r = env.wait_state(id, "waiting", Some("review"), 180).await;
    let evs = env.events(id).await;
    assert!(evs.iter().any(|e| e["kind"] == "runner_lost"), "runner loss was detected");
    assert!(r["runner"]["generation"].as_i64().unwrap() >= 2);

    // Exactly one PR, one announcement, and the branch carries exactly one
    // commit on top of main (the replayed turn produced the same commit).
    let n = r["pr_number"].as_i64().unwrap();
    assert_eq!(n, 1);
    let (s, _) = env.post_status(&format!("/api/fake/{}/pr", env.repo_conn), json!({"repo": env.repo_name, "number": 2})).await;
    assert!(s >= 400, "no second PR");
    let opened = env.chat_messages().await.iter().filter(|m| m["text"].as_str().unwrap_or("").starts_with("Opened PR")).count();
    assert_eq!(opened, 1);
    let branch = r["branch"].as_str().unwrap();
    let count = env.git_upstream(&["rev-list", "--count", &format!("main..refs/heads/{branch}")]);
    assert_eq!(count, "1", "one commit, not two");
    // The interrupted turn was delivered again to the new runner generation.
    let starts: Vec<_> = evs.iter().filter(|e| e["kind"] == "turn_started" && e["payload"]["kind"] == "answer").collect();
    assert!(starts.len() >= 2, "the answer turn ran again after the crash");
    assert_eq!(starts[0]["payload"]["instruction_id"], starts[1]["payload"]["instruction_id"]);
    assert_ne!(starts[0]["payload"]["generation"], starts[1]["payload"]["generation"]);

    // Bring the replica back and finish on it.
    env.replicas[0].start();
    wait_http(&env.http, &format!("{}/readyz", env.replicas[0].url())).await;
    env.pr_action(json!({"action": "review", "number": n, "author": "reviewer", "state": "approved"})).await;
    env.wait_state(id, "finished", None, 60).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_events_and_retried_jobs_are_idempotent() {
    let env = Env::new(Opts::default()).await;
    env.ticket("PROJ-3", "Dupes").await;
    let id = env.start_ticket_run("PROJ-3").await;
    env.wait_state(id, "waiting", Some("human_answer"), 90).await;
    // The same Slack event delivered three times (webhook retries) counts once.
    let ts = env.thread_of(id).await;
    for _ in 0..3 {
        sqlx::query("INSERT INTO fake_store(ns,id,data) VALUES ($1,'dup-evt',$2) ON CONFLICT DO NOTHING")
            .bind(format!("fake_chat:{}", env.chat))
            .bind(json!({"channel":"#factory","ts":"dup-evt","thread_ts":ts,"user":"U-alice","text":"Proceed"}))
            .execute(&env.pool).await.unwrap();
        env.post(&format!("/api/fake/{}/chat/redeliver", env.chat), json!({"id": "dup-evt", "channel": "#factory", "thread_ts": ts, "user": "U-alice", "text": "Proceed"})).await;
    }
    env.wait_state(id, "waiting", Some("review"), 120).await;
    env.wait_state(id, "waiting", Some("review"), 120).await;
    // A second answer to the already-answered question is refused.
    let q = env.run(id).await["questions"][0]["id"].as_str().unwrap().to_string();
    let alice = env.login("alice@example.test").await;
    let (s, _) = env.req(reqwest::Method::POST, &format!("/api/questions/{q}/answer"), Some(json!({"answer": "again"})), &alice).await;
    assert_eq!(s, 409, "first answer wins");
    assert_eq!(env.count_events(id, "question_answered").await, 1);
    // The chat message for the question was updated to show the answer.
    let msgs = env.chat_messages().await;
    assert!(msgs.iter().any(|m| m["edited"] == true && m["text"].as_str().unwrap_or("").contains("answered by alice@example.test")));
}
