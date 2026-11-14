//! The AI Factory control plane.

pub mod api;
pub mod audit;
pub mod blob;
pub mod config;
pub mod connectors;
pub mod crypto;
pub mod db;
pub mod egress;
pub mod engine;
pub mod entra;
pub mod humans;
pub mod events;
pub mod git;
pub mod gitutil;
pub mod jobs;
pub mod launcher;
pub mod metrics;
pub mod model_access;
pub mod model_endpoint;
pub mod notify;
pub mod primitives;
pub mod redact;
pub mod runs;
pub mod settings;
pub mod singleton;
pub mod stack;
pub mod workflows;

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

pub struct Inner {
    pub pool: sqlx::PgPool,
    pub cfg: config::Config,
    pub crypto: crypto::Crypto,
    pub blobs: Arc<dyn blob::BlobStore>,
    pub hub: notify::Hub,
    pub launchers: HashMap<&'static str, Arc<dyn launcher::Launcher>>,
    pub jobs: jobs::Queue,
    /// Per-repository locks for the local git mirrors on this replica.
    pub git_locks: Mutex<HashMap<Uuid, Arc<Mutex<()>>>>,
    pub http: reqwest::Client,
}

pub type AppState = Arc<Inner>;

impl Inner {
    pub fn launcher(&self, kind: &str) -> Arc<dyn launcher::Launcher> {
        self.launchers
            .get(kind)
            .or_else(|| self.launchers.get("process"))
            .cloned()
            .expect("process launcher always present")
    }
}

/// Build state, run migrations, and start the server, job workers and
/// singleton duties. Returns when the server stops.
pub async fn run(cfg: config::Config) -> Result<()> {
    let state = build(cfg).await?;
    let listener = tokio::net::TcpListener::bind(&state.cfg.listen_addr).await?;
    tracing::info!(addr = %state.cfg.listen_addr, replica = %state.cfg.replica_id, "plane listening");
    start_background(&state);
    let app = api::router(state.clone());
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

pub async fn build(cfg: config::Config) -> Result<AppState> {
    let pool = db::connect(&cfg.database_url).await?;
    db::migrate(&pool).await?;
    let crypto = crypto::Crypto::from_spec(&cfg.encryption_keys)?;
    std::fs::create_dir_all(&cfg.data_dir)?;
    let mut launchers: HashMap<&'static str, Arc<dyn launcher::Launcher>> = HashMap::new();
    launchers.insert("process", Arc::new(launcher::process::ProcessLauncher::new(&cfg.data_dir)));
    launchers.insert("docker", Arc::new(launcher::docker::DockerLauncher::from_env()));
    if let Some(f) = launcher::fargate::FargateLauncher::from_env().await {
        launchers.insert("fargate", Arc::new(f));
    }
    let jobs = jobs::Queue::new(&pool).await?;
    let state = Arc::new(Inner {
        blobs: Arc::new(blob::PgBlobStore { pool: pool.clone() }),
        pool,
        cfg,
        crypto,
        hub: notify::Hub::new(),
        launchers,
        jobs,
        git_locks: Mutex::new(HashMap::new()),
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?,
    });
    db::bootstrap(&state).await?;
    Ok(state)
}

pub fn start_background(state: &AppState) {
    tokio::spawn(state.hub.clone().run(state.pool.clone()));
    jobs::spawn_workers(state.clone());
    tokio::spawn(singleton::run(state.clone()));
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
