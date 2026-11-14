//! Global settings and per-project configuration, stored in Postgres and
//! edited in the portal.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Price {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub kill_switch: bool,
    pub model_connector_id: Option<Uuid>,
    pub model_prices: HashMap<String, Price>,
    pub egress_allowlist: Vec<String>,
    pub deny_patterns: Vec<String>,
    pub event_retention_days: i64,
    pub tick_ms: u64,
    pub heartbeat_secs: u64,
    pub stuck_repeat_threshold: u32,
    pub stuck_noprogress_turns: u32,
    pub ops_channel: Option<String>,
    pub launcher: String,
    /// Grace before a launching runner that never said hello is relaunched.
    pub launch_timeout_secs: u64,
    pub max_launch_attempts: i32,
    /// Egress proxy URL given to runners, if any.
    pub egress_proxy_url: Option<String>,
    /// Reconciliation polling interval for ticket sources and repo hosts.
    pub poll_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        let mut prices = HashMap::new();
        prices.insert("gpt-4.1".into(), Price { input_per_mtok: 2.0, output_per_mtok: 8.0 });
        prices.insert("gpt-4.1-mini".into(), Price { input_per_mtok: 0.4, output_per_mtok: 1.6 });
        Settings {
            kill_switch: false,
            model_connector_id: None,
            model_prices: prices,
            egress_allowlist: [
                "pypi.org",
                "files.pythonhosted.org",
                "registry.npmjs.org",
                "crates.io",
                "static.crates.io",
                "index.crates.io",
                "api.nuget.org",
                "github.com",
                "objects.githubusercontent.com",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            deny_patterns: vec![
                r"rm\s+-rf\s+/(\s|$)".into(),
                r"mkfs(\.|\s)".into(),
                r":\(\)\s*\{".into(),
                r"dd\s+if=.*of=/dev/".into(),
                r"curl[^|]*\|\s*(ba)?sh".into(),
                r"chmod\s+-R\s+777\s+/(\s|$)".into(),
            ],
            event_retention_days: 90,
            tick_ms: 15000,
            heartbeat_secs: 10,
            stuck_repeat_threshold: 5,
            stuck_noprogress_turns: 4,
            ops_channel: None,
            launcher: "process".into(),
            launch_timeout_secs: 180,
            max_launch_attempts: 5,
            egress_proxy_url: None,
            poll_secs: 60,
        }
    }
}

pub async fn get(pool: &PgPool) -> Result<Settings> {
    let v: Option<Value> = sqlx::query_scalar("SELECT value FROM settings WHERE key='global'")
        .fetch_optional(pool)
        .await?;
    Ok(match v {
        Some(v) => serde_json::from_value(v).unwrap_or_default(),
        None => Settings::default(),
    })
}

/// Merge a partial settings object into the stored settings.
pub async fn merge(pool: &PgPool, patch: &Value) -> Result<Settings> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(7700002)").execute(&mut *tx).await?;
    let cur: Option<Value> = sqlx::query_scalar("SELECT value FROM settings WHERE key='global'")
        .fetch_optional(&mut *tx)
        .await?;
    let mut base = serde_json::to_value(cur.map(|v| serde_json::from_value::<Settings>(v).unwrap_or_default()).unwrap_or_default())?;
    merge_json(&mut base, patch);
    let s: Settings = serde_json::from_value(base)?;
    sqlx::query("INSERT INTO settings(key,value) VALUES ('global',$1) ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value, updated_at=now()")
        .bind(serde_json::to_value(&s)?)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(s)
}

pub fn merge_json(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                match b.get_mut(k) {
                    // Replace maps wholesale for price tables so entries can be removed.
                    Some(existing) if existing.is_object() && v.is_object() && k != "model_prices" => merge_json(existing, v),
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JiraTrigger {
    pub label: Option<String>,
    pub transition: Option<String>,
}

impl Default for JiraTrigger {
    fn default() -> Self {
        JiraTrigger { label: Some("factory".into()), transition: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    pub enabled_workflows: Vec<String>,
    pub ticket_connector_id: Option<Uuid>,
    pub chat_connector_id: Option<Uuid>,
    pub chat_channel: String,
    pub default_model: String,
    pub harness: String,
    pub budget_usd: f64,
    pub max_turns: i32,
    pub turn_timeout_secs: i64,
    pub idle_reminder_secs: i64,
    pub idle_warning_secs: i64,
    pub idle_expiry_secs: i64,
    pub checkpoint_retention_days: i64,
    pub idle_grace_secs: i64,
    pub max_concurrent_runs: i64,
    pub daily_spend_usd: f64,
    pub review_round_limit: i32,
    pub merge_by_agent: bool,
    pub fallback_contact: Option<String>,
    pub ops_channel: Option<String>,
    pub jira_trigger: JiraTrigger,
    pub review_own_prs: bool,
    pub default_assignee_email: Option<String>,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        ProjectConfig {
            enabled_workflows: proto::WorkflowKind::ALL.iter().map(|w| w.as_str().to_string()).collect(),
            ticket_connector_id: None,
            chat_connector_id: None,
            chat_channel: "#factory".into(),
            default_model: "gpt-4.1".into(),
            harness: "scripted".into(),
            budget_usd: 10.0,
            max_turns: 30,
            turn_timeout_secs: 1800,
            idle_reminder_secs: 24 * 3600,
            idle_warning_secs: 48 * 3600,
            idle_expiry_secs: 72 * 3600,
            checkpoint_retention_days: 14,
            idle_grace_secs: 20 * 60,
            max_concurrent_runs: 5,
            daily_spend_usd: 100.0,
            review_round_limit: 5,
            merge_by_agent: false,
            fallback_contact: None,
            ops_channel: None,
            jira_trigger: JiraTrigger::default(),
            review_own_prs: false,
            default_assignee_email: None,
        }
    }
}

pub fn project_config(v: &Value) -> ProjectConfig {
    serde_json::from_value(v.clone()).unwrap_or_default()
}

pub fn project_config_json(v: &Value) -> Value {
    serde_json::to_value(project_config(v)).unwrap_or(json!({}))
}
