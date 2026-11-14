//! Phase 1 acceptance: a rehearsal ticket-build run goes from trigger to
//! approved PR against the fakes, survives a runner kill and a plane restart.

mod common;
use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn ticket_build_to_approved_pr_surviving_runner_kill_and_plane_restart() {
    let mut env = Env::new(Opts::default()).await;
    env.ticket("PROJ-1", "Add widget docs").await;
    let id = env.start_ticket_run("PROJ-1").await;

    // Research turn ends with a question to the assignee.
    let r = env.wait_state(id, "waiting", Some("human_answer"), 90).await;
    assert_eq!(r["questions"].as_array().unwrap().len(), 1);
    let msgs = env.chat_messages().await;
    assert!(msgs.iter().any(|m| m["text"].as_str().unwrap_or("").starts_with("Question:")), "question posted to chat");

    // Second trigger for the same ticket is refused by the uniqueness constraint.
    let (s, _) = env.post_status("/api/runs", json!({"project_id": env.project, "workflow": "ticket_build", "ticket_key": "PROJ-1"})).await;
    assert_eq!(s, 409);

    // Kill the warm runner outright.
    let pid = env.runner_pid(id).await.expect("runner pid");
    kill_pid(pid);
    env.wait_run(id, "runner lost", 30, |v| v["runner"]["state"] == "lost" || v["runner"]["state"] == "stopped").await;

    // Restart the plane.
    env.replicas[0].kill();
    env.replicas[0].start();
    wait_http(&env.http, &format!("{}/readyz", env.url())).await;

    // The assignee answers in the Slack thread: a new runner restores from the
    // checkpoint and implements, pushes and opens a PR.
    env.reply(id, "U-alice", "Yes, go ahead").await;
    let r = env.wait_state(id, "waiting", Some("review"), 120).await;
    let n = r["pr_number"].as_i64().expect("PR opened");
    assert!(r["runner"]["generation"].as_i64().unwrap() >= 2, "runner was relaunched");
    let evs = env.events(id).await;
    assert!(evs.iter().any(|e| e["kind"] == "runner_hello" && e["payload"]["restoring"] == true), "relaunched runner restored a checkpoint");
    let branch = r["branch"].as_str().unwrap().to_string();
    assert!(!env.git_upstream(&["rev-parse", &format!("refs/heads/{branch}")]).is_empty(), "branch pushed upstream");

    // A reviewer opens a thread; the agent fixes, replies and resolves it.
    env.pr_action(json!({"action": "thread", "number": n, "author": "reviewer", "body": "Please mention the API", "path": "README.md", "line": 1})).await;
    env.wait_run(id, "thread resolved", 120, |_| true).await;
    let start = std::time::Instant::now();
    loop {
        let pr = env.pr(n).await;
        if pr["threads"][0]["status"] == "resolved" {
            assert!(pr["threads"][0]["comments"].as_array().unwrap().len() >= 2, "agent replied");
            break;
        }
        assert!(start.elapsed().as_secs() < 120, "thread never resolved: {pr}");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    env.wait_state(id, "waiting", Some("review"), 60).await;

    // Approval ends the run.
    env.pr_action(json!({"action": "review", "number": n, "author": "reviewer", "state": "approved", "body": "LGTM"})).await;
    let r = env.wait_state(id, "finished", None, 60).await;
    assert_eq!(r["outcome"], "approved");
    env.wait_run(id, "runner removed", 30, |v| v["runner"]["state"] == "removed").await;

    // No duplicated side effects.
    let (s, _) = env.post_status(&format!("/api/fake/{}/pr", env.repo_conn), json!({"repo": env.repo_name, "number": 2})).await;
    assert!(s >= 400, "exactly one PR was opened");
    let msgs = env.chat_messages().await;
    let questions = msgs.iter().filter(|m| m["text"].as_str().unwrap_or("").starts_with("Question:")).count();
    assert_eq!(questions, 1, "question posted once");
    let opened = msgs.iter().filter(|m| m["text"].as_str().unwrap_or("").starts_with("Opened PR")).count();
    assert_eq!(opened, 1, "PR announcement posted once");

    // The event log alone rebuilds the run's state.
    let rb = env.get(&format!("/api/runs/{id}/rebuild")).await;
    assert_eq!(rb["state"], rb["stored_state"]);
    assert_eq!(rb["state"], "finished");
}
