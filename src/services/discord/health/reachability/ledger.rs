//! The durable obligation ledger — 4987 S1 second half (#5071 T4-B2). One
//! JSON file per `(provider, channel_id)` holds the bound incarnation, the
//! byte cursor, and the obligations seen and not yet retired — on disk
//! because 4987 §2.2's "should have been delivered" term must outlive the
//! process, or every restart re-reads a fresh zero.
//!
//! Storage follows [`super::super::super::outbound::completed_turn_ledger`]
//! exactly: a dedicated `runtime/` subtree, `delivery_record::lock_record_path`
//! for the flock, `runtime_store::atomic_write` for the write. **No new lock
//! mechanism and no migration** (4987 §5.2). It is a host-local sidecar, NOT
//! a cluster authority (4987 §-1.5).
//!
//! An obligation leaves only through a named [`ObligationExtinction`] (4987
//! §-1.5 I13) — frontier/cursor advance, structural liveness, and grace
//! expiry are NOT extinction reasons (the #4986 형상1 conflation). This slice
//! only produces `IncarnationRetired` and `ClassifiedDrop{Capacity}`;
//! `ReceiptCovered` has no producer here (the receipt index is T4-B3), so a
//! live obligation means "not yet subtracted", never "undelivered" — see
//! [`ReachabilityLedger::live_obligations`]. [`LEDGER_OBLIGATION_CAP`] bounds
//! otherwise-unbounded growth as a typed `ClassifiedDrop`, never a silent truncation.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::discovery::TranscriptFileId;
use super::obligation::CanonicalRecord;
use crate::services::discord::outbound::delivery_record;
use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

/// Sidecar subtree, kept out of the old-binary reaper's scan set.
const REACHABILITY_LEDGER_DIR: &str = "discord_reachability_ledger";

/// Schema version of the on-disk file. A different version is rejected, not
/// migrated; mutation entry points error rather than replace it with an
/// empty claim.
const LEDGER_SCHEMA_VERSION: u32 = 1;

/// Maximum live obligations retained per channel, sized against the per-tick
/// read cap [`super::tail::TAIL_READ_CAP_BYTES`] rather than a delivery
/// rate: overflow means the channel has been unsubtracted for many ticks.
const LEDGER_OBLIGATION_CAP: usize = 4_096;

/// The incarnation a ledger is bound to — 4987 §-1.3's `IncarnationRange`
/// minus the byte range, which the individual obligations carry. A `None`
/// `spawn_nonce` is never treated as a match for a `Some` (4987 §-1.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct LedgerIncarnation {
    pub tmux_session_name: String,
    pub generation_mtime_ns: i64,
    #[serde(default)]
    pub spawn_nonce: Option<String>,
    pub transcript_dev: u64,
    pub transcript_ino: u64,
}

impl LedgerIncarnation {
    pub(in crate::services::discord) fn new(
        tmux_session_name: String,
        generation_mtime_ns: i64,
        spawn_nonce: Option<String>,
        identity: TranscriptFileId,
    ) -> Self {
        Self {
            tmux_session_name,
            generation_mtime_ns,
            spawn_nonce,
            transcript_dev: identity.dev,
            transcript_ino: identity.ino,
        }
    }

    pub(in crate::services::discord) fn identity(&self) -> TranscriptFileId {
        TranscriptFileId {
            dev: self.transcript_dev,
            ino: self.transcript_ino,
        }
    }
}

/// One unsatisfied obligation: the byte range and when it was first seen.
/// The timestamp is raw rather than pre-bucketed because 4987 §3.4 makes the
/// age histogram the OUTPUT of the 30-day observation, and pre-bucketing
/// would bake in the very thresholds §10 lists as NO-GO for this slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct LedgerObligation {
    pub start: u64,
    pub end: u64,
    pub first_observed_at_epoch_ms: u64,
}

/// Why an obligation left the ledger — 4987 §-1.5 I13's typed extinction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) enum ObligationExtinction {
    /// A confirmed AND committed receipt covered the range — the only reason that may promote health to `Reachable`. **No producer in T4-B2** (T4-B3).
    ReceiptCovered,
    /// Closed for a named non-delivery reason; never counted as delivery.
    ClassifiedDrop { reason: ClassifiedDropReason },
    /// The incarnation is gone, so its byte offsets name nothing — not evidence it was delivered.
    IncarnationRetired,
}

