//! Registered routine script refs (DB side of the "unregistered script"
//! diagnostics, #5727). Kept out of `store.rs` to respect the giant-file gate.

use std::collections::HashSet;

use anyhow::{Result, anyhow};
use sqlx::PgPool;

/// Every `script_ref` attached in the `routines` table, regardless of status
/// (enabled/paused/detached all count as "registered"). Used by the boot
/// warning and `agentdesk doctor` to spot scripts on disk nobody attached.
pub async fn registered_routine_script_refs(pool: &PgPool) -> Result<HashSet<String>> {
    let refs: Vec<String> = sqlx::query_scalar("SELECT DISTINCT script_ref FROM routines")
        .fetch_all(pool)
        .await
        .map_err(|e| anyhow!("list registered routine script refs: {e}"))?;
    Ok(refs.into_iter().collect())
}
