#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
use super::HerdrLaunchError;
#[cfg(unix)]
use crate::db::dispatched_sessions::hosted_execution::{HostedExecution, HostedOwner};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RetiredHoldOutcome {
    Durable,
    NotDurable(String),
    Failed(String),
}

/// Rechecks the retired execution's own hold before preparing or committing its successor.
#[cfg(unix)]
pub(super) async fn before_fresh(
    owner: &HostedOwner,
    retired: &HostedExecution,
) -> Result<(), HerdrLaunchError> {
    let result = checked_before_fresh(owner, retired).await;
    if let Err(error) = &result {
        tracing::warn!(
            event = "herdr_retired_hold_not_durable",
            nonce = %retired.execution_nonce,
            ?error,
            "retired input hold is not durable"
        );
    }
    result
}

#[cfg(unix)]
async fn checked_before_fresh(
    owner: &HostedOwner,
    retired: &HostedExecution,
) -> Result<(), HerdrLaunchError> {
    if &retired.owner != owner {
        let stored = &retired.owner;
        let fields = [
            ("provider", &stored.provider, &owner.provider),
            (
                "discord_token_hash",
                &stored.discord_token_hash,
                &owner.discord_token_hash,
            ),
            ("channel_id", &stored.channel_id, &owner.channel_id),
            ("logical_key", &stored.logical_key, &owner.logical_key),
            ("owner_node", &stored.owner_node, &owner.owner_node),
            ("runtime_root", &stored.runtime_root, &owner.runtime_root),
        ]
        .into_iter()
        .filter(|(_, stored, expected)| stored != expected)
        .map(|(field, _, _)| field)
        .collect::<Vec<_>>()
        .join(", ");
        return Err(HerdrLaunchError::RetiredHold(format!(
            "retired owner differs: {fields}"
        )));
    }
    let nonce = retired.execution_nonce.clone();
    let outcome = tokio::task::spawn_blocking(move || release(&nonce))
        .await
        .map_err(|error| HerdrLaunchError::RetiredHold(error.to_string()))?;
    match outcome {
        RetiredHoldOutcome::Durable => Ok(()),
        RetiredHoldOutcome::NotDurable(error) | RetiredHoldOutcome::Failed(error) => {
            Err(HerdrLaunchError::RetiredHold(error))
        }
    }
}

/// Removes only this nonce's hold; even an earlier unlink needs a directory flush.
#[cfg(unix)]
pub(crate) fn release(nonce: &str) -> RetiredHoldOutcome {
    let (path, _) = match crate::services::claude::herdr_turn::unlink_hold(nonce) {
        Ok(unlinked) => unlinked,
        Err(error) => return RetiredHoldOutcome::Failed(error),
    };
    let Some(parent) = path.parent() else {
        return RetiredHoldOutcome::Failed("hold has no parent directory".into());
    };
    // Path-resolution NotFound includes missing ancestors, which also establish namespace absence.
    match std::fs::symlink_metadata(parent) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return RetiredHoldOutcome::Durable;
        }
        Err(error) => return RetiredHoldOutcome::Failed(error.to_string()),
        Ok(_) => {}
    }
    match sync_parent(&path) {
        Ok(()) => RetiredHoldOutcome::Durable,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Reconfirm path-resolution absence, including ancestors; read failures prove nothing.
            match std::fs::symlink_metadata(parent) {
                Err(missing) if missing.kind() == io::ErrorKind::NotFound => {
                    RetiredHoldOutcome::Durable
                }
                Err(error) => RetiredHoldOutcome::Failed(error.to_string()),
                Ok(_) => RetiredHoldOutcome::NotDurable(error.to_string()),
            }
        }
        Err(error) => RetiredHoldOutcome::NotDurable(error.to_string()),
    }
}

/// No supported directory flush: refuse before resolving or touching any hold.
#[cfg(not(unix))]
pub(crate) fn release(_nonce: &str) -> RetiredHoldOutcome {
    RetiredHoldOutcome::NotDurable("retired hold durability is unavailable on this platform".into())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    super::tests::retired_durability_tests::before_sync(path)?;
    crate::services::discord::runtime_store::fsync_parent_dir(path)
}
