//! The runner's workspace: clone through the plane git endpoint, checkpoint
//! work in progress to the run-owned WIP branch, and restore from it.

use anyhow::{Context, Result, bail};
use client::PlaneClient;
use proto::RunSpec;
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub struct Workspace {
    pub root: PathBuf,
    pub repo: PathBuf,
    pub session: PathBuf,
    pub client: PlaneClient,
    pub spec: RunSpec,
}

pub struct GitOut {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Workspace {
    pub fn new(root: PathBuf, client: PlaneClient, spec: RunSpec) -> Self {
        Workspace { repo: root.join("repo"), session: root.join("session"), root, client, spec }
    }

    fn remote_url(&self) -> Option<String> {
        self.spec.clone_url.as_ref().map(|p| format!("{}{}", self.client.base_url(), p))
    }

    /// Run git with the run token as an HTTP header. The token is passed via
    /// environment-only git configuration, never written to disk.
    pub async fn git_in(&self, dir: &Path, args: &[&str], extra_env: &[(&str, String)]) -> Result<GitOut> {
        let mut c = Command::new("git");
        c.current_dir(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GCM_INTERACTIVE", "never")
            .env("GIT_CONFIG_COUNT", "5")
            .env("GIT_CONFIG_KEY_4", "credential.helper")
            .env("GIT_CONFIG_VALUE_4", "")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", format!("Authorization: Bearer {}", self.client.token()))
            .env("GIT_CONFIG_KEY_1", "user.name")
            .env("GIT_CONFIG_VALUE_1", "factory-bot")
            .env("GIT_CONFIG_KEY_2", "user.email")
            .env("GIT_CONFIG_VALUE_2", "factory-bot@factory.invalid")
            .env("GIT_CONFIG_KEY_3", "core.autocrlf")
            .env("GIT_CONFIG_VALUE_3", "false")
            .kill_on_drop(true);
        for (k, v) in extra_env {
            c.env(k, v);
        }
        let o = c.output().await.context("running git")?;
        Ok(GitOut {
            status: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&o.stderr).trim().to_string(),
        })
    }

    pub async fn git(&self, args: &[&str]) -> Result<String> {
        self.git_env(args, &[]).await
    }

    pub async fn git_env(&self, args: &[&str], env: &[(&str, String)]) -> Result<String> {
        let o = self.git_in(&self.repo, args, env).await?;
        if o.status != 0 {
            bail!("git {} failed (exit {}): {} {}", args.join(" "), o.status, o.stderr, o.stdout);
        }
        Ok(o.stdout)
    }

    /// Point origin at the current plane URL (after failover).
    pub async fn sync_remote(&self) -> Result<()> {
        if let Some(u) = self.remote_url() {
            self.git(&["remote", "set-url", "origin", &u]).await?;
        }
        Ok(())
    }

