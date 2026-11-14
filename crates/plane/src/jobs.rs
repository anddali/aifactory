//! Background jobs on `apalis` with the Postgres backend.
//!
//! Processing is at-least-once. Every handler is idempotent: it re-checks the
//! database state it acts on, and external side effects are recorded in
//! `side_effects` with markers so a retry finds the earlier effect.

use crate::AppState;
use crate::connectors::{ChatEvent, RepoEvent, TicketEvent};
use anyhow::Result;
use apalis::prelude::*;
use apalis_postgres::{Config, PostgresStorage};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

pub const QUEUE: &str = "factory";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Job {
    StartRun { run_id: Uuid },
    Launch { run_id: Uuid, generation: i64 },
    Teardown { run_id: Uuid, reason: String },
    /// Post to the run's chat thread exactly once per key.
    ChatPost {
        run_id: Uuid,
        key: String,
        text: String,
        #[serde(default)]
        question_id: Option<Uuid>,
        #[serde(default)]
        ops: bool,
    },
    /// Update a question's chat message after it was answered.
    ChatUpdate { question_id: Uuid, text: String },
    /// Comment on the run's ticket (and PR) exactly once per key.
    ExternalComment { run_id: Uuid, key: String, text: String },
    RepoEvent { connector_id: Uuid, event: RepoEvent },
    TicketEvent { connector_id: Uuid, event: TicketEvent },
    ChatEvent { connector_id: Uuid, event: ChatEvent },
    /// Final usage reconciliation and model key clean-up at run end.
    CloseModelAccess { run_id: Uuid },
}

impl Job {
    pub fn kind(&self) -> &'static str {
        match self {
            Job::StartRun { .. } => "start_run",
            Job::Launch { .. } => "launch",
            Job::Teardown { .. } => "teardown",
            Job::ChatPost { .. } => "chat_post",
            Job::ChatUpdate { .. } => "chat_update",
            Job::ExternalComment { .. } => "external_comment",
            Job::RepoEvent { .. } => "repo_event",
            Job::TicketEvent { .. } => "ticket_event",
            Job::ChatEvent { .. } => "chat_event",
            Job::CloseModelAccess { .. } => "close_model_access",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub job: Job,
    #[serde(default)]
    pub attempt: u32,
}

#[derive(Clone)]
pub struct Queue {
    storage: PostgresStorage<Envelope>,
}

pub const MAX_ATTEMPTS: u32 = 8;

impl Queue {
    pub async fn new(pool: &sqlx::PgPool) -> Result<Self> {
        let cfg = Config::default()
            .queue(QUEUE)
            .batch_size(8)
            .heartbeat_interval(Duration::from_secs(5))
            .missed_heartbeats(4);
        Ok(Queue { storage: PostgresStorage::new(pool).with_config(cfg) })
    }

    /// Enqueue a job. With a key, enqueuing the same key twice enqueues once.
    pub async fn push(&self, job: Job, key: Option<String>) -> Result<()> {
        self.push_env(Envelope { job, attempt: 0 }, key, None).await
    }

    pub async fn push_delayed(&self, job: Job, delay: Duration) -> Result<()> {
        self.push_env(Envelope { job, attempt: 0 }, None, Some(delay)).await
    }

    async fn push_env(&self, env: Envelope, key: Option<String>, delay: Option<Duration>) -> Result<()> {
        let mut b = TaskBuilder::new(env);
        if let Some(k) = &key {
            b = b.idempotency_key(k);
        }
        if let Some(d) = delay {
            b = b.run_after(d);
        }
        let mut s = self.storage.clone();
        match s.push_task(b.build()).await {
            Ok(_) => Ok(()),
            // A duplicate idempotency key means the job is already queued.
            Err(e) if key.is_some() => {
                tracing::debug!("job push with key {key:?} not inserted: {e}");
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!("job push failed: {e}")),
        }
    }

    fn worker_backend(&self) -> PostgresStorage<Envelope> {
        self.storage.clone()
    }
}

async fn handle(env: Envelope, state: Data<AppState>) -> Result<(), BoxDynError> {
    let state: AppState = (*state).clone();
    let kind = env.job.kind();
    match crate::engine::run_job(&state, env.job.clone()).await {
        Ok(()) => {
            crate::metrics::JOBS.with_label_values(&[kind, "ok"]).inc();
            Ok(())
        }
        Err(e) => {
            crate::metrics::JOBS.with_label_values(&[kind, "error"]).inc();
            let attempt = env.attempt + 1;
            if attempt >= MAX_ATTEMPTS {
                tracing::error!(job = kind, "job failed permanently: {e:#}");
                return Ok(());
            }
            tracing::warn!(job = kind, attempt, "job failed, retrying: {e:#}");
            let delay = Duration::from_millis(500 * 2u64.pow(attempt.min(6)));
            state
                .jobs
                .push_env(Envelope { job: env.job, attempt }, None, Some(delay))
                .await
                .map_err(|e| -> BoxDynError { e.into() })?;
            Ok(())
        }
    }
}

pub fn spawn_workers(state: AppState) {
    for i in 0..4 {
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                let backend = st.jobs.worker_backend().with_pubsub();
                let worker = WorkerBuilder::new(format!("{}-w{i}", st.cfg.replica_id))
                    .backend(backend)
                    .data(st.clone())
                    .build(handle);
                if let Err(e) = worker.run().await {
                    tracing::error!("job worker stopped: {e}");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }
}
