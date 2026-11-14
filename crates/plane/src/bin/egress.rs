//! The factory egress proxy binary. Reloads its allowlist and feeds from the
//! database every few seconds, so portal changes apply without a restart.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().json().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let db = std::env::var("DATABASE_URL")?;
    let keys = std::env::var("FACTORY_ENCRYPTION_KEYS").or_else(|_| std::env::var("FACTORY_ENCRYPTION_KEY").map(|k| format!("k1:{k}")))?;
    let listen = std::env::var("FACTORY_EGRESS_LISTEN").unwrap_or_else(|_| "0.0.0.0:3128".into());
    let pool = plane::db::connect(&db).await?;
    let crypto = plane::crypto::Crypto::from_spec(&keys)?;
    let policy = Arc::new(RwLock::new(plane::egress::load_policy(&pool, &crypto).await.unwrap_or_default()));
    {
        let policy = policy.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                match plane::egress::load_policy(&pool, &crypto).await {
                    Ok(p) => *policy.write().await = p,
                    Err(e) => tracing::warn!("reloading egress policy: {e:#}"),
                }
            }
        });
    }
    let l = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!(%listen, "egress proxy listening");
    plane::egress::serve(l, policy).await
}
