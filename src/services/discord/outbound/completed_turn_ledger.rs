//! #4564 durable completed-turn ledger — a sidecar record of the INBOUND user
//! message ids whose turns reached a confirmed terminal delivery.
//!
//! ## Why this exists
//!
//! After a dcserver restart the catch-up scan re-reads the recent channel
//! history and, for any message it does not recognize as already handled, ages
//! it out to `TooOld` past the 5-minute window (`catch_up/classification.rs`).
//! The "already handled" evidence used to be the pre-processing
//! checkpoint/frontier cursor, which advances BEFORE the turn is delivered
//! (router intake gate) — so a restart re-flags already-answered messages as
//! "unprocessed", the P1 UX bug in #4564. PR #4600 tried promoting the
//! checkpoint/frontier value itself to "settled" and was closed P1 for
//! silent-loss (a checkpoint moves without a delivery).
//!
//! This ledger is the durable authority the catch-up gate consults instead. It
//! is keyed by the INBOUND `user_msg_id` and appended ONLY from a genuine
//! terminal-delivery commit (`is_delivered == true`), never from a checkpoint
//! cursor. A missing ledger entry falls through to the legacy `TooOld`/DLQ path
//! (the ledger only SUPPRESSES the false notice; it never gates the DLQ write),
//! so a crash between the delivery commit and the ledger append can never cause
//! silent loss.
//!
//! ## Storage
//!
//! A sibling of `discord_delivery_records/` under `runtime/`, one JSON file per
//! `(provider, channel_id)` holding a bounded ring of
//! [`CompletedTurnEntry`]. It reuses [`delivery_record`]'s per-record flock
//! (`lock_record_path`) and [`runtime_store::atomic_write`] — no new lock
//! mechanism. Like the delivery-record sidecar, its dedicated subtree keeps it
//! outside the old-binary inflight reaper's scan set.
//! Merged-head aliases: `H`, absorbed by episode `n` of head `P`, is settled only when one read
//! holds both `alias(P, n, H)` and `entry(P, Some(n))`; a missing link duplicates, never loses.

#[cfg(unix)]
pub(in crate::services::discord) mod o_pending;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::delivery_record;
use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

/// Sidecar subtree name — a sibling of `discord_delivery_records/`.
const COMPLETED_TURN_LEDGER_DIR: &str = "discord_completed_turn_ledger";

/// Retain window for ledger entries: `max(catch_up_max_age = 5min, 48h) = 48h`.
/// An entry older than this can no longer be inside the catch-up 5-minute
/// re-scan window on any realistic restart, so it is pruned.
const LEDGER_RETENTION_MS: u64 = 48 * 60 * 60 * 1000;

/// Hard cap on retained entries (the tighter of the time-window / cap bound).
const LEDGER_ENTRY_CAP: usize = 500;

/// Cap on aliases no entry backs yet. Claims are serialized per channel, so the
/// active turn's alias is always the newest and never the one evicted.
const ROWLESS_ALIAS_CAP: usize = 64;

/// One completed turn: the inbound `user_msg_id` and when its terminal delivery
/// committed. `committed_at_epoch_ms` drives the retention prune.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct CompletedTurnEntry {
    pub user_msg_id: u64,
    pub committed_at_epoch_ms: u64,
    /// The delivered episode, only when the delivery proved it; `None` never certifies an alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_nonce: Option<String>,
}

/// Episode `turn_nonce` of merged head `primary` absorbed `absorbed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct MergedAlias {
    pub primary: u64,
    pub turn_nonce: String,
    pub absorbed: Vec<u64>,
    pub claimed_at_epoch_ms: u64,
}

/// The durable per-channel ledger — a bounded ring of completed turns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct CompletedTurnLedger {
    #[serde(default)]
    pub entries: Vec<CompletedTurnEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merged_aliases: Vec<MergedAlias>,
}

/// The one alias join: `entry` is episode `alias.turn_nonce` of `alias.primary`; `None` never joins.
fn entry_backs(entry: &CompletedTurnEntry, alias: &MergedAlias) -> bool {
    entry.user_msg_id == alias.primary
        && entry.turn_nonce.as_deref() == Some(alias.turn_nonce.as_str())
}

impl CompletedTurnLedger {
    fn backs(&self, alias: &MergedAlias) -> bool {
        self.entries.iter().any(|entry| entry_backs(entry, alias))
    }

