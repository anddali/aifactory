//! Model provider role.

use super::Loaded;
use crate::AppState;
use anyhow::Result;

pub async fn test_connection(_state: &AppState, l: &Loaded) -> Result<String> {
    Ok(format!("model connector {} configured", l.row.name))
}