/// The named reasons a `ClassifiedDrop` can carry in this slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) enum ClassifiedDropReason {
    /// The bounded ring overflowed and the oldest entry was evicted.
    LedgerCapacity,
}

/// Monotone observation counters — the 30-day record 4987 §3.4 asks for;
/// nothing branches on them in this slice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct LedgerCounters {
    /// Ticks that reached the ledger for this channel.
    #[serde(default)]
    pub ticks: u64,
    /// Every obligation ever appended.
    #[serde(default)]
    pub total_obligations: u64,
    /// Obligations retired by each typed reason.
    #[serde(default)]
    pub retired_receipt_covered: u64,
    #[serde(default)]
    pub retired_classified_drop: u64,
    #[serde(default)]
    pub retired_incarnation: u64,
    /// Ticks whose tail read did not see a whole record.
    #[serde(default)]
    pub incomplete_observations: u64,
}

/// The per-channel durable ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct ReachabilityLedger {
    pub schema_version: u32,
    pub incarnation: LedgerIncarnation,
    /// Where the tail cursor resumes.
    pub cursor_offset: u64,
    /// Where this incarnation's observation STARTED; bytes before it were never read, so their absence from `obligations` is evidence of nothing.
    pub bootstrap_offset: u64,
    /// Transcript length at the previous tick — the "file is advancing" half of 4987 §-1.4's positive incarnation-alive evidence.
    pub last_observed_len: u64,
    pub obligations: Vec<LedgerObligation>,
    #[serde(default)]
    pub counters: LedgerCounters,
}

impl ReachabilityLedger {
    /// A fresh ledger bootstrapped at `bootstrap_offset` for `incarnation`.
    pub(in crate::services::discord) fn bootstrap(
        incarnation: LedgerIncarnation,
        bootstrap_offset: u64,
        counters: LedgerCounters,
    ) -> Self {
        Self {
            schema_version: LEDGER_SCHEMA_VERSION,
            incarnation,
            cursor_offset: bootstrap_offset,
            bootstrap_offset,
            last_observed_len: bootstrap_offset,
            obligations: Vec::new(),
            counters,
        }
    }

    /// The obligations this ledger currently holds. In T4-B2 this means
    /// "observed and not yet subtracted", NOT "undelivered" — the receipt
    /// index lands in T4-B3, so a non-empty result must not be reported as
    /// a delivery failure.
    pub(in crate::services::discord) fn live_obligations(&self) -> &[LedgerObligation] {
        &self.obligations
    }

    /// Append this tick's obligations, evicting the oldest as a typed
    /// `ClassifiedDrop` when the bounded ring overflows, and return the
    /// extinctions performed. Non-obligation records
    /// (`reason.is_obligation() == false`) are filtered out without
    /// changing the live set or monotone counters.
    pub(in crate::services::discord) fn append_obligations(
        &mut self,
        records: impl IntoIterator<Item = CanonicalRecord>,
        observed_at_epoch_ms: u64,
    ) -> Vec<ObligationExtinction> {
        for record in records {
            if !record.reason.is_obligation() {
                continue;
            }
            self.obligations.push(LedgerObligation {
                start: record.start,
                end: record.end,
                first_observed_at_epoch_ms: observed_at_epoch_ms,
            });
            self.counters.total_obligations = self.counters.total_obligations.saturating_add(1);
        }

        let mut extinctions = Vec::new();
        if self.obligations.len() > LEDGER_OBLIGATION_CAP {
            let overflow = self.obligations.len() - LEDGER_OBLIGATION_CAP;
            self.obligations.drain(0..overflow);
            self.counters.retired_classified_drop = self
                .counters
                .retired_classified_drop
                .saturating_add(overflow as u64);
            for _ in 0..overflow {
                extinctions.push(ObligationExtinction::ClassifiedDrop {
                    reason: ClassifiedDropReason::LedgerCapacity,
                });
            }
        }
        extinctions
    }

    /// Whether the stored incarnation is the one just resolved; a `None` spawn nonce matches only another `None`.
    pub(in crate::services::discord) fn binds_to(&self, incarnation: &LedgerIncarnation) -> bool {
        self.schema_version == LEDGER_SCHEMA_VERSION && &self.incarnation == incarnation
    }

    /// Retire everything for a superseded incarnation and re-bootstrap, keeping counters so the 30-day record survives a rotation.
    pub(in crate::services::discord) fn retire_and_rebootstrap(
        &self,
        incarnation: LedgerIncarnation,
        bootstrap_offset: u64,
    ) -> Self {
        let mut counters = self.counters.clone();
        counters.retired_incarnation = counters
            .retired_incarnation
            .saturating_add(self.obligations.len() as u64);
        Self::bootstrap(incarnation, bootstrap_offset, counters)
    }
}

