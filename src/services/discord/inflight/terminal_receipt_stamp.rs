//! The receipt mark of a row whose confirmed terminal could not be committed. The row stays
//! until boot or the next admission on its channel records the mark and removes it.

use std::fs;
use std::path::Path;

use super::{GuardedSaveOutcome, InflightTurnIdentity, InflightTurnState, TerminalReceiptStamp};
use crate::db::turns::TerminalOutcome;
use crate::services::provider::ProviderKind;

impl InflightTurnState {
    /// A managed execution that consumed an origin and has no terminal recorded or marked.
    pub(in crate::services::discord) fn owes_origin_receipt(&self) -> bool {
        self.user_msg_id != 0
            && !self.input_origin_ids.is_empty()
            && self.terminal_receipt.is_none()
    }
}

/// Marks the on-disk row of this execution; an earlier mark is kept.
pub(in crate::services::discord) fn stamp_terminal_receipt_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    turn_nonce: Option<&str>,
    stamp: TerminalReceiptStamp,
) -> GuardedSaveOutcome {
    let Some(root) = super::inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = super::inflight_state_path(&root, provider, channel_id);
    let Ok(_lock) = super::lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedSaveOutcome::RowAbsent;
    };
    let Ok(mut on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        return GuardedSaveOutcome::AuthorityPinned;
    };
    let nonce_differs = matches!(
        (turn_nonce, on_disk.turn_nonce.as_deref()),
        (Some(expected), Some(found)) if expected != found
    );
    if !expected.matches_state(&on_disk) || nonce_differs {
        return GuardedSaveOutcome::SuccessorOwned;
    }
    if on_disk.terminal_receipt.is_some() {
        return GuardedSaveOutcome::Saved;
    }
    on_disk.terminal_receipt = Some(stamp);
    publish(&path, &mut on_disk)
}

/// Under the caller's row lock: a row about to be discarded while it still owes an origin its
/// receipt is kept instead, marked failed, or unmarked when the mark cannot be written.
pub(super) fn keep_owed_row_marked(
    path: &Path,
    row: &InflightTurnState,
) -> Option<InflightTurnState> {
    if !row.owes_origin_receipt() {
        return None;
    }
    let mut marked = row.clone();
    marked.terminal_receipt = Some(TerminalReceiptStamp {
        outcome: TerminalOutcome::Failed,
        confirmed_at: chrono::Utc::now(),
    });
    if publish(path, &mut marked) == GuardedSaveOutcome::Saved {
        Some(marked)
    } else {
        Some(row.clone())
    }
}

fn publish(path: &Path, row: &mut InflightTurnState) -> GuardedSaveOutcome {
    row.updated_at = super::now_string();
    if super::bump_save_generation_for_write(path, row).is_err() {
        return GuardedSaveOutcome::AuthorityPinned;
    }
    let Ok(json) = serde_json::to_string_pretty(row) else {
        return GuardedSaveOutcome::IoError;
    };
    match super::store::publish_row(path, &json) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "inflight receipt mark not written");
            GuardedSaveOutcome::IoError
        }
    }
}

#[cfg(test)]
#[path = "terminal_receipt_stamp_tests.rs"]
mod tests;
