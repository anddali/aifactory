//! Thin async wrapper around the git command line.

use anyhow::{Result, bail};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub struct Out {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Out {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

fn base(dir: Option<&Path>) -> Command {
    let mut c = Command::new("git");
    if let Some(d) = dir {
        c.arg("-C").arg(d);
    }
    c.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .kill_on_drop(true);
    c
}

/// Run git; fails on a non-zero exit.
pub async fn git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let o = git_raw(dir, args, None, &[]).await?;
    if o.status != 0 {
        bail!("git {} failed ({}): {}", args.join(" "), o.status, o.stderr.trim());
    }
    Ok(o.text())
}

/// Run git with optional stdin and extra `-c` config; never fails on exit status.
pub async fn git_raw(dir: Option<&Path>, args: &[&str], stdin: Option<&[u8]>, config: &[String]) -> Result<Out> {
    let mut c = base(dir);
    for kv in config {
        c.arg("-c").arg(kv);
    }
    c.args(args);
    c.stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = c.spawn()?;
    if let Some(input) = stdin {
        let mut si = child.stdin.take().unwrap();
        let data = input.to_vec();
        // Write in a task so a large output cannot deadlock against our write.
        tokio::spawn(async move {
            let _ = si.write_all(&data).await;
            let _ = si.shutdown().await;
        });
    }
    let o = child.wait_with_output().await?;
    Ok(Out {
        status: o.status.code().unwrap_or(-1),
        stdout: o.stdout,
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    })
}

pub const ZERO_SHA: &str = "0000000000000000000000000000000000000000";
