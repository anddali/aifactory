//! Start-up configuration. Only what the plane needs to start comes from the
//! environment; everything else lives in the database and is edited in the portal.

use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    /// Comma-separated `kid:base64key` pairs. The first is used to encrypt.
    pub encryption_keys: String,
    pub listen_addr: String,
    /// Email of the bootstrap global administrator.
    pub bootstrap_admin: Option<String>,
    /// URLs runners use to reach the plane (comma separated, several for failover).
    pub public_urls: Vec<String>,
    /// Local working directory for git mirrors and process runners.
    pub data_dir: PathBuf,
    pub dev_login: bool,
    pub portal_dir: Option<PathBuf>,
    /// Name of this replica, for logs and job workers.
    pub replica_id: String,
    pub entra: Option<EntraConfig>,
}

#[derive(Clone, Debug)]
pub struct EntraConfig {
    pub tenant_id: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
    /// Authority base URL, overridable for tests.
    pub authority: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let listen_addr = get("FACTORY_LISTEN_ADDR").unwrap_or_else(|| "0.0.0.0:8080".into());
        let port = listen_addr.rsplit(':').next().unwrap_or("8080").to_string();
        let entra = match (get("FACTORY_ENTRA_TENANT_ID"), get("FACTORY_ENTRA_CLIENT_ID"), get("FACTORY_ENTRA_CLIENT_SECRET")) {
            (Some(t), Some(c), Some(s)) => Some(EntraConfig {
                authority: get("FACTORY_ENTRA_AUTHORITY")
                    .unwrap_or_else(|| format!("https://login.microsoftonline.com/{t}")),
                tenant_id: t,
                client_id: c,
                client_secret: s,
                redirect_url: get("FACTORY_ENTRA_REDIRECT_URL")
                    .unwrap_or_else(|| format!("http://localhost:{port}/auth/entra/callback")),
            }),
            _ => None,
        };
        Ok(Config {
            database_url: get("DATABASE_URL").context("DATABASE_URL is required")?,
            encryption_keys: get("FACTORY_ENCRYPTION_KEYS")
                .or_else(|| get("FACTORY_ENCRYPTION_KEY").map(|k| format!("k1:{k}")))
                .context("FACTORY_ENCRYPTION_KEYS (or FACTORY_ENCRYPTION_KEY) is required")?,
            public_urls: get("FACTORY_PUBLIC_URLS")
                .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_else(|| vec![format!("http://127.0.0.1:{port}")]),
            listen_addr,
            bootstrap_admin: get("FACTORY_BOOTSTRAP_ADMIN"),
            data_dir: get("FACTORY_DATA_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".factory-data")),
            dev_login: matches!(get("FACTORY_DEV_LOGIN").as_deref(), Some("1" | "true" | "yes")),
            portal_dir: get("FACTORY_PORTAL_DIR").map(PathBuf::from),
            replica_id: get("FACTORY_REPLICA_ID").unwrap_or_else(|| format!("plane-{}", &uuid::Uuid::new_v4().simple().to_string()[..6])),
            entra,
        })
    }
}
