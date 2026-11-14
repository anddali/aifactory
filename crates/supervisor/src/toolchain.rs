//! Read the repository's own version pins and install the exact version
//! when the image lacks it (through the image's tool-version manager, mise).

use proto::{HarnessEvent, RunnerConfig};
use std::path::Path;
use tokio::process::Command;

#[derive(Debug, PartialEq)]
pub struct Pin {
    pub tool: &'static str,
    pub version: String,
    pub file: &'static str,
}

pub fn read_pins(repo: &Path) -> Vec<Pin> {
    let mut pins = vec![];
    let read = |f: &str| std::fs::read_to_string(repo.join(f)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(v) = read(".nvmrc").or_else(|| read(".node-version")) {
        pins.push(Pin { tool: "node", version: v.trim_start_matches('v').to_string(), file: ".nvmrc" });
    }
    if let Some(v) = read(".python-version") {
        pins.push(Pin { tool: "python", version: v.lines().next().unwrap_or("").to_string(), file: ".python-version" });
    }
    if let Some(v) = read("global.json") {
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&v) {
            if let Some(s) = j["sdk"]["version"].as_str() {
                pins.push(Pin { tool: "dotnet", version: s.to_string(), file: "global.json" });
            }
        }
    }
    if let Some(v) = read("rust-toolchain.toml") {
        if let Some(line) = v.lines().find(|l| l.trim_start().starts_with("channel")) {
            let ver = line.split('=').nth(1).unwrap_or("").trim().trim_matches('"').to_string();
            pins.push(Pin { tool: "rust", version: ver, file: "rust-toolchain.toml" });
        }
    } else if let Some(v) = read("rust-toolchain") {
        pins.push(Pin { tool: "rust", version: v, file: "rust-toolchain" });
    }
    pins
}

async fn installed(tool: &str) -> Option<String> {
    let (cmd, arg) = match tool {
        "node" => ("node", "--version"),
        "python" => ("python3", "--version"),
        "dotnet" => ("dotnet", "--version"),
        "rust" => ("rustc", "--version"),
        _ => return None,
    };
    let o = Command::new(cmd).arg(arg).output().await.ok()?;
    let s = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    Some(s.trim().to_string())
}

pub async fn ensure(repo: &Path, events: &tokio::sync::mpsc::UnboundedSender<HarnessEvent>) {
    for pin in read_pins(repo) {
        let have = installed(pin.tool).await.unwrap_or_default();
        if have.contains(&pin.version) {
            let _ = events.send(HarnessEvent::Log { level: "info".into(), message: format!("{} {} already installed ({})", pin.tool, pin.version, pin.file) });
            continue;
        }
        let spec = format!("{}@{}", pin.tool, pin.version);
        let r = Command::new("mise").args(["use", "--global", &spec]).current_dir(repo).output().await;
        let msg = match r {
            Ok(o) if o.status.success() => format!("installed {spec} from {}", pin.file),
            Ok(o) => format!("could not install {spec}: {}", String::from_utf8_lossy(&o.stderr).trim()),
            Err(_) => format!("{spec} pinned by {} but no tool-version manager is available", pin.file),
        };
        let _ = events.send(HarnessEvent::Log { level: "info".into(), message: msg });
    }
}

/// Read `.factory/runner.yml` if present.
pub fn runner_config(repo: &Path) -> Option<RunnerConfig> {
    let s = std::fs::read_to_string(repo.join(".factory").join("runner.yml")).ok()?;
    serde_norway::from_str(&s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_pins() {
        let d = std::env::temp_dir().join(format!("pins-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(".nvmrc"), "v20.11.0\n").unwrap();
        std::fs::write(d.join("global.json"), r#"{"sdk":{"version":"8.0.100"}}"#).unwrap();
        std::fs::write(d.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"1.80.0\"\n").unwrap();
        let p = read_pins(&d);
        assert!(p.contains(&Pin { tool: "node", version: "20.11.0".into(), file: ".nvmrc" }));
        assert!(p.iter().any(|x| x.tool == "dotnet" && x.version == "8.0.100"));
        assert!(p.iter().any(|x| x.tool == "rust" && x.version == "1.80.0"));
        std::fs::remove_dir_all(d).ok();
    }
}
