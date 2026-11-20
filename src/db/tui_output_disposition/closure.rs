//! The native turn a rejection closed, kept on its reason row so a later takeover never starts on it.
use sqlx::PgPool;

use super::{Identity, Reason};

/// A rollout turn by its source identity and native opener; `until` is where that turn ended
/// when it had ended. It records no delivery and no body.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Closure {
    pub(crate) session_id: String,
    pub(crate) path: String,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) from: u64,
    pub(crate) until: Option<u64>,
}

/// The intent and the turn it closed, committed together; see [`persist_in`].
pub(crate) async fn persist(
    pool: &PgPool,
    id: &Identity,
    reason: Reason,
    closure: Option<&Closure>,
) -> Result<(), String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    persist_in(&mut tx, id, reason, closure).await?;
    tx.commit().await.map_err(|e| e.to_string())
}

/// The intent with its closure in the caller's transaction. A row that holds a closure keeps it;
/// one stored without a closure takes this one once.
pub(crate) async fn persist_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: &Identity,
    reason: Reason,
    closure: Option<&Closure>,
) -> Result<(), String> {
    super::persist_intent_in(tx, id, reason).await?;
    let Some(closure) = closure else {
        return Ok(());
    };
    let closure = serde_json::to_value(closure).map_err(|e| e.to_string())?;
    sqlx::query(
        "UPDATE tui_output_dispositions SET closure=$2 WHERE output_key=$1 AND closure IS NULL",
    )
    .bind(id.key())
    .bind(closure)
    .execute(&mut **tx)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Every closure stored for `channel`'s rejections.
pub(crate) async fn of_channel(pool: &PgPool, channel: u64) -> Result<Vec<Closure>, String> {
    let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT closure FROM tui_output_dispositions WHERE closure IS NOT NULL AND identity->>'channel'=$1 ORDER BY output_key",
    )
    .bind(channel.to_string())
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    rows.into_iter()
        .map(|(closure,)| serde_json::from_value(closure).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
#[path = "closure_tests.rs"]
mod tests;
