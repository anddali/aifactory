//! Postgres LISTEN/NOTIFY fan-out. Each replica holds one listener and
//! rebroadcasts notifications to in-process subscribers (SSE streams and
//! runner long-polls), so live updates reach every replica.

use sqlx::PgPool;
use sqlx::postgres::PgListener;
use std::time::Duration;
use tokio::sync::broadcast;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum Note {
    /// A new event was appended to a run.
    Event { run_id: Uuid, seq: i64 },
    /// New instructions are available for a run.
    Instruction { run_id: Uuid },
    /// Run row changed (state etc.).
    Run { run_id: Uuid },
}

#[derive(Clone)]
pub struct Hub {
    tx: broadcast::Sender<Note>,
}

pub const CH_EVENTS: &str = "factory_events";
pub const CH_INSTR: &str = "factory_instr";
pub const CH_RUN: &str = "factory_run";

impl Hub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(4096);
        Hub { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Note> {
        self.tx.subscribe()
    }

    /// Run the listener loop forever, reconnecting on failure.
    pub async fn run(self, pool: PgPool) {
        loop {
            match self.listen_once(&pool).await {
                Ok(()) => {}
                Err(e) => tracing::warn!("notification listener error: {e:#}"),
            }
            // Wake everyone so they re-check state they might have missed.
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn listen_once(&self, pool: &PgPool) -> anyhow::Result<()> {
        let mut l = PgListener::connect_with(pool).await?;
        l.listen_all([CH_EVENTS, CH_INSTR, CH_RUN]).await?;
        loop {
            let n = l.recv().await?;
            let payload = n.payload();
            let note = match n.channel() {
                CH_EVENTS => {
                    let Some((r, s)) = payload.split_once(':') else { continue };
                    let (Ok(run_id), Ok(seq)) = (r.parse(), s.parse()) else { continue };
                    Note::Event { run_id, seq }
                }
                CH_INSTR => match payload.parse() {
                    Ok(run_id) => Note::Instruction { run_id },
                    Err(_) => continue,
                },
                CH_RUN => match payload.parse() {
                    Ok(run_id) => Note::Run { run_id },
                    Err(_) => continue,
                },
                _ => continue,
            };
            let _ = self.tx.send(note);
        }
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn notify(pool: &PgPool, channel: &str, payload: &str) {
    if let Err(e) = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(channel)
        .bind(payload)
        .execute(pool)
        .await
    {
        tracing::warn!("pg_notify failed: {e}");
    }
}
