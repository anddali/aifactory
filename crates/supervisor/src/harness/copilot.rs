//! GitHub Copilot CLI adapter, bring-your-own-model mode.
//!
//! Verified against Copilot CLI 1.0.x:
//! - BYOK through `COPILOT_PROVIDER_BASE_URL` / `COPILOT_PROVIDER_API_KEY` /
//!   `COPILOT_MODEL`, with `COPILOT_OFFLINE=true` so no GitHub sign-in or
//!   GitHub traffic is involved (offline mode also disables auto-update, so the
//!   pinned CLI version in the image is the one that runs);
//! - headless turns with `-p <prompt> --allow-all-tools --no-ask-user`;
//! - `--output-format=json` JSONL events (`assistant.message`,
//!   `tool.execution_start`, `tool.execution_complete`, `assistant.turn_end`);
//! - `--resume=<uuid>` both creates a session with that id and resumes it;
//!   session state lives under `$COPILOT_HOME/session-state/<uuid>`;
//! - `--additional-mcp-config=@file` adds the `factory` MCP server, whose tools
//!   appear as `factory-<tool>`;
//! - `--secret-env-vars` strips the run token from shell environments.

use super::*;
use anyhow::{Context, bail};
use serde_json::json;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[derive(Default)]
pub struct Copilot {
    session_id: Option<String>,
}

const HOME: &str = "copilot-home";
const SID: &str = "copilot-session-id";

pub fn binary() -> String {
    std::env::var("FACTORY_COPILOT_BIN").unwrap_or_else(|_| "copilot".into())
}

/// Program and leading arguments that start the Copilot CLI. On Windows the
/// npm shim is a batch file, which cannot carry multi-line prompts, so the
/// CLI's node entry point is run directly.
pub fn launcher() -> (String, Vec<String>) {
    let bin = binary();
    if bin.ends_with(".js") {
        return ("node".into(), vec![bin]);
    }
    if cfg!(windows) && !bin.ends_with(".exe") {
        let dirs = std::env::var("PATH").unwrap_or_default();
        let candidates: Vec<std::path::PathBuf> = if bin.contains(['\\', '/']) {
            vec![std::path::PathBuf::from(&bin).parent().map(|p| p.to_path_buf()).unwrap_or_default()]
        } else {
            std::env::split_paths(&dirs).collect()
        };
        for d in candidates {
            let loader = d.join("node_modules").join("@github").join("copilot").join("npm-loader.js");
            if loader.exists() {
                return ("node".into(), vec![loader.to_string_lossy().into_owned()]);
            }
        }
    }
    (bin, vec![])
}

fn factory_bin() -> String {
    std::env::var("FACTORY_BIN").unwrap_or_else(|_| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join(if cfg!(windows) { "factory.exe" } else { "factory" })))
            .filter(|p| p.exists())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "factory".into())
    })
}

/// Map one Copilot JSONL record to normalised events.
pub fn map_event(v: &Value) -> Vec<HarnessEvent> {
    let d = &v["data"];
    let s = |k: &str| d[k].as_str().unwrap_or("").to_string();
    match v["type"].as_str().unwrap_or("") {
        "assistant.turn_start" => vec![HarnessEvent::ModelTurn { model: None, text: None }],
        "assistant.message" => {
            let mut out = vec![];
            let text = s("content");
            if !text.is_empty() {
                out.push(HarnessEvent::AgentMessage { text });
            }
            if let Some(n) = d["outputTokens"].as_u64() {
                out.push(HarnessEvent::Usage { model: None, input_tokens: d["inputTokens"].as_u64().unwrap_or(0), output_tokens: n });
            }
            out
        }
        "tool.execution_start" => vec![HarnessEvent::ToolCall { call_id: s("toolCallId"), name: s("toolName"), args: d["arguments"].clone() }],
        "tool.execution_complete" => vec![HarnessEvent::ToolResult {
            call_id: s("toolCallId"),
            ok: d["success"].as_bool().unwrap_or(false),
            output: d.get("result").cloned().or_else(|| d.get("error").cloned()).unwrap_or(Value::Null),
        }],
        "session.error" | "error" => vec![HarnessEvent::Error { message: d["message"].as_str().map(String::from).unwrap_or_else(|| d.to_string()) }],
        _ => vec![],
    }
}

