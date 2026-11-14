//! End-to-end test harness: real plane processes (one or more replicas),
//! real supervisor processes launched by the process launcher, a fresh
//! Postgres database per test, and fake connectors. No external credentials.

#![allow(dead_code)]

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Once;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const KEY_B64: &str = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=";
pub const ADMIN: &str = "admin@example.test";

pub fn pg_admin_url() -> String {
    std::env::var("FACTORY_TEST_PG").unwrap_or_else(|_| "postgres://factory:factory@localhost:55432/factory".into())
}

pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

pub fn bin(name: &str) -> PathBuf {
    let dir = std::env::var("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|_| workspace_root().join("target"));
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    dir.join("debug").join(exe)
}

static BUILD: Once = Once::new();

pub fn build_bins() {
    BUILD.call_once(|| {
        let st = Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "factory-plane", "-p", "factory-supervisor", "-p", "factory-cli"])
            .current_dir(workspace_root())
            .status()
            .expect("cargo build");
        assert!(st.success(), "building binaries failed");
    });
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

pub struct Replica {
    pub port: u16,
    pub child: Option<Child>,
    pub data_dir: PathBuf,
    pub env: Vec<(String, String)>,
}

impl Replica {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    pub fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
    pub fn start(&mut self) {
        let log = std::fs::File::create(self.data_dir.join(format!("plane-{}.log", Uuid::new_v4().simple()))).unwrap();
        let mut cmd = Command::new(bin("plane"));
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.stdout(log.try_clone().unwrap()).stderr(log).stdin(Stdio::null());
        self.child = Some(cmd.spawn().expect("start plane"));
    }
}

impl Drop for Replica {
    fn drop(&mut self) {
        self.kill();
    }
}

pub struct Env {
    pub db_url: String,
    pub db_name: String,
    pub replicas: Vec<Replica>,
    pub root: tempfile::TempDir,
    pub http: reqwest::Client,
    pub session: String,
    pub pool: sqlx::PgPool,
    pub repo_root: PathBuf,
    pub tickets: Uuid,
    pub repo_conn: Uuid,
    pub chat: Uuid,
    pub project: Uuid,
    pub repo_name: String,
}

pub struct Opts {
    /// Launch runners as Docker containers instead of local processes.
    pub docker: bool,
    pub replicas: usize,
    pub fault: Option<&'static str>,
    pub project_config: Value,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { docker: false, replicas: 1, fault: None, project_config: json!({}) }
    }
}