    /// Git network operations, retried across plane replicas.
    pub async fn git_net(&self, args: &[&str]) -> Result<String> {
        let mut last = None;
        for attempt in 0..8u64 {
            self.sync_remote().await?;
            let o = self.git_in(&self.repo, args, &[]).await?;
            if o.status == 0 {
                return Ok(o.stdout);
            }
            // Policy refusals are final.
            if o.stderr.contains("403") || o.stderr.contains("refused by policy") || o.stderr.contains("non-fast-forward") || o.stderr.contains("rejected") {
                bail!("git {} refused: {}", args.join(" "), o.stderr);
            }
            last = Some(o.stderr);
            tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt + 1))).await;
            // Probe so the client fails over to another replica if needed.
            let _ = self.client.heartbeat(&proto::Heartbeat { generation: -1, busy: false, current_input: None }).await;
        }
        bail!("git {} failed: {}", args.join(" "), last.unwrap_or_default())
    }

    /// Clone (or reuse) the repository and restore the latest checkpoint.
    pub async fn setup(&self) -> Result<String> {
        std::fs::create_dir_all(&self.session)?;
        let Some(url) = self.remote_url() else {
            std::fs::create_dir_all(&self.repo)?;
            return Ok("no repository".into());
        };
        if !self.repo.join(".git").exists() {
            let mut ok = false;
            let mut err = String::new();
            for attempt in 0..8u64 {
                let url = self.remote_url().unwrap_or(url.clone());
                let o = self.git_in(&self.root, &["clone", "-q", &url, "repo"], &[]).await?;
                if o.status == 0 {
                    ok = true;
                    break;
                }
                err = o.stderr;
                let _ = std::fs::remove_dir_all(&self.repo);
                tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt + 1))).await;
                let _ = self.client.heartbeat(&proto::Heartbeat { generation: -1, busy: false, current_input: None }).await;
            }
            if !ok {
                bail!("clone failed: {err}");
            }
        }
        let base = self.spec.base_branch.clone().unwrap_or_else(|| "main".into());
        let Some(branch) = self.spec.branch.clone() else {
            return Ok(format!("read-only checkout of {base}"));
        };
        // Restore from the WIP checkpoint if there is one.
        if let (Some(wip), Some(_)) = (&self.spec.wip_ref, &self.spec.checkpoint_commit) {
            let fetched = self.git_net(&["fetch", "-q", "origin", &format!("+{wip}:refs/remotes/origin/wip")]).await;
            if fetched.is_ok() {
                self.git(&["checkout", "-q", "-B", &branch, "refs/remotes/origin/wip^1"]).await?;
                self.git(&["read-tree", "-u", "--reset", "refs/remotes/origin/wip"]).await?;
                self.git(&["reset", "-q"]).await?;
                return Ok(format!("restored checkpoint {}", self.spec.checkpoint_commit.clone().unwrap_or_default()));
            }
        }
        let remote_branch = self.git_net(&["ls-remote", "--heads", "origin", &branch]).await.unwrap_or_default();
        if !remote_branch.is_empty() {
            self.git_net(&["fetch", "-q", "origin", &branch]).await?;
            self.git(&["checkout", "-q", "-B", &branch, "FETCH_HEAD"]).await?;
            Ok(format!("checked out existing {branch}"))
        } else {
            self.git(&["checkout", "-q", "-B", &branch, &format!("origin/{base}")]).await?;
            Ok(format!("new branch {branch} from {base}"))
        }
    }

    /// Commit everything (tracked and untracked) as a WIP commit whose parents
    /// are HEAD and the previous WIP tip, and push it to the WIP branch. The
    /// working tree and index are left untouched. Returns the WIP commit.
    pub async fn checkpoint_git(&self) -> Result<Option<String>> {
        let (Some(wip), Some(_)) = (&self.spec.wip_ref, &self.spec.branch) else { return Ok(None) };
        if !self.repo.join(".git").exists() {
            return Ok(None);
        }
        let index = self.root.join("wip-index");
        let _ = std::fs::remove_file(&index);
        let env = [("GIT_INDEX_FILE", index.to_string_lossy().to_string())];
        self.git_env(&["read-tree", "HEAD"], &env).await?;
        self.git_env(&["add", "-A"], &env).await?;
        let tree = self.git_env(&["write-tree"], &env).await?;
        let head = self.git(&["rev-parse", "HEAD"]).await?;
        let _ = std::fs::remove_file(&index);
        let prev = self.git_net(&["ls-remote", "origin", wip]).await.unwrap_or_default();
        let prev = prev.split_whitespace().next().map(String::from);
        if let Some(p) = &prev {
            // Nothing changed since the last checkpoint.
            self.git_net(&["fetch", "-q", "origin", &format!("+{wip}:refs/remotes/origin/wip")]).await?;
            let ptree = self.git(&["rev-parse", &format!("{p}^{{tree}}")]).await.unwrap_or_default();
            let pparent = self.git(&["rev-parse", &format!("{p}^1")]).await.unwrap_or_default();
            if ptree == tree && pparent == head {
                return Ok(Some(p.clone()));
            }
        }
        let mut args = vec!["commit-tree", tree.as_str(), "-p", head.as_str()];
        if let Some(p) = &prev {
            if p != &head {
                args.push("-p");
                args.push(p.as_str());
            }
        }
        args.push("-m");
        args.push("factory checkpoint");
        let commit = self.git(&args).await?;
        self.git_net(&["push", "-q", "origin", &format!("{commit}:{wip}")]).await?;
        Ok(Some(commit))
    }

    /// Archive the harness session directory.
    pub fn export_session(&self, files: &[PathBuf]) -> Result<Vec<u8>> {
        let mut buf = vec![];
        {
            let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::fast());
            let mut tar = tar::Builder::new(enc);
            for f in files {
                let p = self.session.join(f);
                if p.is_dir() {
                    tar.append_dir_all(f, &p)?;
                } else if p.is_file() {
                    tar.append_path_with_name(&p, f)?;
                }
            }
            tar.into_inner()?.finish()?;
        }
        Ok(buf)
    }

    pub fn import_session(&self, data: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.session)?;
        let dec = flate2::read::GzDecoder::new(data);
        let mut ar = tar::Archive::new(dec);
        ar.unpack(&self.session)?;
        Ok(())
    }
}
