//! Local-process launcher: runs the supervisor as a child process with a
//! scrubbed environment. Used for development and the end-to-end tests.

use super::*;
use anyhow::{Context, bail};
use std::path::PathBuf;

pub struct ProcessLauncher {
    pub supervisor_bin: PathBuf,
    pub root: PathBuf,
}

impl ProcessLauncher {
    pub fn new(data_dir: &std::path::Path) -> Self {
        let exe_name = if cfg!(windows) { "supervisor.exe" } else { "supervisor" };
        let supervisor_bin = std::env::var("FACTORY_SUPERVISOR_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join(exe_name)))
                    .unwrap_or_else(|| PathBuf::from(exe_name))
            });
        ProcessLauncher { supervisor_bin, root: data_dir.join("runners") }
    }
}

pub fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")),
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

pub fn kill_pid(pid: u32) {
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
    #[cfg(not(windows))]
    let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).output();
}

fn parse_handle(handle: &str) -> Option<(u32, String)> {
    let rest = handle.strip_prefix("pid:")?;
    let (pid, name) = rest.split_once(':')?;
    Some((pid.parse().ok()?, name.to_string()))
}

#[async_trait]
impl Launcher for ProcessLauncher {
    fn kind(&self) -> &'static str {
        "process"
    }

    async fn launch(&self, spec: &LaunchSpec) -> Result<String> {
        assert_env_clean(&spec.env)?;
        let dir = self.root.join(&spec.name);
        let pid_file = dir.join("supervisor.pid");
        if let Ok(p) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = p.trim().parse::<u32>() {
                if pid_alive(pid) {
                    return Ok(format!("pid:{pid}:{}", spec.name));
                }
            }
        }
        let work = dir.join("work");
        std::fs::create_dir_all(&work)?;
        if !self.supervisor_bin.exists() {
            bail!("supervisor binary not found at {}", self.supervisor_bin.display());
        }
        let mut cmd = tokio::process::Command::new(&self.supervisor_bin);
        cmd.env_clear();
        // Only what git and the OS need, plus the runner's own variables.
        for k in ["PATH", "SYSTEMROOT", "SystemRoot", "COMSPEC", "PATHEXT", "WINDIR", "TEMP", "TMP", "LANG"] {
            if let Ok(v) = std::env::var(k) {
                cmd.env(k, v);
            }
        }
        cmd.env("HOME", &dir).env("USERPROFILE", &dir).env("FACTORY_WORKDIR", &work);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let log = std::fs::File::create(dir.join("supervisor.log")).context("runner log")?;
        let log2 = log.try_clone()?;
        cmd.stdout(log).stderr(log2).stdin(std::process::Stdio::null()).current_dir(&work);
        #[cfg(windows)]
        {
            // Own process group (no Ctrl+C from the plane's console) and a
            // hidden console that git and other children inherit. The runner
            // survives the plane process being killed.
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        let child = cmd.spawn().with_context(|| format!("spawning {}", self.supervisor_bin.display()))?;
        let pid = child.id().unwrap_or(0);
        std::fs::write(&pid_file, pid.to_string())?;
        // Reap in the background so we leave no zombies.
        tokio::spawn(async move {
            let mut child = child;
            let _ = child.wait().await;
        });
        Ok(format!("pid:{pid}:{}", spec.name))
    }

    async fn remove(&self, handle: &str) -> Result<()> {
        let Some((pid, name)) = parse_handle(handle) else { return Ok(()) };
        if pid_alive(pid) {
            kill_pid(pid);
        }
        let dir = self.root.join(&name);
        for _ in 0..10 {
            if !dir.exists() || std::fs::remove_dir_all(&dir).is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        Ok(())
    }

    async fn alive(&self, handle: &str) -> Result<Option<bool>> {
        Ok(parse_handle(handle).map(|(pid, _)| pid_alive(pid)))
    }
}
