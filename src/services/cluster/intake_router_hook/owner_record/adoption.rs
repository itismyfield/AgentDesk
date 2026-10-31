//! Initial adoption retains the original local binding; it never stamps old artifacts.
use super::{OwnerIdentity, custody};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("native binding cannot be verified: {0:?}")]
    Native(crate::services::tui_o::ownership::execution::NativeRefFailure),
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

pub(crate) async fn adopt_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
    local: &str,
    session_key: &str,
    binding: &Value,
) -> Result<bool, Error> {
    let channel = id.raw_channel_id().parse().map_err(|_| {
        Error::Native(crate::services::tui_o::ownership::execution::NativeRefFailure::Malformed)
    })?;
    #[cfg(test)]
    let bypass = super::cleanup::tests::mutant("adoption-native");
    #[cfg(not(test))]
    let bypass = false;
    if !bypass {
        crate::services::tui_o::ownership::execution::validate_local(
            id.provider(),
            channel,
            local,
            binding,
        )
        .map_err(Error::Native)?;
    }
    custody::lock(tx, id).await?;
    let sessions: Vec<(i64, String, String)> = sqlx::query_as("SELECT id,session_key,instance_id FROM sessions WHERE LOWER(BTRIM(provider))=$1 AND channel_id=$2 AND COALESCE(LOWER(BTRIM(status)),'') NOT IN ('disconnected','aborted') ORDER BY id FOR UPDATE")
        .bind(id.provider()).bind(id.raw_channel_id()).fetch_all(&mut **tx).await?;
    let [(session_id, locator, instance)] = sessions.as_slice() else {
        return Ok(false);
    };
    if locator != session_key
        || instance != local
        || !binding["owner"]
            .as_str()
            .is_some_and(|owner| locator.ends_with(&format!(":{owner}")))
    {
        return Ok(false);
    }
    let original = json!({"session_id":session_id,"session_key":session_key,"executor":local,"binding":binding});
    if let Some(existing) = super::read_latest_owner_in_tx(tx, id).await? {
        let matches: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND owner_instance_id=$3 AND generation=0 AND status='active' AND adopted_from_session AND routing_adoption=$4)")
            .bind(id.provider()).bind(id.raw_channel_id()).bind(local).bind(&original).fetch_one(&mut **tx).await?;
        return Ok(existing.generation == 0 && matches);
    }
    super::adopt_owner_from_session_in_tx(tx, id, local).await?;
    let changed = sqlx::query("UPDATE intake_session_owners SET routing_adoption=$3 WHERE provider=$1 AND raw_channel_id=$2 AND generation=0 AND adopted_from_session AND routing_adoption IS NULL")
        .bind(id.provider()).bind(id.raw_channel_id()).bind(original).execute(&mut **tx).await?.rows_affected();
    Ok(changed == 1)
}

#[cfg(all(test, unix))]
#[path = "adoption_tests.rs"]
mod tests;
