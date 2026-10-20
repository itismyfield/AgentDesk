use std::io;
use std::path::Path;

use super::HerdrLaunchError;
use crate::db::dispatched_sessions::hosted_execution::{HostedExecution, HostedOwner};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RetiredHoldOutcome {
    Durable,
    NotDurable(String),
    Failed(String),
}

/// Rechecks the retired execution's own hold before preparing or committing its successor.
pub(super) async fn before_fresh(
    owner: &HostedOwner,
    retired: &HostedExecution,
) -> Result<(), HerdrLaunchError> {
    if &retired.owner != owner {
        return Err(HerdrLaunchError::Row(
            "retired execution owner changed".into(),
        ));
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
pub(crate) fn release(nonce: &str) -> RetiredHoldOutcome {
    let path = match crate::services::claude::herdr_turn::hold_path(nonce) {
        Ok(path) => path,
        Err(error) => return RetiredHoldOutcome::Failed(error),
    };
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return RetiredHoldOutcome::Failed(error.to_string()),
    }
    let (path, _) = match crate::services::claude::herdr_turn::unlink_hold(nonce) {
        Ok(unlinked) => unlinked,
        Err(error) => return RetiredHoldOutcome::Failed(error),
    };
    let Some(parent) = path.parent() else {
        return RetiredHoldOutcome::Failed("hold has no parent directory".into());
    };
    match std::fs::symlink_metadata(parent) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return RetiredHoldOutcome::Durable;
        }
        Err(error) => return RetiredHoldOutcome::Failed(error.to_string()),
        Ok(_) => {}
    }
    match sync_parent(&path) {
        Ok(()) if crate::services::discord::runtime_store::PARENT_DIR_FSYNC_FLUSHES => {
            RetiredHoldOutcome::Durable
        }
        Ok(()) => RetiredHoldOutcome::NotDurable("directory fsync is unavailable".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Confirm directory absence; read errors cannot prove the hold durably gone.
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

fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    super::tests::retired_durability_tests::before_sync(path)?;
    crate::services::discord::runtime_store::fsync_parent_dir(path)
}
