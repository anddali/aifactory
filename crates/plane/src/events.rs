//! The append-only run event log: the system of record for a run.

use crate::blob::BlobStore;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// Payloads larger than this are stored once in the blob store and referenced.
pub const INLINE_LIMIT: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Event {
    pub seq: i64,
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub actor: Option<String>,
    pub payload: Value,
    pub blob_ref: Option<String>,
}

/// Append an event. Payload strings are scanned and redacted first.
pub async fn append(
    pool: &PgPool,
    blobs: &dyn BlobStore,
    run_id: Uuid,
    kind: &str,
    actor: Option<&str>,
    mut payload: Value,
) -> Result<i64> {
    let hits = crate::redact::redact_json(&mut payload);
    if !hits.is_empty() {
        if let Value::Object(m) = &mut payload {
            m.insert("_redacted".into(), json!(hits));
        }
    }
    let mut blob_ref = None;
    let serialized = serde_json::to_vec(&payload)?;
    if serialized.len() > INLINE_LIMIT {
        let hash = blobs.put(&serialized).await?;
        let preview: String = String::from_utf8_lossy(&serialized[..2048]).into_owned();
        payload = json!({"_blob": hash, "_size": serialized.len(), "_preview": preview});
        blob_ref = Some(hash);
    }
    let seq: i64 = sqlx::query_scalar(
        "WITH s AS (UPDATE runs SET event_seq = event_seq + 1 WHERE id = $1 RETURNING event_seq),
              ins AS (INSERT INTO events(run_id, seq, kind, actor, payload, blob_ref)
                      SELECT $1, event_seq, $2, $3, $4, $5 FROM s RETURNING run_id, seq)
         SELECT seq FROM ins, LATERAL (SELECT pg_notify('factory_events', ins.run_id::text || ':' || ins.seq::text)) n",
    )
    .bind(run_id)
    .bind(kind)
    .bind(actor)
    .bind(&payload)
    .bind(&blob_ref)
    .fetch_one(pool)
    .await?;
    crate::metrics::EVENTS.with_label_values(&[kind_family(kind)]).inc();
    Ok(seq)
}

fn kind_family(kind: &str) -> &str {
    kind.split('.').next().unwrap_or(kind)
}

pub async fn list(
    pool: &PgPool,
    run_id: Uuid,
    after: Option<i64>,
    before: Option<i64>,
    limit: i64,
) -> Result<Vec<Event>> {
    let limit = limit.clamp(1, 2000);
    let rows: Vec<Event> = if let Some(after) = after {
        sqlx::query_as(
            "SELECT seq, ts, kind, actor, payload, blob_ref FROM events WHERE run_id=$1 AND seq > $2 ORDER BY seq ASC LIMIT $3",
        )
        .bind(run_id)
        .bind(after)
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else {
        let before = before.unwrap_or(i64::MAX);
        let mut v: Vec<Event> = sqlx::query_as(
            "SELECT seq, ts, kind, actor, payload, blob_ref FROM events WHERE run_id=$1 AND seq < $2 ORDER BY seq DESC LIMIT $3",
        )
        .bind(run_id)
        .bind(before)
        .bind(limit)
        .fetch_all(pool)
        .await?;
        v.reverse();
        v
    };
    Ok(rows)
}

pub async fn all(pool: &PgPool, run_id: Uuid) -> Result<Vec<Event>> {
    Ok(sqlx::query_as(
        "SELECT seq, ts, kind, actor, payload, blob_ref FROM events WHERE run_id=$1 ORDER BY seq ASC",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?)
}

/// Create monthly partitions for the current and next month, and drop
/// partitions entirely older than the retention period.
pub async fn maintain_partitions(pool: &PgPool, retention_days: i64) -> Result<()> {
    use chrono::Datelike;
    let now = Utc::now();
    let mut months = vec![];
    let (mut y, mut m) = (now.year(), now.month());
    for _ in 0..3 {
        months.push((y, m));
        m += 1;
        if m > 12 {
            m = 1;
            y += 1;
        }
    }
    for (y, m) in months {
        let name = format!("events_{y:04}{m:02}");
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(&name)
            .fetch_one(pool)
            .await?;
        if exists {
            continue;
        }
        // Rows already sitting in the default partition for this range would
        // block attaching; move them across in one transaction.
        let from = format!("{y:04}-{m:02}-01");
        let to = format!("{ny:04}-{nm:02}-01");
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(7700001)").execute(&mut *tx).await?;
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(&name)
            .fetch_one(&mut *tx)
            .await?;
        if !exists {
            let ddl = format!("CREATE TABLE {name} (LIKE events INCLUDING DEFAULTS)");
            sqlx::query(sqlx::AssertSqlSafe(ddl)).execute(&mut *tx).await?;
            let mv = format!(
                "WITH moved AS (DELETE FROM events_default WHERE ts >= '{from}' AND ts < '{to}' RETURNING *) INSERT INTO {name} SELECT * FROM moved"
            );
            sqlx::query(sqlx::AssertSqlSafe(mv)).execute(&mut *tx).await?;
            let attach = format!("ALTER TABLE events ATTACH PARTITION {name} FOR VALUES FROM ('{from}') TO ('{to}')");
            sqlx::query(sqlx::AssertSqlSafe(attach)).execute(&mut *tx).await?;
        }
        tx.commit().await?;
    }
    // Retention: drop whole partitions that end before the cut-off.
    let cutoff = now - chrono::Duration::days(retention_days.max(1));
    let parts: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
         JOIN pg_class p ON p.oid = i.inhparent WHERE p.relname = 'events' AND c.relname ~ '^events_[0-9]{6}$'",
    )
    .fetch_all(pool)
    .await?;
    for name in parts {
        let y: i32 = name[7..11].parse().unwrap_or(9999);
        let m: u32 = name[11..13].parse().unwrap_or(12);
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        let end = chrono::NaiveDate::from_ymd_opt(ny, nm, 1).unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc();
        if end < cutoff {
            sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE IF EXISTS {name}")))
                .execute(pool)
                .await?;
            tracing::info!(partition = %name, "dropped event partition past retention");
        }
    }
    // Rows in the default partition past retention.
    sqlx::query("DELETE FROM events_default WHERE ts < $1").bind(cutoff).execute(pool).await?;
    Ok(())
}
