//! Blob store interface. Postgres is the only backend today; object storage
//! can implement the same trait later.

use anyhow::Result;
use async_trait::async_trait;
use sqlx::PgPool;

#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Store bytes; returns their content hash. Storing the same bytes twice
    /// stores them once.
    async fn put(&self, data: &[u8]) -> Result<String>;
    async fn get(&self, hash: &str) -> Result<Option<Vec<u8>>>;
    async fn delete(&self, hash: &str) -> Result<()>;
}

pub struct PgBlobStore {
    pub pool: PgPool,
}

#[async_trait]
impl BlobStore for PgBlobStore {
    async fn put(&self, data: &[u8]) -> Result<String> {
        let hash = crate::crypto::sha256_hex(data);
        sqlx::query("INSERT INTO blobs(hash, size, data) VALUES ($1,$2,$3) ON CONFLICT (hash) DO NOTHING")
            .bind(&hash)
            .bind(data.len() as i64)
            .bind(data)
            .execute(&self.pool)
            .await?;
        Ok(hash)
    }
    async fn get(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        Ok(sqlx::query_scalar("SELECT data FROM blobs WHERE hash=$1")
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?)
    }
    async fn delete(&self, hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM blobs WHERE hash=$1").bind(hash).execute(&self.pool).await?;
        Ok(())
    }
}