fn ledger_root() -> Option<PathBuf> {
    runtime_store::runtime_root().map(|root| root.join(REACHABILITY_LEDGER_DIR))
}

/// `<runtime_root>/discord_reachability_ledger/<provider>/<channel_id>.json`.
pub(in crate::services::discord) fn ledger_path(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<PathBuf> {
    ledger_root().map(|root| {
        root.join(provider.as_str())
            .join(format!("{channel_id}.json"))
    })
}

/// Conservative read: missing, unreadable or malformed all read as `None`.
/// 4987 §-1.4 counterexample 7: a malformed ledger must become
/// `Unknown{ReceiptStoreUnreadable}` at the caller, never an empty
/// obligation set that would look like `Reachable`.
pub(in crate::services::discord) fn read_ledger_at(path: &Path) -> Option<ReachabilityLedger> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str::<ReachabilityLedger>(&content)
        .ok()
        .filter(|ledger| ledger.schema_version == LEDGER_SCHEMA_VERSION)
}

/// Whether the file exists at all — distinguishes "no ledger yet" from "a
/// ledger that would not parse", which [`read_ledger_at`]'s `None` merges.
pub(in crate::services::discord) fn ledger_file_exists(path: &Path) -> bool {
    path.is_file()
}

/// flock-guarded atomic write of the whole ledger. Overwrites with exactly
/// the supplied in-memory snapshot; does not merge another writer's view.
/// **For read-modify-write transactions, use [`append_ledger_at`] or
/// [`retire_ledger_at`] instead** — direct use risks lost updates unless the
/// caller holds a lock over the entire sequence. Only
/// [`bootstrap_ledger_at`] creates a ledger. The atomic rename means
/// concurrent lock-free readers always see either the old or new file, never
/// a partial write.
pub(in crate::services::discord) fn write_ledger_at(
    path: &Path,
    ledger: &ReachabilityLedger,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let _lock = delivery_record::lock_record_path(path)?;
    read_bootstrapped_ledger_at(path)?;
    let data = serde_json::to_string_pretty(ledger).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(path, &data)
}

fn read_bootstrapped_ledger_at(path: &Path) -> Result<ReachabilityLedger, String> {
    if !ledger_file_exists(path) {
        return Err("ledger not bootstrapped".to_string());
    }
    read_ledger_at(path).ok_or_else(|| "ledger unreadable or schema incompatible".to_string())
}

/// flock-guarded explicit ledger bootstrap. A missing ledger is created at
/// `bootstrap_offset`; a ledger already bound to `incarnation` is left
/// as-is; a different valid incarnation is retired and re-bootstrapped so
/// its counters survive. An unreadable or schema-incompatible file returns
/// an error rather than being replaced with an empty observation claim.
pub(in crate::services::discord) fn bootstrap_ledger_at(
    path: &Path,
    incarnation: LedgerIncarnation,
    bootstrap_offset: u64,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let _lock = delivery_record::lock_record_path(path)?;
    let ledger = if ledger_file_exists(path) {
        let current = read_bootstrapped_ledger_at(path)?;
        if current.binds_to(&incarnation) {
            return Ok(());
        }
        current.retire_and_rebootstrap(incarnation, bootstrap_offset)
    } else {
        ReachabilityLedger::bootstrap(incarnation, bootstrap_offset, LedgerCounters::default())
    };
    let data = serde_json::to_string_pretty(&ledger).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(path, &data)
}

/// Re-bootstrap an existing ledger only while the caller's watcher snapshot
/// is still the live incarnation. Returns `Ok(false)` without changing the
/// ledger when revalidation rejects a stale caller — the callback runs
/// while the ledger file lock is held so a concurrent observation cannot
/// retire the newly selected incarnation before this transition commits.
pub(in crate::services::discord) fn rebootstrap_ledger_at_if_snapshot_current<F>(
    path: &Path,
    incarnation: LedgerIncarnation,
    bootstrap_offset: u64,
    revalidate_live_incarnation: F,
) -> Result<bool, String>
where
    F: FnOnce() -> Option<LedgerIncarnation>,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let _lock = delivery_record::lock_record_path(path)?;
    let current = read_bootstrapped_ledger_at(path)?;
    if current.binds_to(&incarnation) {
        return Ok(true);
    }

    // The lock serializes only; authority for this destructive transition
    // comes from revalidating the live watcher incarnation while held.
    if revalidate_live_incarnation().as_ref() != Some(&incarnation) {
        return Ok(false);
    }

    let ledger = current.retire_and_rebootstrap(incarnation, bootstrap_offset);
    let data = serde_json::to_string_pretty(&ledger).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(path, &data)?;
    Ok(true)
}

