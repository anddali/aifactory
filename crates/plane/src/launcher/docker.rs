//! Docker launcher: one container per runner generation, via the docker CLI.

use super::*;
use anyhow::bail;
use tokio::process::Command;

pub struct DockerLauncher {
    /// Docker network the runners join (the factory network).
    pub network: Option<String>,
    /// Image name prefix, e.g. `ai-factory/runner-`.
    pub image_prefix: String,
}

impl DockerLauncher {
    pub fn from_env() -> Self {
        DockerLauncher {
            network: std::env::var("FACTORY_DOCKER_NETWORK").ok().filter(|s| !s.is_empty()),
            image_prefix: std::env::var("FACTORY_IMAGE_PREFIX").unwrap_or_else(|_| "ai-factory/runner-".into()),
        }
    }

    pub fn image_for(&self, stack: &str) -> String {
        format!("{}{stack}:latest", self.image_prefix)
    }

    /// Arguments for `docker run`, separated for unit testing.
    pub fn run_args(&self, spec: &LaunchSpec) -> Vec<String> {
        let mut a = vec![
            "run".to_string(),
            "-d".into(),
            "--name".into(),
            spec.name.clone(),
            "--label".into(),
            format!("factory.run={}", spec.run_id),
            "--label".into(),
            format!("factory.generation={}", spec.generation),
            "--memory".into(),
            "4g".into(),
            "--pids-limit".into(),
            "2048".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
        ];
        if let Some(n) = &self.network {
            a.push("--network".into());
            a.push(n.clone());
        }
        for (k, v) in &spec.env {
            a.push("-e".into());
            a.push(format!("{k}={v}"));
        }
        a.push(spec.image.clone());
        a
    }
}

async fn docker(args: &[String]) -> Result<(i32, String, String)> {
    let o = Command::new("docker").args(args).output().await?;
    Ok((
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).trim().to_string(),
        String::from_utf8_lossy(&o.stderr).trim().to_string(),
    ))
}

#[async_trait]
impl Launcher for DockerLauncher {
    fn kind(&self) -> &'static str {
        "docker"
    }

    async fn launch(&self, spec: &LaunchSpec) -> Result<String> {
        assert_env_clean(&spec.env)?;
        // Idempotent on name.
        let (code, out, _) = docker(&["inspect".into(), "-f".into(), "{{.Id}} {{.State.Running}}".into(), spec.name.clone()]).await?;
        if code == 0 {
            let mut parts = out.split_whitespace();
            let id = parts.next().unwrap_or_default().to_string();
            if parts.next() == Some("true") {
                return Ok(id);
            }
            // A stopped container with this name: remove and start fresh.
            let _ = docker(&["rm".into(), "-f".into(), spec.name.clone()]).await;
        }
        let (code, out, err) = docker(&self.run_args(spec)).await?;
        if code != 0 {
            if err.contains("is already in use") {
                let (_, out, _) = docker(&["inspect".into(), "-f".into(), "{{.Id}}".into(), spec.name.clone()]).await?;
                return Ok(out);
            }
            bail!("docker run failed: {err}");
        }
        Ok(out)
    }

    async fn remove(&self, handle: &str) -> Result<()> {
        let (code, _, err) = docker(&["rm".into(), "-f".into(), "-v".into(), handle.to_string()]).await?;
        if code != 0 && !err.contains("No such container") {
            bail!("docker rm failed: {err}");
        }
        Ok(())
    }

    async fn alive(&self, handle: &str) -> Result<Option<bool>> {
        let (code, out, _) = docker(&["inspect".into(), "-f".into(), "{{.State.Running}}".into(), handle.to_string()]).await?;
        Ok(Some(code == 0 && out == "true"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn run_args_carry_only_given_env() {
        let l = DockerLauncher { network: Some("factory".into()), image_prefix: "x/runner-".into() };
        let spec = LaunchSpec {
            run_id: Uuid::nil(),
            generation: 2,
            name: "factory-run-x-g2".into(),
            image: l.image_for("rust"),
            env: vec![("FACTORY_RUN_TOKEN".into(), "t".into())],
        };
        let a = l.run_args(&spec);
        assert!(a.contains(&"--network".to_string()));
        assert!(a.contains(&"FACTORY_RUN_TOKEN=t".to_string()));
        assert_eq!(a.last().unwrap(), "x/runner-rust:latest");
        assert_eq!(a.iter().filter(|x| *x == "-e").count(), 1);
    }
}