impl Copilot {
    fn sid_path(ws: &Workspace) -> PathBuf {
        ws.session.join(SID)
    }

    fn mcp_config(&self, ws: &Workspace, instruction_id: i64) -> Result<PathBuf> {
        let path = ws.root.join("factory-mcp.json");
        let cfg = json!({"mcpServers": {"factory": {
            "type": "local", "command": factory_bin(), "args": ["mcp"], "tools": ["*"],
            "env": {
                "FACTORY_PLANE_URLS": std::env::var("FACTORY_PLANE_URLS").unwrap_or_default(),
                "FACTORY_RUN_TOKEN": ws.client.token(),
                "FACTORY_INSTRUCTION_ID": instruction_id.to_string(),
            }
        }}});
        std::fs::write(&path, serde_json::to_vec_pretty(&cfg)?)?;
        Ok(path)
    }

    pub fn command(&self, ws: &Workspace, prompt: &str, mcp: &std::path::Path) -> Command {
        let (prog, pre) = launcher();
        let mut c = Command::new(prog);
        c.args(pre);
        let sid = self.session_id.clone().unwrap_or_default();
        c.current_dir(&ws.repo)
            .arg("-p")
            .arg(prompt)
            .args(["--allow-all-tools", "--no-ask-user", "--output-format=json", "--disable-builtin-mcps", "--no-color"])
            .arg(format!("--resume={sid}"))
            .arg(format!("--additional-mcp-config=@{}", mcp.display()))
            .arg("--secret-env-vars=FACTORY_RUN_TOKEN,COPILOT_PROVIDER_API_KEY")
            .arg(format!("--model={}", ws.spec.model.model))
            .env("COPILOT_HOME", ws.session.join(HOME))
            .env("COPILOT_OFFLINE", "true")
            .env("COPILOT_PROVIDER_BASE_URL", &ws.spec.model.base_url)
            .env("COPILOT_PROVIDER_API_KEY", &ws.spec.model.api_key)
            .env("COPILOT_PROVIDER_TYPE", "openai")
            .env("COPILOT_MODEL", &ws.spec.model.model)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("COPILOT_GITHUB_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        c
    }
}

#[async_trait]
impl Harness for Copilot {
    fn name(&self) -> &'static str {
        "copilot"
    }
    fn capabilities(&self) -> Capabilities {
        // Output tokens only; the plane meters full usage at its endpoint.
        Capabilities { resume: true, usage: true, interrupt: true }
    }
    async fn prepare(&mut self, ws: &Workspace) -> Result<()> {
        std::fs::create_dir_all(ws.session.join(HOME))?;
        let sid = match std::fs::read_to_string(Self::sid_path(ws)) {
            Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => {
                let s = Uuid::new_v4().to_string();
                std::fs::write(Self::sid_path(ws), &s)?;
                s
            }
        };
        self.session_id = Some(sid);
        let (prog, pre) = launcher();
        let o = Command::new(&prog).args(&pre).arg("--version").output().await.with_context(|| format!("{} is not installed", binary()))?;
        if !o.status.success() {
            bail!("copilot --version failed");
        }
        Ok(())
    }

