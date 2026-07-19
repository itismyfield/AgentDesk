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

use std::collections::HashSet;
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

/// One completed turn: the inbound `user_msg_id` and when its terminal delivery
/// committed. `committed_at_epoch_ms` drives the retention prune.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct CompletedTurnEntry {
    pub user_msg_id: u64,
    pub committed_at_epoch_ms: u64,
}

/// The durable per-channel ledger — a bounded ring of completed turns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct CompletedTurnLedger {
    #[serde(default)]
    pub entries: Vec<CompletedTurnEntry>,
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
pub(in crate::services::discord) fn settled_user_msg_ids(
    provider: &ProviderKind,
    channel_id: u64,
) -> HashSet<u64> {
    read_ledger(provider, channel_id)
        .map(|ledger| {
            ledger
                .entries
                .into_iter()
                .map(|entry| entry.user_msg_id)
                .collect()
        })
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

/// flock-guarded read-modify-write append. Dedups by `user_msg_id` (keeps the
/// latest commit time), prunes lazily, and atomically rewrites. A `user_msg_id`
/// of `0` (synthetic/no-inbound-message turn) is a no-op sentinel — there is no
/// catch-up message to suppress, so nothing is recorded.
fn append_at(path: &Path, user_msg_id: u64, committed_at_epoch_ms: u64) -> Result<(), String> {
    if user_msg_id == 0 {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _lock = delivery_record::lock_record_path(path)?;
    let mut ledger = read_ledger_at(path).unwrap_or_default();
    ledger
        .entries
        .retain(|entry| entry.user_msg_id != user_msg_id);
    ledger.entries.push(CompletedTurnEntry {
        user_msg_id,
        committed_at_epoch_ms,
    });
    prune_entries(&mut ledger.entries, committed_at_epoch_ms);
    let data = serde_json::to_string_pretty(&ledger).map_err(|e| e.to_string())?;
    runtime_store::atomic_write(path, &data)
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
    if let Err(error) = append_at(&path, user_msg_id, now_epoch_ms()) {
        tracing::warn!(
            provider = provider.as_str(),
            channel_id,
            user_msg_id,
            error = %error,
            "#4564 completed-turn ledger append failed (best-effort; falls through to TooOld/DLQ)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(user_msg_id: u64, committed_at_epoch_ms: u64) -> CompletedTurnEntry {
        CompletedTurnEntry {
            user_msg_id,
            committed_at_epoch_ms,
        }
    }

    #[test]
    fn append_then_read_roundtrips_the_user_msg_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("codex").join("4564.json");
        append_at(&path, 7_001, 1_000).expect("append");

        let ledger = read_ledger_at(&path).expect("ledger present");
        assert_eq!(ledger.entries, vec![entry(7_001, 1_000)]);
    }

    #[test]
    fn append_dedups_by_user_msg_id_keeping_latest_commit_time() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("codex").join("4564.json");
        append_at(&path, 7_001, 1_000).expect("append");
        append_at(&path, 7_001, 2_000).expect("re-append");

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
        append_at(&path, 0, 1_000).expect("sentinel append is a no-op");
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
}