    /// `entries.ids ∪ { a ∈ alias.absorbed | entry(alias.primary, Some(alias.turn_nonce)) }`,
    /// from this one read.
    pub(in crate::services::discord) fn settled_ids(&self) -> HashSet<u64> {
        self.settled_commit_ms().into_keys().collect()
    }

    /// Commit time of each [`Self::settled_ids`] id: a direct id keeps its last-appended entry's
    /// time; an absorbed id takes its backing entry's, or the later of that and its own entry's.
    fn settled_commit_ms(&self) -> HashMap<u64, u64> {
        let mut commits: HashMap<u64, u64> = self
            .entries
            .iter()
            .map(|entry| (entry.user_msg_id, entry.committed_at_epoch_ms))
            .collect();
        for entry in &self.entries {
            let backed = self
                .merged_aliases
                .iter()
                .filter(|alias| entry_backs(entry, alias));
            for id in backed.flat_map(|alias| alias.absorbed.iter().copied()) {
                let commit = commits.entry(id).or_insert(entry.committed_at_epoch_ms);
                *commit = (*commit).max(entry.committed_at_epoch_ms);
            }
        }
        commits
    }

    /// The ids episode `turn_nonce` of `primary` absorbed, per its durable alias.
    pub(in crate::services::discord) fn absorbed_by_episode(
        &self,
        primary: u64,
        turn_nonce: &str,
    ) -> Vec<u64> {
        self.merged_aliases
            .iter()
            .filter(|alias| alias.primary == primary && alias.turn_nonce == turn_nonce)
            .flat_map(|alias| alias.absorbed.iter().copied())
            .collect()
    }
}

fn ledger_root() -> Option<PathBuf> {
    runtime_store::runtime_root().map(|root| root.join(COMPLETED_TURN_LEDGER_DIR))
}

