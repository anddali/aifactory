//! Phase 2: the real Copilot CLI adapter code path, end to end, with no
//! credentials: Copilot CLI (BYOK, offline) → the plane's model endpoint with
//! the run-scoped key → the fake provider (which requires the provider key
//! that only the plane holds). Copilot's MCP tool calls go through the
//! `factory` binary to the real primitives.
//!
//! Skipped when the Copilot CLI is not installed (set FACTORY_COPILOT_BIN to
//! point at it).

mod common;
use common::*;
use serde_json::json;

fn copilot_available() -> bool {
    let out = match local_copilot() {
        Some(js) if js.ends_with(".js") => std::process::Command::new("node").arg(js).arg("--help").output(),
        Some(b) => std::process::Command::new(b).arg("--help").output(),
        None => std::process::Command::new(if cfg!(windows) { "copilot.cmd" } else { "copilot" }).arg("--help").output(),
    };
    // The adapter needs JSONL output (Copilot CLI 1.0+).
    out.map(|o| String::from_utf8_lossy(&o.stdout).contains("--output-format")).unwrap_or(false)
}

async fn copilot_env(budget: f64) -> Option<Env> {
    if !copilot_available() {
        eprintln!("SKIPPED: Copilot CLI not installed");
        return None;
    }
    let env = Env::new(Opts { project_config: json!({"harness": "copilot", "budget_usd": budget, "default_model": "gpt-4.1"}), ..Default::default() }).await;
    let m = env.connector("fake_model", json!({})).await;
    env.put(&format!("/api/connectors/{m}/secrets"), json!({"secrets": {"api_key": "fake-provider-key-1"}})).await;
    env.put("/api/settings", json!({"model_connector_id": m})).await;
    let t = env.post(&format!("/api/connectors/{m}/test"), json!({})).await;
    assert_eq!(t["ok"], true, "{t}");
    Some(env)
}

#[tokio::test(flavor = "multi_thread")]
async fn copilot_turn_through_metered_model_endpoint_and_mcp_primitives() {
    let Some(env) = copilot_env(10.0).await else { return };
    env.ticket("PROJ-20", "Copilot").await;
    let id = env.start_ticket_run("PROJ-20").await;
    let r = env.wait_run(id, "copilot turn ended", 240, |v| v["state"] == "waiting" && v["turns_used"].as_i64().unwrap_or(0) >= 1).await;
    let evs = env.events(id).await;
    assert!(evs.iter().any(|e| e["kind"] == "harness.tool_call" && e["payload"]["name"] == "factory-status_report"), "copilot called the MCP tool");
    assert!(evs.iter().any(|e| e["kind"] == "primitive_call" && e["payload"]["name"] == "status.report" && e["payload"]["ok"] == true), "the MCP call reached the primitive");
    assert!(evs.iter().any(|e| e["kind"] == "status_report" && e["payload"]["text"] == "fake model progress"));
    assert!(evs.iter().any(|e| e["kind"] == "harness.agent_message"));
    // Metered at the plane endpoint and priced from the price table.
    assert!(r["spent_usd"].as_f64().unwrap() > 0.0, "cost recorded: {r}");
    assert!(r["tokens_in"].as_i64().unwrap() > 0);
    assert!(evs.iter().any(|e| e["kind"] == "cost" && e["payload"]["source"] == "plane_endpoint"));
    assert_eq!(r["cost_by_model"][0]["model"], "gpt-4.1");
    // The provider key never reached the runner.
    for d in std::fs::read_dir(env.replicas[0].data_dir.join("runners")).unwrap().flatten() {
        for f in walk(&d.path()) {
            if let Ok(s) = std::fs::read_to_string(&f) {
                assert!(!s.contains("fake-provider-key"), "provider key found in {}", f.display());
            }
        }
    }
    // Resume: a second turn continues the same Copilot session.
    env.post(&format!("/api/runs/{id}/steer"), json!({"text": "Carry on"})).await;
    env.wait_run(id, "second turn", 240, |v| v["turns_used"].as_i64().unwrap_or(0) >= 2 && v["state"] == "waiting").await;
    let sessions = env.events(id).await.iter().filter(|e| e["kind"] == "checkpoint_saved").count();
    assert!(sessions >= 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn budget_is_enforced_at_the_model_endpoint() {
    let Some(env) = copilot_env(0.000_001).await else { return };
    env.ticket("PROJ-21", "Budget").await;
    let id = env.start_ticket_run("PROJ-21").await;
    let r = env.wait_state(id, "waiting", Some("cap_decision"), 240).await;
    assert!(r["spent_usd"].as_f64().unwrap() >= r["budget_usd"].as_f64().unwrap());
    env.wait_chat("hit its budget cap", 30).await;
    // Further model requests with the run key are refused.
    let (salt,): (String,) = sqlx::query_as("SELECT token_salt FROM runs WHERE id=$1").bind(id).fetch_one(&env.pool).await.unwrap();
    let c = plane::crypto::Crypto::from_spec(&format!("k1:{KEY_B64}")).unwrap();
    let key = format!("fmk_{}", c.derive(&format!("model|{id}|{salt}")));
    let resp = env.http.post(format!("{}/model/v1/chat/completions", env.url())).bearer_auth(&key).json(&json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]})).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 402);
    // And a key that is not a run key gets nothing.
    let resp = env.http.post(format!("{}/model/v1/chat/completions", env.url())).bearer_auth("fake-provider-key-1").json(&json!({"messages": []})).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

fn walk(p: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                if !path.ends_with(".git") && !path.ends_with("pkg") {
                    out.extend(walk(&path));
                }
            } else if e.metadata().map(|m| m.len() < 2_000_000).unwrap_or(false) {
                out.push(path);
            }
        }
    }
    out
}