    async fn run_turn(&mut self, ctx: TurnCtx<'_>, input: TurnInput) -> Result<TurnOutcome> {
        let ws = ctx.ws;
        let mcp = self.mcp_config(ws, input.instruction_id)?;
        let mut prompt = String::new();
        if let Some(h) = &input.handoff {
            prompt.push_str("You are continuing a run whose previous session could not be resumed. Summary so far:\n\n");
            prompt.push_str(h);
            prompt.push_str("\n\n---\n\n");
        }
        prompt.push_str(&format!("[{} input]\n{}\n", input.kind.as_str(), input.text));
        prompt.push_str("\nUse the `factory` tools for every action on tickets, pull requests, CI and humans. When you ask a human (factory-human_ask or factory-human_request_approval), end your turn immediately.");
        if let Some(p) = ctx.deny.check(&prompt) {
            let _ = ctx.events.send(HarnessEvent::Log { level: "warn".into(), message: format!("prompt mentions a denied command pattern ({p})") });
        }
        let mut child = self.command(ws, &prompt, &mcp).spawn().context("starting copilot")?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let ev = ctx.events.clone();
        let deny_hit = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let patterns = ctx.ws.spec.deny_patterns.clone();
        let dh = deny_hit.clone();
        let reader = tokio::spawn(async move {
            let deny = crate::deny::DenyList::new(&patterns);
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
                for e in map_event(&v) {
                    if let HarnessEvent::ToolCall { args, .. } = &e {
                        let cmd = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
                        if let Some(p) = deny.check(cmd) {
                            *dh.lock().unwrap() = Some(p);
                        }
                    }
                    let _ = ev.send(e);
                }
            }
        });
        let ev2 = ctx.events.clone();
        let err_reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut tail = vec![];
            while let Ok(Some(l)) = lines.next_line().await {
                tail.push(l);
                if tail.len() > 20 {
                    tail.remove(0);
                }
            }
            if !tail.is_empty() {
                let _ = ev2.send(HarnessEvent::Log { level: "debug".into(), message: tail.join("\n") });
            }
        });
        let mut interrupt = ctx.interrupt.clone();
        let mut denied = false;
        let status = loop {
            tokio::select! {
                s = child.wait() => break s?,
                _ = interrupt.changed() => {
                    if *interrupt.borrow() {
                        let _ = child.start_kill();
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                    if deny_hit.lock().unwrap().is_some() && !denied {
                        denied = true;
                        let _ = child.start_kill();
                    }
                }
            }
        };
        let _ = reader.await;
        let _ = err_reader.await;
        if let Some(p) = deny_hit.lock().unwrap().clone() {
            let _ = ctx.events.send(HarnessEvent::Error { message: format!("shell command matched deny-list pattern {p}; turn stopped") });
            return Ok(TurnOutcome::Denied);
        }
        if *ctx.interrupt.borrow() {
            return Ok(TurnOutcome::Interrupted);
        }
        Ok(if status.success() { TurnOutcome::Completed } else { TurnOutcome::Failed })
    }

    fn session_files(&self) -> Vec<PathBuf> {
        vec![PathBuf::from(HOME), PathBuf::from(SID)]
    }

    fn has_session(&self, ws: &Workspace) -> bool {
        match std::fs::read_to_string(Self::sid_path(ws)) {
            Ok(s) => ws.session.join(HOME).join("session-state").join(s.trim()).exists(),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_observed_copilot_events() {
        let lines = [
            r#"{"type":"assistant.turn_start","data":{"turnId":"0"}}"#,
            r#"{"type":"assistant.message","data":{"content":"","toolRequests":[{"toolCallId":"call_1","name":"factory-status_report"}],"outputTokens":22}}"#,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"call_1","toolName":"factory-status_report","arguments":{"text":"hi"},"mcpServerName":"factory"}}"#,
            r#"{"type":"tool.execution_complete","data":{"toolCallId":"call_1","success":true,"result":{"content":"recorded"}}}"#,
            r#"{"type":"assistant.message","data":{"content":"All done.","toolRequests":[],"outputTokens":5}}"#,
            r#"{"type":"session.skills_loaded","data":{}}"#,
        ];
        let evs: Vec<HarnessEvent> = lines.iter().flat_map(|l| map_event(&serde_json::from_str(l).unwrap())).collect();
        assert!(matches!(evs[0], HarnessEvent::ModelTurn { .. }));
        assert!(matches!(&evs[1], HarnessEvent::Usage { output_tokens: 22, .. }));
        assert!(matches!(&evs[2], HarnessEvent::ToolCall { name, .. } if name == "factory-status_report"));
        assert!(matches!(&evs[3], HarnessEvent::ToolResult { ok: true, .. }));
        assert!(matches!(&evs[4], HarnessEvent::AgentMessage { text } if text == "All done."));
        assert_eq!(evs.len(), 6);
    }
}
