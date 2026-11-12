//! Entry dispatch-failure recording for the engine `recordDispatchFailure` op, planning and the
//! runtime failure path. An entry left `failed` attempts run completion on the same tx.

use super::completion::maybe_finalize_run_if_ready_pg;
use crate::db::auto_queue::EntryDispatchFailureResult;
use sqlx::PgPool;

pub(crate) async fn record_entry_dispatch_failure_on_pg(
    pool: &PgPool,
    entry_id: &str,
    max_retries: i64,
    trigger_source: &str,
) -> Result<EntryDispatchFailureResult, String> {
    let mut tx = pool.begin().await.map_err(|error| {
        format!("begin postgres auto-queue dispatch failure transaction: {error}")
    })?;
    let result =
        EntryDispatchFailureResult::record_on_pg_tx(&mut tx, entry_id, max_retries, trigger_source)
            .await?;
    tx.commit().await.map_err(|error| {
        format!("commit postgres auto-queue dispatch failure {entry_id}: {error}")
    })?;
    Ok(result)
}

impl EntryDispatchFailureResult {
    /// Apply one dispatch failure inside a caller-owned transaction, then attempt run
    /// completion there when the entry became `failed`.
    pub(crate) async fn record_on_pg_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        entry_id: &str,
        max_retries: i64,
        trigger_source: &str,
    ) -> Result<Self, String> {
        let result = Self::write_on_pg_tx(tx, entry_id, max_retries, trigger_source).await?;
        if result.run_finalize_due() {
            maybe_finalize_run_if_ready_pg(tx, &result.run_id).await?;
        }
        Ok(result)
    }
}
