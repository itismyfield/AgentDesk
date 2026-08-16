use std::path::Path;

use super::{
    InflightTurnIdentity, InflightTurnState, inflight_runtime_root, inflight_state_path,
    lock_inflight_state_path,
};
use crate::services::provider::ProviderKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum DestructiveCancelPinField {
    Identity,
    UpdatedAt,
    SaveGeneration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum DestructiveCancelCommitOutcome {
    /// The pin matched and the callback reported a pinned watcher to cancel.
    /// Since #5071 T3-A1 neither caller cancels inside the callback, so this
    /// records that the row pin held over a watcher-bearing turn — not that any
    /// watcher was cancelled. The cancel happens afterwards, under the registry
    /// CAS, and can still fail closed there.
    CommittedCancelled,
    /// The pin matched, but the caller had pinned no watcher to cancel.
    CommittedNoWatcher,
    PinMismatch {
        field: DestructiveCancelPinField,
    },
    RowMissing,
    RowMalformed,
    /// Lock setup or syscall failure. Lock contention blocks because the flock
    /// uses `LOCK_EX` without `LOCK_NB` or a timeout; it does not return this.
    LockUnavailable,
    IoError,
}

/// What the verified callback found. Since #5071 T3-A1 both callers report the
/// SHAPE of the pin rather than an action they already took: the actual cancel
/// store now happens after this function returns, inside the watcher-registry
/// CAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum CommitEvidence {
    CancelledWatcher,
    NoWatcher,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct CommitError {
    pub message: String,
}

/// Verifies the inflight row under its sidecar flock and then runs a callback.
///
/// A `Committed*` outcome means only that the row matched the identity plus
/// `save_generation` pin at that instant and the callback returned typed evidence.
/// `CommittedCancelled` distinguishes a watcher-bearing pin from
/// `CommittedNoWatcher`. Neither means that destructive authority was acquired
/// exclusively or durably, and since #5071 T3-A1 neither means a watcher was
/// cancelled: both callers store `cancel` only after the later registry CAS.
/// `updated_at` is checked as supplemental diagnostics, but its one-second local
/// timestamp resolution means it is not an independent fencing dimension.
///
/// Excluded writers: every resurrection that already completed a flock-held
/// persist (the three watcher persists, both creation APIs, and the legacy
/// rebind-origin backfill). Those writers advance `save_generation`; a different
/// turn changes the identity, so either change aborts with `PinMismatch`.
///
/// Not closed:
/// - delivery/persist from the last watcher iteration before it observes cancel;
/// - row recreation after destruction (#5012/E3);
/// - durable observation of a partially degraded finalizer commit (E2/E6);
/// - the callback writes no durable intent/epoch, so a crash after a `Committed*` outcome
///   can erase the cancel decision before finalization starts;
/// - the callback does not advance row version, so multiple serialized callers
///   may receive a `Committed*` outcome; downstream registry CAS and the finalizer's
///   exact-key ledger prevent duplicate finalization, not duplicate commit claims;
/// - the cancel `Arc` and the #5071 T3-A1 spawn-nonce pin are both captured
///   outside the flock and may name a replaced watcher incarnation; the registry
///   CAS fails closed with nothing cancelled whenever one of those captured
///   VALUES stops equalling the live row, but a value comparison cannot see a row
///   that was replaced and then re-admitted with every pinned value restored
///   (`tmux_watcher_registry::WatcherIdentityFence` declares that limit). Either
///   way a `Committed*` outcome was already returned;
/// - sidecar flock authority is host-local and does not fence another node's
///   watcher, inflight row, or mailbox authority;
/// - the age/lifecycle-qualified stale-row sweep in `reconcile.rs` removes rows
///   without taking this sidecar flock.
///
/// # Safety
///
/// The callback must not acquire the watcher-registry mutex. This is a caller
/// contract rather than a type-level guarantee: violating it can create an ABBA
/// deadlock with a registry holder waiting for the sidecar flock, causing a
/// permanent hang rather than a recoverable commit failure. E2 may prepare and
/// fsync a temporary intent before this call, but while the flock is held its
/// callback may only rename that prepared file; directory fsync belongs after
/// this function returns and before destruction proceeds.
pub(in crate::services::discord) fn commit_destructive_cancel_locked(
    provider: &ProviderKind,
    channel_id: u64,
    expected_identity: &InflightTurnIdentity,
    expected_updated_at: &str,
    expected_save_generation: u64,
    on_verified: impl FnOnce(&InflightTurnState) -> Result<CommitEvidence, CommitError>,
) -> DestructiveCancelCommitOutcome {
    let Some(root) = inflight_runtime_root() else {
        return DestructiveCancelCommitOutcome::IoError;
    };
    let path = inflight_state_path(&root, provider, channel_id);
    commit_destructive_cancel_locked_at_path(
        &path,
        expected_identity,
        expected_updated_at,
        expected_save_generation,
        on_verified,
    )
}

fn commit_destructive_cancel_locked_at_path(
    path: &Path,
    expected_identity: &InflightTurnIdentity,
    expected_updated_at: &str,
    expected_save_generation: u64,
    on_verified: impl FnOnce(&InflightTurnState) -> Result<CommitEvidence, CommitError>,
) -> DestructiveCancelCommitOutcome {
    let Ok(_flock) = lock_inflight_state_path(path) else {
        return DestructiveCancelCommitOutcome::LockUnavailable;
    };
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return DestructiveCancelCommitOutcome::RowMissing;
        }
        Err(_) => return DestructiveCancelCommitOutcome::IoError,
    };
    let current = match serde_json::from_str::<InflightTurnState>(&content) {
        Ok(current) => current,
        Err(_) => return DestructiveCancelCommitOutcome::RowMalformed,
    };
    if !expected_identity.matches_state(&current) {
        return DestructiveCancelCommitOutcome::PinMismatch {
            field: DestructiveCancelPinField::Identity,
        };
    }
    if current.updated_at != expected_updated_at {
        return DestructiveCancelCommitOutcome::PinMismatch {
            field: DestructiveCancelPinField::UpdatedAt,
        };
    }
    if current.save_generation != expected_save_generation {
        return DestructiveCancelCommitOutcome::PinMismatch {
            field: DestructiveCancelPinField::SaveGeneration,
        };
    }
    match on_verified(&current) {
        Ok(CommitEvidence::CancelledWatcher) => DestructiveCancelCommitOutcome::CommittedCancelled,
        Ok(CommitEvidence::NoWatcher) => DestructiveCancelCommitOutcome::CommittedNoWatcher,
        Err(_) => DestructiveCancelCommitOutcome::IoError,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    fn state() -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            5014,
            None,
            1,
            501_401,
            501_402,
            "destructive commit fixture".to_string(),
            None,
            Some("AgentDesk-codex-5014".to_string()),
            Some("/fixture/output.jsonl".to_string()),
            None,
            0,
        );
        state.started_at = "2026-07-30 12:00:00".to_string();
        state.updated_at = "2026-07-30 12:02:00".to_string();
        state.turn_start_offset = Some(17);
        state.save_generation = 9;
        state
    }

    fn write(path: &Path, state: &InflightTurnState) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(
            path,
            serde_json::to_string_pretty(state).expect("serialize"),
        )
        .expect("write row");
    }

    fn commit(
        path: &Path,
        expected: &InflightTurnState,
        callback: impl FnOnce(&InflightTurnState) -> Result<CommitEvidence, CommitError>,
    ) -> DestructiveCancelCommitOutcome {
        commit_destructive_cancel_locked_at_path(
            path,
            &InflightTurnIdentity::from_state(expected),
            &expected.updated_at,
            expected.save_generation,
            callback,
        )
    }

    #[test]
    fn typed_outcomes_cover_commit_and_every_pin_field() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("row.json");
        let baseline = state();
        write(&path, &baseline);
        assert_eq!(
            commit(&path, &baseline, |_| Ok(CommitEvidence::CancelledWatcher)),
            DestructiveCancelCommitOutcome::CommittedCancelled
        );
        assert_eq!(
            commit(&path, &baseline, |_| Ok(CommitEvidence::NoWatcher)),
            DestructiveCancelCommitOutcome::CommittedNoWatcher
        );

        let mut changed = baseline.clone();
        changed.user_msg_id += 1;
        write(&path, &changed);
        assert_eq!(
            commit(&path, &baseline, |_| Ok(CommitEvidence::CancelledWatcher)),
            DestructiveCancelCommitOutcome::PinMismatch {
                field: DestructiveCancelPinField::Identity
            }
        );

        let mut changed = baseline.clone();
        changed.updated_at.push('1');
        write(&path, &changed);
        assert_eq!(
            commit(&path, &baseline, |_| Ok(CommitEvidence::CancelledWatcher)),
            DestructiveCancelCommitOutcome::PinMismatch {
                field: DestructiveCancelPinField::UpdatedAt
            }
        );

        let mut changed = baseline.clone();
        changed.save_generation += 1;
        write(&path, &changed);
        assert_eq!(
            commit(&path, &baseline, |_| Ok(CommitEvidence::CancelledWatcher)),
            DestructiveCancelCommitOutcome::PinMismatch {
                field: DestructiveCancelPinField::SaveGeneration
            }
        );
    }

    #[test]
    fn typed_outcomes_cover_missing_malformed_lock_and_io_failures() {
        let root = tempfile::tempdir().expect("root");
        let baseline = state();
        let missing = root.path().join("missing.json");
        assert_eq!(
            commit(&missing, &baseline, |_| Ok(
                CommitEvidence::CancelledWatcher
            )),
            DestructiveCancelCommitOutcome::RowMissing
        );

        let malformed = root.path().join("malformed.json");
        std::fs::write(&malformed, "{").expect("write malformed");
        assert_eq!(
            commit(&malformed, &baseline, |_| Ok(
                CommitEvidence::CancelledWatcher
            )),
            DestructiveCancelCommitOutcome::RowMalformed
        );

        let blocked_parent = root.path().join("not-a-directory");
        std::fs::write(&blocked_parent, "file").expect("write blocking file");
        assert_eq!(
            commit(&blocked_parent.join("row.json"), &baseline, |_| Ok(
                CommitEvidence::CancelledWatcher
            )),
            DestructiveCancelCommitOutcome::LockUnavailable
        );

        let directory_row = root.path().join("directory-row.json");
        std::fs::create_dir(&directory_row).expect("create directory row");
        assert_eq!(
            commit(&directory_row, &baseline, |_| Ok(
                CommitEvidence::CancelledWatcher
            )),
            DestructiveCancelCommitOutcome::IoError
        );

        write(&malformed, &baseline);
        assert_eq!(
            commit(&malformed, &baseline, |_| {
                Err(CommitError {
                    message: "intent rename failed".to_string(),
                })
            }),
            DestructiveCancelCommitOutcome::IoError
        );
    }

    #[test]
    fn generation_advance_aborts_before_cancel_store() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("row.json");
        let baseline = state();
        let mut revived = baseline.clone();
        revived.save_generation += 1;
        write(&path, &revived);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_commit = cancel.clone();

        assert_eq!(
            commit(&path, &baseline, move |_| {
                cancel_for_commit.store(true, Ordering::Release);
                Ok(CommitEvidence::CancelledWatcher)
            }),
            DestructiveCancelCommitOutcome::PinMismatch {
                field: DestructiveCancelPinField::SaveGeneration
            }
        );
        assert!(!cancel.load(Ordering::Acquire));
    }
}
