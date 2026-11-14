//! Database connection, migrations and bootstrap.

use crate::AppState;
use anyhow::{Context, Result};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

pub async fn connect(url: &str) -> Result<PgPool> {
    let mut last = None;
    for _ in 0..30 {
        match PgPoolOptions::new()
            .max_connections(30)
            .acquire_timeout(Duration::from_secs(10))
            .connect(url)
            .await
        {
            Ok(p) => return Ok(p),
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    Err(last.unwrap()).context("connecting to Postgres")
}

/// Run plane and queue migrations. Safe when several replicas start together:
/// a session advisory lock serialises them, and each migrator records what it
/// applied.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock(7700000)").execute(&mut *conn).await?;
    let r: Result<()> = async {
        sqlx::migrate!("./migrations").run(pool).await.context("plane migrations")?;
        apalis_postgres::PostgresStorage::setup(pool).await.context("queue migrations")?;
        Ok(())
    }
    .await;
    let _ = sqlx::query("SELECT pg_advisory_unlock(7700000)").execute(&mut *conn).await;
    r
}

pub async fn bootstrap(state: &AppState) -> Result<()> {
    if let Some(email) = &state.cfg.bootstrap_admin {
        sqlx::query(
            "INSERT INTO users(id, email, display_name, is_global_admin) VALUES ($1,$2,$3,true)
             ON CONFLICT (email) DO UPDATE SET is_global_admin = true",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(email.to_lowercase())
        .bind(email.split('@').next().unwrap_or(email))
        .execute(&state.pool)
        .await?;
    }
    // Make sure today's event partitions exist.
    let s = crate::settings::get(&state.pool).await?;
    if let Err(e) = crate::events::maintain_partitions(&state.pool, s.event_retention_days).await {
        tracing::warn!("partition maintenance at start-up failed: {e:#}");
    }
    Ok(())
}

pub async fn ready(pool: &PgPool) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations WHERE success")
        .fetch_one(pool)
        .await
        .map(|n| n > 0)
        .unwrap_or(false)
}
