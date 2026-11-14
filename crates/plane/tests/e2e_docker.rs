//! The Docker launcher with the real runner image: a rehearsal ticket build
//! where the runner is a container. Skipped when Docker or the
//! `ai-factory/runner-rust` image is not available
//! (`docker compose --profile images build runner-base runner-rust`).

mod common;
use common::*;
use serde_json::json;

fn docker_ready() -> bool {
    std::process::Command::new("docker")
        .args(["image", "inspect", "ai-factory/runner-rust:latest"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn container_running(name_part: &str) -> bool {
    let o = std::process::Command::new("docker").args(["ps", "--format", "{{.Names}}"]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).lines().any(|l| l.contains(name_part))
}

#[tokio::test(flavor = "multi_thread")]
async fn ticket_build_with_docker_runner_containers() {
    if !docker_ready() {
        eprintln!("SKIPPED: runner image not built");
        return;
    }
    let env = Env::new(Opts { docker: true, ..Default::default() }).await;
    env.ticket("PROJ-40", "Docker").await;
    let id = env.start_ticket_run("PROJ-40").await;
    let r = env.wait_state(id, "waiting", Some("human_answer"), 240).await;
    assert!(r["runner"]["handle"].as_str().unwrap().starts_with("docker|"));
    assert_eq!(r["runner"]["image"], "rust", "stack detection picked the rust image");
    let simple = id.simple().to_string();
    assert!(container_running(&simple), "runner container is running");
    // The container's environment holds only the run's own credentials.
    let o = std::process::Command::new("docker").args(["inspect", "-f", "{{range .Config.Env}}{{println .}}{{end}}", &format!("factory-run-{simple}-g1")]).output().unwrap();
    let envs = String::from_utf8_lossy(&o.stdout).to_string();
    assert!(envs.contains("FACTORY_RUN_TOKEN=frt_"));
    assert!(!envs.contains("DATABASE_URL") && !envs.contains("ENCRYPTION"), "{envs}");
    env.reply(id, "U-alice", "go").await;
    let r = env.wait_state(id, "waiting", Some("review"), 240).await;
    let n = r["pr_number"].as_i64().unwrap();
    env.pr_action(json!({"action": "review", "number": n, "author": "reviewer", "state": "approved"})).await;
    env.wait_state(id, "finished", None, 120).await;
    env.wait_run(id, "container removed", 60, |v| v["runner"]["state"] == "removed").await;
    let start = std::time::Instant::now();
    while container_running(&simple) {
        assert!(start.elapsed().as_secs() < 60, "container was not removed");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}
