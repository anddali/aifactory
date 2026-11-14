//! Stack detection from repository marker files.

use crate::AppState;
use crate::engine::RepoRow;
use anyhow::Result;
use serde_json::{Value, json};

pub const STACKS: [&str; 5] = ["python", "dotnet", "node", "rust", "polyglot"];

/// Stacks indicated by a list of file paths.
pub fn detect(files: &[String]) -> (String, Value) {
    let mut found: Vec<&str> = vec![];
    let mut markers: Vec<String> = vec![];
    let mut pins = serde_json::Map::new();
    for f in files {
        let name = f.rsplit('/').next().unwrap_or(f);
        let depth = f.matches('/').count();
        let stack = match name {
            "pyproject.toml" | "requirements.txt" | "setup.py" | "Pipfile" | "setup.cfg" => Some("python"),
            "package.json" => Some("node"),
            "Cargo.toml" => Some("rust"),
            n if n.ends_with(".csproj") || n.ends_with(".sln") || n.ends_with(".fsproj") => Some("dotnet"),
            _ => None,
        };
        // Ignore vendored and deeply nested manifests.
        if let Some(s) = stack {
            if depth <= 2 && !f.contains("node_modules/") {
                if !found.contains(&s) {
                    found.push(s);
                }
                markers.push(f.clone());
            }
        }
        match name {
            "global.json" | ".nvmrc" | ".node-version" | ".python-version" | "rust-toolchain.toml" | "rust-toolchain" | ".tool-versions" if depth == 0 => {
                pins.insert(name.to_string(), json!(f));
            }
            _ => {}
        }
    }
    let stack = if found.len() == 1 { found[0].to_string() } else { "polyglot".to_string() };
    (stack, json!({"stacks": found, "markers": markers, "version_pins": pins}))
}

pub async fn detect_for_repo(state: &AppState, r: &RepoRow) -> Result<(String, Value)> {
    let host = crate::connectors::repo_host(state, r.connector_id).await?;
    let files = host.list_files(&r.full_name, &r.default_branch).await?;
    Ok(detect(&files))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }
    #[test]
    fn single_and_multi() {
        assert_eq!(detect(&s(&["Cargo.toml", "src/main.rs"])).0, "rust");
        assert_eq!(detect(&s(&["pyproject.toml"])).0, "python");
        assert_eq!(detect(&s(&["src/App/App.csproj", "global.json"])).0, "dotnet");
        assert_eq!(detect(&s(&["package.json", "node_modules/x/Cargo.toml"])).0, "node");
        assert_eq!(detect(&s(&["package.json", "Cargo.toml"])).0, "polyglot");
        assert_eq!(detect(&s(&["README.md"])).0, "polyglot");
        let (_, p) = detect(&s(&[".nvmrc", "package.json"]));
        assert!(p["version_pins"].get(".nvmrc").is_some());
    }
}
