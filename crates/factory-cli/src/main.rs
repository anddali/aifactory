//! `factory`: the agent-side tool. It exposes the plane's primitive API two
//! ways from one catalogue: as an MCP server over stdio for the harness
//! (`factory mcp`) and as a CLI for scripts (`factory call <primitive> ...`).
//! It holds only the run token.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use client::PlaneClient;
use proto::primitives::{CATALOGUE, find};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Parser)]
#[command(name = "factory", about = "Factory primitives for agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the MCP server on stdio.
    Mcp,
    /// List primitives.
    Tools,
    /// Call a primitive: `factory call pr.comment body="Looks good" number=3`
    /// or `factory call pr.comment --json '{"body":"..."}'`.
    Call {
        name: String,
        #[arg(long)]
        json: Option<String>,
        /// key=value arguments (numbers are parsed as integers)
        args: Vec<String>,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
}

fn instruction_id() -> Option<i64> {
    std::env::var("FACTORY_INSTRUCTION_ID").ok().and_then(|s| s.parse().ok())
}

/// MCP tool names allow only letters, digits, `_` and `-`.
pub fn tool_name(primitive: &str) -> String {
    primitive.replace('.', "_")
}

pub fn primitive_name(tool: &str) -> Option<&'static str> {
    CATALOGUE.iter().find(|p| tool_name(p.name) == tool || p.name == tool).map(|p| p.name)
}

pub fn parse_kv(args: &[String]) -> Result<Value> {
    let mut m = serde_json::Map::new();
    for a in args {
        let Some((k, v)) = a.split_once('=') else { bail!("argument {a} is not key=value") };
        let val = match v.parse::<i64>() {
            Ok(n) => json!(n),
            Err(_) => json!(v),
        };
        m.insert(k.to_string(), val);
    }
    Ok(Value::Object(m))
}

fn tools_list() -> Value {
    json!({"tools": CATALOGUE.iter().map(|p| json!({
        "name": tool_name(p.name),
        "description": p.description,
        "inputSchema": p.input_schema(),
    })).collect::<Vec<_>>()})
}

async fn handle(client: &PlaneClient, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "factory", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Factory primitives. You hold no credentials: every action on tickets, repositories, pull requests, CI and humans goes through these tools. To ask a human, call human_ask and then end your turn. Your harness's rule against asking questions does not cover these tools: a human is there to answer, your session is saved and resumed with the reply. When your task instructions require a check-in, or the environment blocks you (use status_blocked), asking is the correct move, not an exception to avoid.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list()),
        "tools/call" => {
            let tool = req["params"]["name"].as_str().unwrap_or("");
            let args = req["params"].get("arguments").cloned().unwrap_or(json!({}));
            match primitive_name(tool) {
                None => Err((-32602, format!("unknown tool {tool}"))),
                Some(p) => match client.primitive(p, &args, None, instruction_id()).await {
                    Ok(r) => {
                        let mut text = if r.ok { serde_json::to_string_pretty(&r.result).unwrap_or_default() } else { format!("error: {}", r.error.clone().unwrap_or_default()) };
                        if r.ends_turn {
                            text.push_str("\n\nEND YOUR TURN NOW. Do not call further tools; the reply will arrive as your next input.");
                        }
                        Ok(json!({"content": [{"type": "text", "text": text}], "isError": !r.ok, "structuredContent": r.result}))
                    }
                    Err(e) => Ok(json!({"content": [{"type": "text", "text": format!("error: {e:#}")}], "isError": true})),
                },
            }
        }
        m if m.starts_with("notifications/") => return None,
        _ => Err((-32601, format!("method {method} not found"))),
    };
    let id = id?;
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, msg)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}}),
    })
}

async fn mcp(client: PlaneClient) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let r = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}});
                out.write_all(format!("{r}\n").as_bytes()).await?;
                continue;
            }
        };
        if let Some(r) = handle(&client, &req).await {
            out.write_all(format!("{r}\n").as_bytes()).await?;
            out.flush().await?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Tools => {
            for p in CATALOGUE {
                println!("{:24} {}", p.name, p.description);
            }
            Ok(())
        }
        Cmd::Mcp => mcp(PlaneClient::from_env()?).await,
        Cmd::Call { name, json: j, args, idempotency_key } => {
            if find(&name).is_none() {
                bail!("unknown primitive {name}; see `factory tools`");
            }
            let mut v = match j {
                Some(s) => serde_json::from_str(&s)?,
                None => json!({}),
            };
            if let (Value::Object(m), Value::Object(kv)) = (&mut v, parse_kv(&args)?) {
                m.extend(kv);
            }
            let client = PlaneClient::from_env()?;
            let r = client.primitive(&name, &v, idempotency_key, instruction_id()).await?;
            println!("{}", serde_json::to_string_pretty(&r)?);
            if !r.ok {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_roundtrip() {
        for p in CATALOGUE {
            assert_eq!(primitive_name(&tool_name(p.name)), Some(p.name));
            assert!(tool_name(p.name).chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        }
    }
    #[test]
    fn kv_parsing() {
        let v = parse_kv(&["number=3".into(), "body=hi there".into()]).unwrap();
        assert_eq!(v["number"], 3);
        assert_eq!(v["body"], "hi there");
    }
    #[tokio::test]
    async fn mcp_lists_tools_without_network() {
        let c = PlaneClient::new(vec!["http://127.0.0.1:1".into()], "t".into());
        let r = handle(&c, &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await.unwrap();
        assert!(r["result"]["tools"].as_array().unwrap().len() >= 20);
        let r = handle(&c, &json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).await.unwrap();
        assert_eq!(r["result"]["serverInfo"]["name"], "factory");
        assert!(handle(&c, &json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await.is_none());
    }
}