impl Env {
    pub async fn new(opts: Opts) -> Env {
        build_bins();
        let admin = sqlx::PgPool::connect(&pg_admin_url()).await.expect("Postgres for tests (docker compose up -d postgres)");
        let db_name = format!("t_{}", Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {db_name}"))).execute(&admin).await.unwrap();
        admin.close().await;
        let base = pg_admin_url();
        let db_url = format!("{}/{db_name}", base.rsplit_once('/').unwrap().0);
        let root = tempfile::tempdir().unwrap();
        let repo_root = root.path().join("fake-repos");
        std::fs::create_dir_all(&repo_root).unwrap();
        let ports: Vec<u16> = (0..opts.replicas).map(|_| free_port()).collect();
        let host = if opts.docker { "host.docker.internal" } else { "127.0.0.1" };
        let public = ports.iter().map(|p| format!("http://{host}:{p}")).collect::<Vec<_>>().join(",");
        let listen_host = if opts.docker { "0.0.0.0" } else { "127.0.0.1" };
        let mut replicas = vec![];
        for (i, port) in ports.iter().enumerate() {
            let data_dir = root.path().join(format!("plane{i}"));
            std::fs::create_dir_all(&data_dir).unwrap();
            let mut env = vec![
                ("DATABASE_URL".into(), db_url.clone()),
                ("FACTORY_ENCRYPTION_KEYS".into(), format!("k1:{KEY_B64}")),
                ("FACTORY_LISTEN_ADDR".into(), format!("{listen_host}:{port}")),
                ("FACTORY_PUBLIC_URLS".into(), public.clone()),
                ("FACTORY_DATA_DIR".into(), data_dir.to_string_lossy().to_string()),
                ("FACTORY_DEV_LOGIN".into(), "1".into()),
                ("FACTORY_BOOTSTRAP_ADMIN".into(), ADMIN.into()),
                ("FACTORY_REPLICA_ID".into(), format!("replica-{i}")),
                ("FACTORY_SUPERVISOR_BIN".into(), bin("supervisor").to_string_lossy().to_string()),
                ("RUST_LOG".into(), "info,sqlx=warn".into()),
            ];
            // Entra sign-in against the plane's own fake Entra.
            for (k, v) in [
                ("FACTORY_ENTRA_TENANT_ID", "t1".to_string()),
                ("FACTORY_ENTRA_CLIENT_ID", "factory-client".to_string()),
                ("FACTORY_ENTRA_CLIENT_SECRET", "fake-entra-secret".to_string()),
                ("FACTORY_ENTRA_AUTHORITY", format!("http://127.0.0.1:{port}/fake-entra/t1")),
                ("FACTORY_ENTRA_REDIRECT_URL", format!("http://127.0.0.1:{port}/auth/entra/callback")),
            ] {
                env.push((k.into(), v));
            }
            if let Some(c) = local_copilot() {
                env.push(("FACTORY_COPILOT_BIN".into(), c));
            }
            if let Some(f) = opts.fault {
                env.push(("FACTORY_TEST_RUNNER_FAULT".into(), f.into()));
            }
            let mut r = Replica { port: *port, child: None, data_dir, env };
            r.start();
            replicas.push(r);
        }
        let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build().unwrap();
        for r in &replicas {
            wait_http(&http, &format!("{}/readyz", r.url())).await;
        }
        let pool = sqlx::PgPool::connect(&db_url).await.unwrap();
        let mut env = Env {
            db_url,
            db_name,
            replicas,
            root,
            http,
            session: String::new(),
            pool,
            repo_root,
            tickets: Uuid::nil(),
            repo_conn: Uuid::nil(),
            chat: Uuid::nil(),
            project: Uuid::nil(),
            repo_name: "acme/widgets".into(),
        };
        env.session = env.login(ADMIN).await;
        env.setup(opts.project_config).await;
        if opts.docker {
            env.put("/api/settings", json!({"launcher": "docker", "heartbeat_secs": 2})).await;
        }
        env
    }

    pub fn url(&self) -> String {
        // First live replica.
        self.replicas.iter().find(|r| r.child.is_some()).map(|r| r.url()).expect("a live replica")
    }

    pub async fn login(&self, email: &str) -> String {
        let r = self.http.post(format!("{}/auth/dev-login", self.url())).json(&json!({"email": email})).send().await.unwrap();
        assert!(r.status().is_success(), "login failed: {}", r.status());
        r.headers().get("x-factory-session").unwrap().to_str().unwrap().to_string()
    }

    pub async fn req(&self, method: reqwest::Method, path: &str, body: Option<Value>, session: &str) -> (u16, Value) {
        let mut last = None;
        for _ in 0..20 {
            let mut rb = self.http.request(method.clone(), format!("{}{path}", self.url())).header("x-factory-session", session);
            if let Some(b) = &body {
                rb = rb.json(b);
            }
            match rb.send().await {
                Ok(r) => {
                    let s = r.status().as_u16();
                    let t = r.text().await.unwrap_or_default();
                    return (s, serde_json::from_str(&t).unwrap_or(json!({"raw": t})));
                }
                Err(e) => {
                    last = Some(e);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
        panic!("request {path} failed: {last:?}");
    }

    pub async fn get(&self, path: &str) -> Value {
        let (s, v) = self.req(reqwest::Method::GET, path, None, &self.session.clone()).await;
        assert!(s < 300, "GET {path} -> {s}: {v}");
        v
    }
    pub async fn post(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.req(reqwest::Method::POST, path, Some(body), &self.session.clone()).await;
        assert!(s < 300, "POST {path} -> {s}: {v}");
        v
    }
    pub async fn post_status(&self, path: &str, body: Value) -> (u16, Value) {
        self.req(reqwest::Method::POST, path, Some(body), &self.session.clone()).await
    }
    pub async fn put(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.req(reqwest::Method::PUT, path, Some(body), &self.session.clone()).await;
        assert!(s < 300, "PUT {path} -> {s}: {v}");
        v
    }

    async fn setup(&mut self, project_config: Value) {
        self.put("/api/settings", json!({"tick_ms": 300, "heartbeat_secs": 1, "poll_secs": 1, "launch_timeout_secs": 60})).await;
        self.tickets = self.connector("fake_tickets", json!({})).await;
        self.repo_conn = self.connector("fake_repo", json!({"root": self.repo_root.to_string_lossy()})).await;
        self.chat = self.connector("fake_chat", json!({})).await;
        self.post(&format!("/api/fake/{}/repos", self.repo_conn), json!({"name": self.repo_name, "files": {
            "README.md": "# Widgets\n", "Cargo.toml": "[package]\nname = \"widgets\"\nversion = \"0.1.0\"\n", "src/lib.rs": "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
            "src/config.rs": "const password = \"hunter2hunter2\";\n"
        }})).await;
        self.post(&format!("/api/fake/{}/repos", self.repo_conn), json!({"name": "acme/other", "files": {"README.md": "other\n"}})).await;
        let mut cfg = json!({
            "ticket_connector_id": self.tickets, "chat_connector_id": self.chat, "chat_channel": "#factory",
            "idle_grace_secs": 30, "max_turns": 30, "max_concurrent_runs": 20,
        });
        plane::settings::merge_json(&mut cfg, &project_config);
        let p = self.post("/api/projects", json!({"key": "PROJ", "name": "Project", "config": cfg})).await;
        self.project = p["id"].as_str().unwrap().parse().unwrap();
        let r = self.post(&format!("/api/projects/{}/repos", self.project), json!({"connector_id": self.repo_conn, "full_name": self.repo_name})).await;
        assert_eq!(r["stack"], "rust", "stack detection");
        // People: alice is the assignee and approver, bob only views.
        for (email, slack, role) in [("alice@example.test", "U-alice", "approve"), ("bob@example.test", "U-bob", "view"), ("olly@example.test", "U-olly", "operate")] {
            let u = self.post("/api/users", json!({"email": email})).await;
            let id = u["id"].as_str().unwrap().to_string();
            self.put(&format!("/api/users/{id}/identities"), json!({"identities": [{"provider": "slack", "external_id": slack}]})).await;
            self.put(&format!("/api/users/{id}/grants"), json!({"grants": [{"project_id": self.project, "role": role}]})).await;
        }
    }

    pub async fn connector(&self, kind: &str, config: Value) -> Uuid {
        let c = self.post("/api/connectors", json!({"kind": kind, "name": kind, "config": config})).await;
        c["id"].as_str().unwrap().parse().unwrap()
    }

    pub async fn ticket(&self, key: &str, title: &str) {
        self.post(&format!("/api/fake/{}/tickets", self.tickets), json!({"key": key, "title": title, "description": format!("Please do {title}"), "status": "To Do", "assignee_email": "alice@example.test", "repo": self.repo_name})).await;
    }

    pub async fn start_ticket_run(&self, key: &str) -> Uuid {
        let r = self.post("/api/runs", json!({"project_id": self.project, "workflow": "ticket_build", "ticket_key": key})).await;
        r["id"].as_str().unwrap().parse().unwrap()
    }

    pub async fn run(&self, id: Uuid) -> Value {
        self.get(&format!("/api/runs/{id}")).await
    }

    pub async fn wait_run<F: Fn(&Value) -> bool>(&self, id: Uuid, what: &str, secs: u64, f: F) -> Value {
        let start = Instant::now();
        let mut last = json!(null);
        while start.elapsed() < Duration::from_secs(secs) {
            let (s, v) = self.req(reqwest::Method::GET, &format!("/api/runs/{id}"), None, &self.session.clone()).await;
            if s == 200 {
                if f(&v) {
                    return v;
                }
                last = v;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let evs = self.get(&format!("/api/runs/{id}/events?limit=60")).await;
        let kinds: Vec<String> = evs["events"].as_array().unwrap().iter().map(|e| format!("{} {}", e["kind"].as_str().unwrap(), e["payload"].to_string().chars().take(200).collect::<String>())).collect();
        panic!("timed out waiting for {what}. last run: {last}\nevents:\n{}", kinds.join("\n"));
    }

    pub async fn wait_state(&self, id: Uuid, state: &str, waiting_for: Option<&str>, secs: u64) -> Value {
        let w = waiting_for.map(String::from);
        self.wait_run(id, &format!("{state}/{waiting_for:?}"), secs, move |v| v["state"] == state && (w.is_none() || v["waiting_for"].as_str() == w.as_deref())).await
    }

    pub async fn chat_messages(&self) -> Vec<Value> {
        self.get(&format!("/api/fake/{}/chat/messages", self.chat)).await["messages"].as_array().unwrap().clone()
    }

    pub async fn wait_chat(&self, needle: &str, secs: u64) -> Vec<Value> {
        let start = Instant::now();
        loop {
            let m = self.chat_messages().await;
            if m.iter().any(|x| x["text"].as_str().unwrap_or("").contains(needle)) {
                return m;
            }
            assert!(start.elapsed() < Duration::from_secs(secs), "no chat message containing {needle:?}: {m:?}");
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    pub async fn thread_of(&self, id: Uuid) -> String {
        let r = self.run(id).await;
        r["chat_thread"]["ts"].as_str().expect("chat thread").to_string()
    }

    pub async fn reply(&self, id: Uuid, user: &str, text: &str) {
        let ts = self.thread_of(id).await;
        self.post(&format!("/api/fake/{}/chat/reply", self.chat), json!({"channel": "#factory", "thread_ts": ts, "user": user, "text": text})).await;
    }

    pub async fn pr_action(&self, body: Value) -> Value {
        let mut b = body;
        b["repo"] = json!(self.repo_name);
        self.post(&format!("/api/fake/{}/pr-action", self.repo_conn), b).await
    }

    pub async fn pr(&self, n: i64) -> Value {
        self.post(&format!("/api/fake/{}/pr", self.repo_conn), json!({"repo": self.repo_name, "number": n})).await
    }

    pub async fn events(&self, id: Uuid) -> Vec<Value> {
        self.get(&format!("/api/runs/{id}/events?limit=2000")).await["events"].as_array().unwrap().clone()
    }

    pub async fn count_events(&self, id: Uuid, kind: &str) -> usize {
        self.events(id).await.iter().filter(|e| e["kind"] == kind).count()
    }

    /// The bare upstream repository of the fake repo host.
    pub fn upstream(&self) -> PathBuf {
        self.repo_root.join(format!("{}.git", self.repo_name))
    }

    pub fn git_upstream(&self, args: &[&str]) -> String {
        let o = Command::new("git").arg("-C").arg(self.upstream()).args(args).output().unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// The current run token, derived exactly as the plane derives it.
    pub async fn run_token(&self, id: Uuid) -> String {
        let (salt, generation): (String, i64) = sqlx::query_as("SELECT token_salt, runner_generation FROM runs WHERE id=$1").bind(id).fetch_one(&self.pool).await.unwrap();
        let c = plane::crypto::Crypto::from_spec(&format!("k1:{KEY_B64}")).unwrap();
        format!("frt_{}", c.derive(&format!("run|{id}|{generation}|{salt}")))
    }

    pub async fn runner_pid(&self, id: Uuid) -> Option<u32> {
        let r = self.run(id).await;
        let h = r["runner"]["handle"].as_str()?;
        let inner = h.split_once('|')?.1;
        inner.strip_prefix("pid:")?.split(':').next()?.parse().ok()
    }

    pub async fn tick(&self) {
        self.post("/api/_tick", json!({})).await;
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Kill runners first, then the planes.
        if let Ok(rd) = std::fs::read_dir(self.root.path()) {
            for d in rd.flatten() {
                let runners = d.path().join("runners");
                if let Ok(rs) = std::fs::read_dir(runners) {
                    for r in rs.flatten() {
                        if let Ok(p) = std::fs::read_to_string(r.path().join("supervisor.pid")) {
                            if let Ok(pid) = p.trim().parse::<u32>() {
                                kill_pid(pid);
                            }
                        }
                    }
                }
            }
        }
        for r in &mut self.replicas {
            r.kill();
        }
        if std::env::var("FACTORY_TEST_KEEP").is_ok() {
            let p = self.root.path().to_path_buf();
            eprintln!("keeping test data at {}", p.display());
            std::mem::forget(std::mem::replace(&mut self.root, tempfile::tempdir().unwrap()));
        }
    }
}

pub fn kill_pid(pid: u32) {
    #[cfg(windows)]
    let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
    #[cfg(not(windows))]
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
}

pub async fn wait_http(http: &reqwest::Client, url: &str) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        if let Ok(r) = http.get(url).send().await {
            if r.status().is_success() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("{url} never became ready");
}

/// The Copilot CLI to use in tests: FACTORY_COPILOT_BIN, else the newest
/// locally auto-updated package under ~/.copilot/pkg, else None (PATH).
pub fn local_copilot() -> Option<String> {
    if let Ok(b) = std::env::var("FACTORY_COPILOT_BIN") {
        return Some(b);
    }
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).ok()?;
    let dir = std::path::Path::new(&home).join(".copilot").join("pkg").join("universal");
    let mut versions: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.join("index.js").exists()).collect();
    versions.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().split('.').map(|x| x.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>()).unwrap_or_default());
    versions.last().map(|p| p.join("index.js").to_string_lossy().into_owned())
}