/// `<runtime_root>/discord_completed_turn_ledger/<provider>/<channel_id>.json`.
pub(in crate::services::discord) fn ledger_path(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<PathBuf> {
    ledger_root().map(|root| {
        root.join(provider.as_str())
            .join(format!("{channel_id}.json"))
    })
}

/// Conservative read: missing OR malformed → `None`, never an error a caller
/// might misread as "settled". A torn/garbage file reads as no completed turns.
pub(in crate::services::discord) fn read_ledger_at(path: &Path) -> Option<CompletedTurnLedger> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Read the durable ledger for `(provider, channel_id)`. `None` when the runtime
/// root is unavailable, the file is missing, or the content is malformed.
pub(in crate::services::discord) fn read_ledger(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<CompletedTurnLedger> {
    read_ledger_at(&ledger_path(provider, channel_id)?)
}

/// The set of settled inbound `user_msg_id`s for `(provider, channel_id)`. Empty
/// when the ledger is absent/malformed (conservative — an unreadable ledger
/// suppresses NOTHING, so a real message is never wrongly treated as settled).
/// Production scans read through `catch_up::settled_ledger_consult`, which
/// needs the same read for the restart arm; this is the plain set for tests.
#[cfg(test)]
pub(in crate::services::discord) fn settled_user_msg_ids(
    provider: &ProviderKind,
    channel_id: u64,
) -> HashSet<u64> {
    read_ledger(provider, channel_id)
        .map(|ledger| ledger.settled_ids())
        .unwrap_or_default()
}

/// Commit time (Unix ms) of each settled inbound `user_msg_id`, merged-head aliases
/// included; empty when the ledger is absent or malformed, exactly like [`settled_user_msg_ids`].
pub(crate) fn settled_commit_ms_by_user_msg_id(
    provider: &ProviderKind,
    channel_id: u64,
) -> HashMap<u64, u64> {
    read_ledger(provider, channel_id)
        .map(|ledger| ledger.settled_commit_ms())
        .unwrap_or_default()
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Lazy prune (mirrors `delivery_record::prune_recent_content_fingerprints`):
/// drop entries older than the retention window, then cap to the newest
/// [`LEDGER_ENTRY_CAP`] by dropping from the front (oldest).
fn prune_entries(entries: &mut Vec<CompletedTurnEntry>, now_ms: u64) {
    entries
        .retain(|entry| now_ms.saturating_sub(entry.committed_at_epoch_ms) <= LEDGER_RETENTION_MS);
    if entries.len() > LEDGER_ENTRY_CAP {
        entries.drain(0..entries.len() - LEDGER_ENTRY_CAP);
    }
}

/// Prune entries, then aliases. A row-backed alias dies with its row; rowless ones
/// are capped at [`ROWLESS_ALIAS_CAP`] only, earliest-appended evicted first.
fn prune_ledger(ledger: &mut CompletedTurnLedger, now_ms: u64) {
    let backed_before: Vec<bool> = ledger
        .merged_aliases
        .iter()
        .map(|alias| ledger.backs(alias))
        .collect();
    prune_entries(&mut ledger.entries, now_ms);
    let aliases = std::mem::take(&mut ledger.merged_aliases);
    let (mut kept, mut rowless) = (Vec::new(), Vec::new());
    for (alias, backed_before) in aliases.into_iter().zip(backed_before) {
        if !backed_before {
            rowless.push(alias);
        } else if ledger.backs(&alias) {
            kept.push(alias);
        }
    }
    // Evict by stored append order, not `claimed_at`: the wall clock can move backwards.
    if rowless.len() > ROWLESS_ALIAS_CAP {
        rowless.drain(0..rowless.len() - ROWLESS_ALIAS_CAP);
    }
    kept.extend(rowless);
    ledger.merged_aliases = kept;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentSync {
    Flushed,
    Unflushed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Publication {
    Skipped,
    Published(ParentSync),
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
enum PublishError {
    #[error("write failed: {0}")]
    WriteFailed(String),
    #[error("parent sync failed after publication: {0}")]
    ParentSyncFailed(String),
}

/// The ledger write a log line reports, with the id that names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LedgerOperation {
    Append { user_msg_id: u64 },
    MergedAlias { primary: u64 },
}

/// Logs what a publication left behind: failures as warnings, an unflushed one at debug.
fn log_publish_outcome(
    outcome: Result<Publication, PublishError>,
    provider: &ProviderKind,
    channel_id: u64,
    operation: LedgerOperation,
) {
    let provider = provider.as_str();
    let (error, published, message) = match outcome {
        Ok(Publication::Published(ParentSync::Unflushed)) => {
            return tracing::debug!(
                provider, channel_id, operation = ?operation, published = true,
                "completed-turn ledger published without a directory flush (durability unconfirmed)"
            );
        }
        Ok(_) => return,
        Err(PublishError::ParentSyncFailed(error)) => (
            error,
            true,
            "completed-turn ledger published; parent sync failed (best-effort durability unconfirmed)",
        ),
        Err(PublishError::WriteFailed(error)) => (
            error,
            false,
            "completed-turn ledger write failed (best-effort; settlement evidence unavailable)",
        ),
    };
    match operation {
        LedgerOperation::Append { user_msg_id } => tracing::warn!(
            provider, channel_id, user_msg_id, operation = "append", issue = "#4564",
            error = %error, published, "{message}"
        ),
        LedgerOperation::MergedAlias { primary } => tracing::warn!(
            provider, channel_id, primary, operation = "merged_alias", issue = "#6035",
            error = %error, published, "{message}"
        ),
    }
}

fn mutate_at(
    path: &Path,
    now_ms: u64,
    mutate: impl FnOnce(&mut CompletedTurnLedger),
) -> Result<Publication, PublishError> {
    mutate_at_with_sync(
        path,
        now_ms,
        mutate,
        runtime_store::fsync_parent_and_ancestors,
        runtime_store::PARENT_DIR_FSYNC_FLUSHES,
    )
}

fn mutate_at_with_sync(
    path: &Path,
    now_ms: u64,
    mutate: impl FnOnce(&mut CompletedTurnLedger),
    sync: impl FnOnce(&Path) -> std::io::Result<()>,
    flushes: bool,
) -> Result<Publication, PublishError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| PublishError::WriteFailed(e.to_string()))?;
    }
    let _lock = delivery_record::lock_record_path(path).map_err(PublishError::WriteFailed)?;
    let mut ledger = read_ledger_at(path).unwrap_or_default();
    mutate(&mut ledger);
    prune_ledger(&mut ledger, now_ms);
    let data = serde_json::to_string_pretty(&ledger)
        .map_err(|e| PublishError::WriteFailed(e.to_string()))?;
    #[cfg(test)]
    BEFORE_PUBLISH.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    runtime_store::atomic_write(path, &data).map_err(PublishError::WriteFailed)?;
    // The rename is already visible; a parent-sync failure must never retry the write.
    sync(path).map_err(|error| PublishError::ParentSyncFailed(error.to_string()))?;
    Ok(Publication::Published(if flushes {
        ParentSync::Flushed
    } else {
        ParentSync::Unflushed
    }))
}

/// flock-guarded append deduped by `(user_msg_id, turn_nonce)`, keeping the latest commit time.
/// `user_msg_id == 0` (no inbound message) records nothing: there is nothing to suppress.
fn append_at(
    path: &Path,
    user_msg_id: u64,
    turn_nonce: Option<&str>,
    committed_at_epoch_ms: u64,
) -> Result<Publication, PublishError> {
    append_at_with_sync(
        path,
        user_msg_id,
        turn_nonce,
        committed_at_epoch_ms,
        runtime_store::fsync_parent_and_ancestors,
        runtime_store::PARENT_DIR_FSYNC_FLUSHES,
    )
}

fn append_at_with_sync(
    path: &Path,
    user_msg_id: u64,
    turn_nonce: Option<&str>,
    committed_at_epoch_ms: u64,
    sync: impl FnOnce(&Path) -> std::io::Result<()>,
    flushes: bool,
) -> Result<Publication, PublishError> {
    if user_msg_id == 0 {
        return Ok(Publication::Skipped);
    }
    let turn_nonce = turn_nonce
        .filter(|nonce| !nonce.is_empty())
        .map(str::to_owned);
    mutate_at_with_sync(
        path,
        committed_at_epoch_ms,
        |ledger| {
            ledger
                .entries
                .retain(|entry| entry.user_msg_id != user_msg_id || entry.turn_nonce != turn_nonce);
            ledger.entries.push(CompletedTurnEntry {
                user_msg_id,
                committed_at_epoch_ms,
                turn_nonce,
            });
        },
        sync,
        flushes,
    )
}

fn record_alias_at(
    path: &Path,
    primary: u64,
    turn_nonce: &str,
    absorbed: &[u64],
    claimed_at_epoch_ms: u64,
) -> Result<Publication, PublishError> {
    let absorbed: Vec<u64> = absorbed
        .iter()
        .copied()
        .filter(|id| *id != 0 && *id != primary)
        .collect();
    if primary == 0 || turn_nonce.is_empty() || absorbed.is_empty() {
        return Ok(Publication::Skipped);
    }
    mutate_at(path, claimed_at_epoch_ms, |ledger| {
        ledger
            .merged_aliases
            .retain(|alias| alias.primary != primary || alias.turn_nonce != turn_nonce);
        ledger.merged_aliases.push(MergedAlias {
            primary,
            turn_nonce: turn_nonce.to_owned(),
            absorbed,
            claimed_at_epoch_ms,
        });
    })
}

/// Append `user_msg_id` as a completed turn for `(provider, channel_id)`. Called
/// ONLY from a confirmed terminal-delivery commit (`is_delivered == true`) — the
/// SAME gate a `DeliveredCommit` write obeys, never a checkpoint cursor. Best
/// effort: a path/IO failure is logged and swallowed (the ledger only suppresses
/// a false notice; its absence falls through to the legacy TooOld/DLQ path, so a
/// missed append is never silent loss).
pub(in crate::services::discord) fn append_completed_turn(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) {
    append_completed_episode(provider, channel_id, user_msg_id, None);
}

/// [`append_completed_turn`] for a delivery that proved its episode. Pass a nonce only with
/// same-episode provenance: a wrong one could settle another episode's absorbed ids.
pub(in crate::services::discord) fn append_completed_episode(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    turn_nonce: Option<&str>,
) {
    if user_msg_id == 0 {
        return;
    }
    let Some(path) = ledger_path(provider, channel_id) else {
        tracing::warn!(
            provider = provider.as_str(),
            channel_id,
            user_msg_id,
            "#4564 completed-turn ledger path unavailable (runtime root); skipping append"
        );
        return;
    };
    let outcome = append_at(&path, user_msg_id, turn_nonce, now_epoch_ms());
    log_publish_outcome(
        outcome,
        provider,
        channel_id,
        LedgerOperation::Append { user_msg_id },
    );
}

/// Durably record that episode `turn_nonce` of `primary` absorbed `absorbed`, before the claim
/// is reported. Best effort: a missing alias leaves them unsettled (a duplicate, never loss).
pub(in crate::services::discord) fn record_merged_alias(
    provider: &ProviderKind,
    channel_id: u64,
    primary: u64,
    turn_nonce: &str,
    absorbed: &[u64],
) {
    let Some(path) = ledger_path(provider, channel_id) else {
        return;
    };
    let outcome = record_alias_at(&path, primary, turn_nonce, absorbed, now_epoch_ms());
    log_publish_outcome(
        outcome,
        provider,
        channel_id,
        LedgerOperation::MergedAlias { primary },
    );
}

#[cfg(test)]
thread_local! {
    static BEFORE_PUBLISH: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(in crate::services::discord) fn append_before_publish_for_tests(
    channel_id: u64,
    user_msg_id: u64,
    nonce: &str,
    committed_ms: u64,
    hook: impl FnOnce() + 'static,
) -> Result<(), String> {
    BEFORE_PUBLISH.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    let path = ledger_path(&ProviderKind::Claude, channel_id).ok_or("test ledger path missing")?;
    append_at(&path, user_msg_id, Some(nonce), committed_ms)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs(run: impl FnOnce()) -> String {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let output = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || Capture(output.clone()))
            .finish();
        crate::logging::test_capture::pin_callsite_interest();
        tracing::subscriber::with_default(subscriber, run);
        let bytes = buffer.lock().unwrap_or_else(|e| e.into_inner()).clone();
        String::from_utf8(bytes).unwrap()
    }

    fn entry(user_msg_id: u64, committed_at_epoch_ms: u64) -> CompletedTurnEntry {
        CompletedTurnEntry {
            user_msg_id,
            committed_at_epoch_ms,
            turn_nonce: None,
        }
    }

    fn alias(primary: u64, turn_nonce: &str, absorbed: u64, claimed_at: u64) -> MergedAlias {
        MergedAlias {
            primary,
            turn_nonce: turn_nonce.to_owned(),
            absorbed: vec![absorbed],
            claimed_at_epoch_ms: claimed_at,
        }
    }

    fn episode(user_msg_id: u64, turn_nonce: &str, committed_at: u64) -> CompletedTurnEntry {
        CompletedTurnEntry {
            turn_nonce: Some(turn_nonce.to_owned()),
            ..entry(user_msg_id, committed_at)
        }
    }

    const APPEND: LedgerOperation = LedgerOperation::Append { user_msg_id: 7_001 };

    #[test]
    fn ledger_operation_logs_keep_primary_and_issue() {
        let sync_failed = || Err(PublishError::ParentSyncFailed("eio".into()));
        let alias = LedgerOperation::MergedAlias { primary: P };
        let logs =
            capture_logs(|| log_publish_outcome(sync_failed(), &ProviderKind::Codex, 1, alias));
        for field in [
            format!("primary={P}"),
            "operation=\"merged_alias\"".into(),
            "#6035".into(),
        ] {
            assert!(logs.contains(&field), "{field}: {logs}");
        }
        assert!(!logs.contains("user_msg_id"), "{logs}");
        let logs =
            capture_logs(|| log_publish_outcome(sync_failed(), &ProviderKind::Codex, 1, APPEND));
        assert!(
            logs.contains("user_msg_id=7001") && logs.contains("#4564"),
            "{logs}"
        );
    }

    #[test]
    fn unflushed_publication_is_observable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("5166.json");
        let unflushed = append_at_with_sync(&path, 7_001, None, 1_000, |_| Ok(()), false);
        let logs = capture_logs(|| log_publish_outcome(unflushed, &ProviderKind::Codex, 1, APPEND));
        assert!(
            logs.contains("DEBUG") && logs.contains("durability unconfirmed"),
            "{logs}"
        );
        assert!(
            logs.contains("published=true") && logs.contains("7001"),
            "{logs}"
        );
        let flushed = Ok(Publication::Published(ParentSync::Flushed));
        assert!(
            capture_logs(|| log_publish_outcome(flushed, &ProviderKind::Codex, 1, APPEND))
                .is_empty()
        );
    }

    #[test]
    fn parent_sync_observes_published_append_exactly_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("codex/5166.json");
        let mut syncs = 0;
        let result = append_at_with_sync(
            &path,
            7_001,
            Some("episode"),
            1_000,
            |published| {
                syncs += 1;
                assert_eq!(published, path);
                let ledger = read_ledger_at(published);
                assert!(ledger.is_some(), "rename must precede parent sync");
                assert_eq!(
                    ledger.unwrap().entries,
                    vec![episode(7_001, "episode", 1_000)]
                );
                Ok(())
            },
            true,
        )
        .unwrap();
        assert_eq!(syncs, 1, "successful publication must sync its parent once");
        assert_eq!(result, Publication::Published(ParentSync::Flushed));
    }

    #[test]
    fn parent_sync_is_skipped_when_rename_fails() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("5166.json");
        append_at(&path, 7_000, None, 900).unwrap();
        let before = fs::read(&path).unwrap();
        let fault = runtime_store::rename_fault::Armed::new(&path);
        let mut syncs = 0;
        let result = append_at_with_sync(
            &path,
            7_001,
            None,
            1_000,
            |_| {
                syncs += 1;
                Ok(())
            },
            true,
        );
        let error = result.expect_err("rename failure must remain a write failure");
        assert!(matches!(&error, PublishError::WriteFailed(detail) if detail.contains("rename")));
        let logs =
            capture_logs(|| log_publish_outcome(Err(error), &ProviderKind::Codex, 5166, APPEND));
        assert!(logs.contains("ledger write failed"), "{logs}");
        assert!(logs.contains("published=false"), "{logs}");
        assert!(!logs.contains("durability unconfirmed"), "{logs}");
        assert_eq!(syncs, 0, "failed publication must never sync its parent");
        assert_eq!(fault.evidence().0, 1, "one real rename was attempted");
        assert_eq!(fault.evidence().1.len(), 1, "the OS refused the rename");
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn parent_sync_failure_keeps_the_published_entry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("5166.json");
        let mut retry_fault = None;
        let mut syncs = 0;
        let logs = capture_logs(|| {
            let error = append_at_with_sync(
                &path,
                7_001,
                None,
                1_000,
                |published| {
                    syncs += 1;
                    retry_fault = Some(runtime_store::rename_fault::Armed::new(published));
                    Err(std::io::Error::other("injected parent sync failure"))
                },
                true,
            )
            .unwrap_err();
            assert_eq!(
                error,
                PublishError::ParentSyncFailed("injected parent sync failure".into())
            );
            log_publish_outcome(Err(error), &ProviderKind::Codex, 5166, APPEND);
        });
        assert_eq!(syncs, 1);
        assert_eq!(
            retry_fault.unwrap().evidence().0,
            0,
            "a visible entry must never be rewritten after a sync failure"
        );
        assert_eq!(
            read_ledger_at(&path).unwrap().entries,
            vec![entry(7_001, 1_000)]
        );
        assert!(
            logs.contains("best-effort durability unconfirmed"),
            "{logs}"
        );
        assert!(logs.contains("published=true"), "{logs}");
        assert!(logs.contains("operation=\"append\""), "{logs}");
        assert!(!logs.contains("ledger write failed"), "{logs}");
        assert!(!logs.contains("delivery failed"), "{logs}");
        assert!(!logs.contains("settlement evidence unavailable"), "{logs}");
    }

    #[test]
    fn parent_sync_no_flush_is_classified_without_claiming_durability() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("5166.json");
        let mut syncs = 0;
        let result = append_at_with_sync(
            &path,
            7_001,
            None,
            1_000,
            |published| {
                syncs += 1;
                assert_eq!(published, path);
                Ok(())
            },
            false,
        )
        .unwrap();
        assert_eq!(syncs, 1);
        assert_eq!(result, Publication::Published(ParentSync::Unflushed));
        assert_eq!(
            read_ledger_at(&path).unwrap().entries,
            vec![entry(7_001, 1_000)]
        );
    }

    #[test]
    fn append_then_read_roundtrips_the_user_msg_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("codex").join("4564.json");
        let result = append_at(&path, 7_001, None, 1_000).expect("append");
        let expected = if runtime_store::PARENT_DIR_FSYNC_FLUSHES {
            ParentSync::Flushed
        } else {
            ParentSync::Unflushed
        };
        assert_eq!(result, Publication::Published(expected));

        let ledger = read_ledger_at(&path).expect("ledger present");
        assert_eq!(ledger.entries, vec![entry(7_001, 1_000)]);
    }

    #[test]
    fn append_dedups_by_user_msg_id_keeping_latest_commit_time() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("codex").join("4564.json");
        append_at(&path, 7_001, None, 1_000).expect("append");
        append_at(&path, 7_001, None, 2_000).expect("re-append");

        let ledger = read_ledger_at(&path).expect("ledger present");
        assert_eq!(
            ledger.entries,
            vec![entry(7_001, 2_000)],
            "a re-delivered turn must not accumulate duplicate ledger rows"
        );
    }

    #[test]
    fn zero_user_msg_id_is_a_no_op_sentinel() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("codex").join("4564.json");
        assert_eq!(
            append_at(&path, 0, None, 1_000).unwrap(),
            Publication::Skipped
        );
        assert!(
            read_ledger_at(&path).is_none(),
            "a synthetic turn (user_msg_id == 0) must not create a ledger"
        );
    }

    #[test]
    fn prune_drops_entries_past_the_retention_window() {
        let now = 100 * LEDGER_RETENTION_MS;
        let mut entries = vec![
            entry(1, now - LEDGER_RETENTION_MS - 1), // just outside the window
            entry(2, now - 10),                      // inside
        ];
        prune_entries(&mut entries, now);
        assert_eq!(entries, vec![entry(2, now - 10)]);
    }

    #[test]
    fn prune_caps_to_the_newest_entries() {
        let mut entries: Vec<CompletedTurnEntry> = (0..(LEDGER_ENTRY_CAP as u64 + 5))
            .map(|i| entry(i, i))
            .collect();
        prune_entries(&mut entries, LEDGER_ENTRY_CAP as u64 + 5);
        assert_eq!(entries.len(), LEDGER_ENTRY_CAP);
        assert_eq!(
            entries.first().map(|e| e.user_msg_id),
            Some(5),
            "the 5 oldest entries are dropped from the front"
        );
    }

    #[test]
    fn settled_user_msg_ids_of_absent_ledger_is_empty() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("codex").join("absent.json");
        assert!(read_ledger_at(&path).is_none());
    }

    const P: u64 = 6_035_200;
    const H: u64 = 6_035_100;

    fn settled_at(path: &Path) -> HashSet<u64> {
        read_ledger_at(path).expect("ledger").settled_ids()
    }

    #[test]
    fn alias_settles_only_with_the_same_episode_entry_6035() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("codex").join("6035.json");
        record_alias_at(&path, P, "n2", &[H, P, 0], 1_000).expect("alias");
        assert_eq!(
            read_ledger_at(&path).unwrap().merged_aliases,
            vec![alias(P, "n2", H, 1_000)]
        );
        assert!(
            !settled_at(&path).contains(&H),
            "a rowless alias is no evidence"
        );
        // A delayed append of P's earlier episode cannot certify n2.
        append_at(&path, P, Some("n1"), 2_000).expect("delayed n1");
        assert!(!settled_at(&path).contains(&H));
        append_at(&path, P, Some("n2"), 3_000).expect("n2 delivered");
        assert_eq!(settled_at(&path), HashSet::from([P, H]));
        assert_eq!(
            read_ledger_at(&path).unwrap().entries.len(),
            2,
            "(id, nonce) dedup key"
        );
    }

    #[test]
    fn none_nonce_never_joins_t_s4_6035() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("codex").join("6035.json");
        // Legacy/unknown-provenance rows, including an old binary's JSON shape.
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(r#"{{"entries":[{{"user_msg_id":{P},"committed_at_epoch_ms":1}}]}}"#),
        )
        .unwrap();
        assert_eq!(read_ledger_at(&path).unwrap().entries, vec![entry(P, 1)]);
        record_alias_at(&path, P, "n", &[H], 2).expect("alias");
        append_at(&path, P, Some(""), 3).expect("empty nonce is None");
        assert!(
            !settled_at(&path).contains(&H),
            "None must not join an alias"
        );
        record_alias_at(&path, P, "", &[H], 4).expect("nonce-less claim");
        assert_eq!(
            read_ledger_at(&path).unwrap().merged_aliases.len(),
            1,
            "no alias without a nonce"
        );
    }

    #[test]
    fn active_rowless_alias_survives_row_backed_pool_t_s5_6035() {
        let now = 10 * LEDGER_RETENTION_MS;
        let mut ledger = CompletedTurnLedger::default();
        for i in 0..LEDGER_ENTRY_CAP as u64 {
            ledger.entries.push(episode(i + 1, "n", now - 1_000 + i));
            ledger
                .merged_aliases
                .push(alias(i + 1, "n", 10_000 + i, now - 1_000 + i));
        }
        ledger.merged_aliases.insert(0, alias(P, "active", H, now));
        prune_ledger(&mut ledger, now);
        assert_eq!(ledger.merged_aliases.len(), LEDGER_ENTRY_CAP + 1);
        assert!(
            ledger.merged_aliases.contains(&alias(P, "active", H, now)),
            "the active alias is kept"
        );
    }

    #[test]
    fn prune_only_removes_evidence_t_s6_6035() {
        let now = 10 * LEDGER_RETENTION_MS;
        let mut ledger = CompletedTurnLedger::default();
        for i in 0..=ROWLESS_ALIAS_CAP as u64 {
            ledger
                .merged_aliases
                .push(alias(100 + i, "n", 1_000 + i, now - 100 + i));
        }
        // A row-backed alias whose row is past retention: it dies with that row.
        ledger
            .entries
            .push(episode(P, "old", now - LEDGER_RETENTION_MS - 1));
        ledger.merged_aliases.push(alias(P, "old", H, now - 50));
        let before = ledger.settled_ids();
        prune_ledger(&mut ledger, now);
        assert!(
            ledger.settled_ids().is_subset(&before),
            "pruning can only remove evidence"
        );
        let primaries: Vec<u64> = ledger.merged_aliases.iter().map(|a| a.primary).collect();
        assert_eq!(
            primaries,
            (101..=100 + ROWLESS_ALIAS_CAP as u64).collect::<Vec<_>>()
        );
    }

    #[test]
    fn active_rowless_alias_survives_a_backwards_clock_at_cap_6035() {
        let now = 10 * LEDGER_RETENTION_MS;
        let mut ledger = CompletedTurnLedger::default();
        for i in 0..ROWLESS_ALIAS_CAP as u64 {
            ledger
                .merged_aliases
                .push(alias(100 + i, "n", 1_000 + i, now - 100 + i));
        }
        ledger
            .merged_aliases
            .push(alias(P, "active", H, now - 10_000));
        prune_ledger(&mut ledger, now);
        let primaries: Vec<u64> = ledger.merged_aliases.iter().map(|a| a.primary).collect();
        let expected: Vec<u64> = (101..100 + ROWLESS_ALIAS_CAP as u64).chain([P]).collect();
        assert_eq!(primaries, expected, "first-inserted evicted, active kept");
    }

    #[test]
    fn an_absorbed_id_is_dated_only_by_its_exact_backing_episode() {
        let mut ledger = CompletedTurnLedger {
            entries: vec![episode(P, "n", 2_000), episode(P, "later", 9_000)],
            merged_aliases: vec![alias(P, "n", H, 1_000), alias(P, "undelivered", 7, 1_000)],
        };
        let commits = ledger.settled_commit_ms();
        assert_eq!(commits, HashMap::from([(P, 9_000), (H, 2_000)]));
        assert_eq!(ledger.settled_ids(), commits.into_keys().collect());
        ledger.entries.insert(0, entry(H, 3_000));
        assert_eq!(
            ledger.settled_commit_ms()[&H],
            3_000,
            "the later of the two proofs"
        );
        ledger.entries[0].committed_at_epoch_ms = 1_500;
        assert_eq!(ledger.settled_commit_ms()[&H], 2_000);
    }

    #[test]
    fn a_long_active_rowless_alias_survives_an_unrelated_append() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("codex").join("active.json");
        record_alias_at(&path, P, "active", &[H], 1_000).expect("alias");
        append_at(&path, 7_001, None, 1_000 + LEDGER_RETENTION_MS + 1).expect("append");
        let aliases = read_ledger_at(&path).expect("ledger").merged_aliases;
        assert_eq!(aliases, vec![alias(P, "active", H, 1_000)]);
    }
}
