use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,apalis=warn,tower_http=warn")))
        .with_current_span(false)
        .init();
    let cfg = plane::config::Config::from_env()?;
    plane::run(cfg).await
}