/// flock-guarded read-modify-write: append obligations to the durable
/// ledger, evicting oldest when capped, and atomically persist. Lock is held
/// for the entire read → append → write sequence, serializing against
/// concurrent writers.
pub(in crate::services::discord) fn append_ledger_at(
    path: &Path,
    records: impl IntoIterator<Item = CanonicalRecord>,
    observed_at_epoch_ms: u64,
) -> Result<Vec<ObligationExtinction>, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let _lock = delivery_record::lock_record_path(path)?;
    let mut ledger = read_bootstrapped_ledger_at(path)?;
    let extinctions = ledger.append_obligations(records, observed_at_epoch_ms);
    let data = serde_json::to_string_pretty(&ledger).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(path, &data)?;
    Ok(extinctions)
}

/// Telemetry only: neither the appended count nor an extinction authorizes
/// a relay or recovery decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct ObservationCommit {
    pub obligations_appended: usize,
    pub extinctions: Vec<ObligationExtinction>,
}

/// Persist one framed transcript observation and its resume cursor
/// together. The lock revalidates both incarnation and cursor because a
/// tick may have read while another process committed first; every record
/// must also carry the same transcript identity and generation as the
/// ledger incarnation. Ordering is indivisible: obligations are appended in
/// memory, then the cursor advances in that same JSON snapshot, and one
/// atomic rename publishes both — no interval where only one side is
/// durable, so a crash never double-counts or silently drops one.
pub(in crate::services::discord) fn record_observation_at(
    path: &Path,
    incarnation: &LedgerIncarnation,
    expected_cursor: u64,
    records: impl IntoIterator<Item = CanonicalRecord>,
    next_offset: u64,
    observed_len: u64,
    observation_incomplete: bool,
    observed_at_epoch_ms: u64,
) -> Result<ObservationCommit, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let _lock = delivery_record::lock_record_path(path)?;
    let mut ledger = read_bootstrapped_ledger_at(path)?;
    if !ledger.binds_to(incarnation) {
        return Err("ledger incarnation changed during observation".to_string());
    }
    if ledger.cursor_offset != expected_cursor {
        return Err("ledger cursor changed during observation".to_string());
    }
    if next_offset < expected_cursor || next_offset > observed_len {
        return Err("observation cursor is outside the observed transcript".to_string());
    }

    let records: Vec<_> = records.into_iter().collect();
    if records.iter().any(|record| {
        record.generation_mtime_ns != incarnation.generation_mtime_ns
            || record.identity != incarnation.identity()
            || record.start < expected_cursor
            || record.end < record.start
            || record.end > observed_len
    }) {
        return Err("observation record does not bind to the ledger incarnation".to_string());
    }

    let before = ledger.counters.total_obligations;
    let extinctions = ledger.append_obligations(records, observed_at_epoch_ms);
    ledger.cursor_offset = next_offset;
    ledger.last_observed_len = observed_len;
    ledger.counters.ticks = ledger.counters.ticks.saturating_add(1);
    if observation_incomplete {
        ledger.counters.incomplete_observations =
            ledger.counters.incomplete_observations.saturating_add(1);
    }
    let obligations_appended = ledger.counters.total_obligations.saturating_sub(before) as usize;
    let data = serde_json::to_string_pretty(&ledger).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(path, &data)?;
    Ok(ObservationCommit {
        obligations_appended,
        extinctions,
    })
}

/// flock-guarded read-modify-write: retire the current incarnation and
/// rebootstrap for a new one. Lock is held for the entire read → retire →
/// write sequence, serializing against concurrent writers.
pub(in crate::services::discord) fn retire_ledger_at(
    path: &Path,
    incarnation: LedgerIncarnation,
    bootstrap_offset: u64,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let _lock = delivery_record::lock_record_path(path)?;
    let ledger = read_bootstrapped_ledger_at(path)?;
    let rebootstrapped = ledger.retire_and_rebootstrap(incarnation, bootstrap_offset);
    let data = serde_json::to_string_pretty(&rebootstrapped).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(path, &data)
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
