//! #3089 Phase B0 — durable delivery-record sidecar store (mixed-binary safe).
//! Phase B gives turn-output delivery a **durable** authority that survives a
//! dcserver restart (AC6). B0 resolves design §4.4's hard blocker:
//! **mixed-binary safety**.
//!
//! ## Why a sidecar (not new `inflight.rs` fields)
//!
//! Old binaries deserialize `InflightTurnState`, then reserialize it with
//! `serde_json::to_string_pretty` (`inflight.rs`), **dropping unknown fields**;
//! standby rewrites the same `{channel_id}.json` on its heartbeat. So a
//! `#[serde(default)]` field added on a new primary would be silently erased by
//! an older standby — `#[serde(default)]` + tolerant deserialize is NOT enough
//! (design §4.4 / risk #4). The resolution (option a, the default) is a
//! **sidecar store old binaries never read or scan**.
//!
//! ## The isolation guarantee (the entire B0 deliverable)
//!
//! Old binaries enumerate the inflight provider dir and **delete malformed**
//! entries: `load_inflight_states_from_root` (`inflight.rs:2577`) does a
//! NON-recursive `fs::read_dir(runtime/discord_inflight/<provider>/)` and, for
//! every entry whose `extension == "json"`, parses it and `fs::remove_file`s the
//! malformed ones (`inflight.rs:2587`+). The two other inflight enumerations
//! (`:800` sweep, `:2251` clear-by-tmux) apply the same `"json"` filter on the
//! same dir.
//!
//! The delivery record store therefore lives in a **dedicated sibling subtree**
//! `runtime/discord_delivery_records/<provider>/<channel_id>.json` — a peer of
//! `runtime/discord_inflight/`, matching the existing sidecar-root convention in
//! `runtime_store.rs` (`discord_status_panel_orphans`,
//! `discord_queued_placeholders`, …). Because the reaper's `read_dir` is
//! non-recursive and only ever opens `discord_inflight/<provider>/`, it never
//! descends into `discord_delivery_records/`. The delivery-channel -> owner-channel
//! context added by #3751 lives in a **separate** sibling subtree,
//! `runtime/discord_delivery_owner_context/<provider>/<channel_id>.json`, because
//! older binaries can still rewrite the known delivery-record JSON object and drop
//! unknown fields. The directory — not the `.json` extension — is the isolator
//! (`sidecar_path_is_outside_inflight_scan_set` pins this). No other component
//! (standby heartbeat rewrites only `{channel_id}.json` via
//! `inflight::save_inflight_state`; the sweeper / recovery read via the inflight
//! loaders) knows these paths exist.
//!
//! ## B0 is a pure add (no production caller)
//!
//! Like the A1 controller skeleton, B0 ships the types + the store API fully
//! implemented and tested but wired by NOBODY — B1 adds the shadow-write, B2
//! flips read-authority, B3 hydrates on restart. So every public item is
//! `#[allow(dead_code)]` until B1, and the store is behaviorally a no-op in this
//! PR.
//!
//! ## Conservatism (invariants I2 / I3)
//!
//! - `read_record` returns `None` on a missing OR malformed file — never
//!   "assume delivered" (I3). A B1 reader treats `None` as not-yet-delivered.
//! - Only [`write_delivered_frontier`] advances `delivered_frontier` — a lease
//!   acquire/clear never touches it, so an ambiguous (`Unknown`/`NotDelivered`)
//!   commit never advances the durable offset (I2).
//! - `clear_lease` (release) clears `delivery_lease` ONLY; `delivered_frontier`
//!   survives (design §4.3).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use poise::serenity_prelude::ChannelId;
use serde::{Deserialize, Serialize};

use super::completed_turn_ledger;
use super::turn_output_controller::DeliveryOutcome;
use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

/// Sidecar subtree name — a sibling of `discord_inflight/` under `runtime/`.
/// MUST NOT be `discord_inflight` (that dir is the old-binary reaper's scan
/// target); the isolation proof test asserts this segment differs.
const DELIVERY_RECORDS_DIR: &str = "discord_delivery_records";
const FRESH_SEND_RECORDS_DIR: &str = "discord_fresh_send_records";
const DELIVERY_OWNER_CONTEXT_DIR: &str = "discord_delivery_owner_context";
const RECENT_DELIVERED_CONTENT_LIMIT: usize = 16;
const RECENT_DELIVERED_CONTENT_WINDOW_MS: u64 = 15 * 60 * 1000;
const CONFIRMED_DELIVERY_RECEIPT_LIMIT: usize = 32;

/// Durable per-turn delivery record (design §4.3). Two **independent** durable
/// fields, deliberately not folded into one state machine: the lease is the
/// transient in-flight claim (cleared on release), the frontier is the
/// release-surviving delivered offset (only a `Delivered` outcome writes it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct DeliveryRecord {
    /// Live/in-flight claim. CLEARED on release (`clear_lease`); a leftover
    /// value after restart is the in-flight state B3 reconciles.
    #[serde(default)]
    pub delivery_lease: Option<DurableLease>,
    /// Durable mirror of `confirmed_end_offset`. SURVIVES release; only a
    /// `Delivered` outcome writes it (I2). Hydrates `confirmed_end_offset` on
    /// restart in B3 (no 0-reset).
    #[serde(default)]
    pub delivered_frontier: Option<DeliveredCommit>,
    /// Bounded byte-content fingerprints for degenerate lease fallback dedup.
    #[serde(default)]
    pub recent_delivered_contents: Vec<DeliveredContentFingerprint>,
    /// Exact confirmed-delivery proofs. Unlike the monotonic frontier, these
    /// retain individual turn/range identity so a later turn advancing the
    /// frontier cannot erase the proof needed by a stale restored watcher seed.
    ///
    /// `#[serde(default)]` is deliberately fail-safe for mixed binaries: a
    /// legacy record has no receipts and therefore can never authorize seed
    /// removal. Older binaries may drop this unknown field when rewriting the
    /// sidecar; that loses dedup authority (and preserves the seed) rather than
    /// creating false delivery authority.
    #[serde(default)]
    pub confirmed_deliveries: Vec<ConfirmedDeliveryReceipt>,
}

/// Immutable JSONL coordinates shared by confirmed terminal receipts and the
/// restored-watcher seed adapter. Phase B can reuse this exact source identity
/// for subordinate assistant-text segment receipts without inventing another
/// turn/incarnation coordinate system.
///
/// Every field is serde-defaulted so partially populated newer records remain
/// readable by mixed binaries. [`ExactJsonlSourceIdentity::is_authoritative`]
/// rejects every default/missing component; compatibility must never become a
/// false delivered proof.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct ExactJsonlSourceIdentity {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub tmux_session_name: String,
    #[serde(default)]
    pub turn_nonce: String,
    #[serde(default)]
    pub range: (u64, u64),
    #[serde(default)]
    pub generation_mtime_ns: i64,
    #[serde(default)]
    pub offset_authority_channel_id: u64,
    #[serde(default)]
    pub delivery_channel_id: u64,
}

impl ExactJsonlSourceIdentity {
    pub(in crate::services::discord) fn is_authoritative(&self) -> bool {
        ProviderKind::from_str(&self.provider).is_some()
            && !self.tmux_session_name.is_empty()
            && !self.turn_nonce.is_empty()
            && self.range.1 > self.range.0
            && self.generation_mtime_ns != 0
            && self.offset_authority_channel_id != 0
            && self.delivery_channel_id != 0
    }
}

/// A confirmed Discord delivery bound to an exact immutable JSONL source.
/// `delivery_channel_id` and `message_id` identify what the user actually saw;
/// the source's `offset_authority_channel_id` identifies the record key. Those
/// channels intentionally differ for reused-watcher bridge handoffs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct ConfirmedDeliveryReceipt {
    #[serde(default)]
    pub source: ExactJsonlSourceIdentity,
    #[serde(default)]
    pub delivery_channel_id: u64,
    #[serde(default)]
    pub message_id: u64,
}

impl ConfirmedDeliveryReceipt {
    fn is_authoritative(&self) -> bool {
        self.source.is_authoritative()
            && self.delivery_channel_id == self.source.delivery_channel_id
            && self.message_id != 0
    }
}

/// The offset-authority channel for a delivery channel and tmux generation.
///
/// Stored under the DELIVERY channel in its own sidecar subtree, while the
/// delivered frontier itself is stored under `watcher_owner_channel_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct WatcherOwnerContext {
    pub watcher_owner_channel_id: u64,
    pub tmux_session_name: String,
    pub generation_mtime_ns: i64,
}

/// The transient lease (design §4.3). `deadline_epoch_ms` is ABSOLUTE
/// wall-clock (review r2 H): the in-memory `lease_now_ms()` is process-monotonic
/// and meaningless after a restart. `holder_generation` distinguishes a
/// pre-restart holder (reclaim immediately) from a live one.
#[allow(dead_code)] // #3089 B0: constructed by B1's lease writer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct DurableLease {
    /// Stable serialized holder id. The in-memory `LeaseHolder` enum
    /// (Watcher/Sink/Bridge) maps to this in B1's writer — kept a plain `u64`
    /// here so the durable record carries no cross-module / serde-fragile dep.
    pub holder_id: u64,
    pub attempt_id: u64,
    pub range: (u64, u64),
    pub deadline_epoch_ms: u64,
    pub holder_generation: i64,
}

/// The release-surviving delivered frontier (design §4.3) — the durable mirror
/// of `confirmed_end_offset`. Written only after a confirmed Discord POST and
/// the identity-gated inline advance (I1), never the removed pre-sink Part(a)
/// write. `generation_mtime_ns` guards the #1270 rotation-vs-respawn watermark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct DeliveredCommit {
    pub range: (u64, u64),
    pub generation_mtime_ns: i64,
    pub attempts: u32,
    /// #3610 (Phase B PR-1): the durable TERMINAL ANCHOR — the Discord message id
    /// terminal-replace edited in place (the assistant response `current_msg_id`).
    pub panel_msg_id: Option<u64>,
    /// #3610 (Phase B PR-1b): the channel `panel_msg_id` lives in. The frontier is
    /// KEYED by the offset-authority channel (`watcher_owner_channel_id`), which the
    /// bridge cutover can resolve DIFFERENTLY from the edit-target channel the anchor
    /// message belongs to (a recovered/reused-watcher bridge edits its own dispatch
    /// channel while leasing on the resolved owner channel — terminal_controller_cutover
    /// `Channel split`). PR-2's re-post must therefore know WHICH channel to edit, not
    /// just which message. `#[serde(default)]` keeps PR-1 records (no field) and any
    /// pre-#3610 record forward/backward compatible. Same-channel callers (sink,
    /// watcher) set this to their single channel; null = no anchor recorded.
    #[serde(default)]
    pub panel_channel_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct DeliveredContentFingerprint {
    pub channel_id: u64,
    pub content_hash: String,
    pub content_len: u64,
    pub generation_mtime_ns: i64,
    pub delivered_at_epoch_ms: u64,
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// `runtime/discord_delivery_records/` — the sidecar root, a sibling of
/// `discord_inflight/`. `None` when the runtime root is unavailable.
fn delivery_records_root() -> Option<PathBuf> {
    runtime_store::runtime_root().map(|root| root.join(DELIVERY_RECORDS_DIR))
}

fn fresh_send_records_root() -> Option<PathBuf> {
    runtime_store::runtime_root().map(|root| root.join(FRESH_SEND_RECORDS_DIR))
}

fn delivery_owner_context_root() -> Option<PathBuf> {
    runtime_store::runtime_root().map(|root| root.join(DELIVERY_OWNER_CONTEXT_DIR))
}

/// `<runtime_root>/discord_delivery_records/<provider>/<channel_id>.json`.
/// Inner form takes the `runtime/` dir explicitly so tests can target an
/// isolated tempdir without the `AGENTDESK_ROOT_DIR` env dance.
#[allow(dead_code)] // #3089 B0: exercised by the store tests; B1 uses the env-resolved form.
fn delivery_record_path_in_root(
    runtime_root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
) -> PathBuf {
    runtime_root
        .join(DELIVERY_RECORDS_DIR)
        .join(provider.as_str())
        .join(format!("{channel_id}.json"))
}

pub(in crate::services::discord) fn delivery_record_path(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<PathBuf> {
    delivery_records_root().map(|root| {
        root.join(provider.as_str())
            .join(format!("{channel_id}.json"))
    })
}

fn fresh_send_record_path(provider: &ProviderKind, channel_id: u64) -> Option<PathBuf> {
    fresh_send_records_root().map(|root| {
        root.join(provider.as_str())
            .join(format!("{channel_id}.json"))
    })
}

#[cfg(test)]
fn delivery_owner_context_path_in_root(
    runtime_root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
) -> PathBuf {
    runtime_root
        .join(DELIVERY_OWNER_CONTEXT_DIR)
        .join(provider.as_str())
        .join(format!("{channel_id}.json"))
}

fn delivery_owner_context_path(provider: &ProviderKind, channel_id: u64) -> Option<PathBuf> {
    delivery_owner_context_root().map(|root| {
        root.join(provider.as_str())
            .join(format!("{channel_id}.json"))
    })
}

// ---------------------------------------------------------------------------
// Per-record flock (mirrors inflight.rs's `lock_inflight_state_path`) so a
// read-modify-write (acquire lease / write frontier / clear lease) is atomic
// against a concurrent writer in the same process (invariant I4 — no new
// non-atomic load→mutate→save race). The `.lock` file lives in the sidecar
// dir, still outside the inflight scan path.
// ---------------------------------------------------------------------------

// #5071 T4-B2 widened this pair from `outbound` to the discord module so the
// reachability obligation ledger reuses this flock instead of introducing a
// second lock mechanism (4987 §5.2 storage rule). Widening a visibility is not
// a new caller contract: the lock's semantics, its `.json.lock` sidecar path
// and its Drop-unlock are untouched, and the two existing writers are
// unchanged.
pub(in crate::services::discord) struct DeliveryRecordLock {
    _file: fs::File,
}

impl Drop for DeliveryRecordLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // Best effort unlock; closing the fd would release it anyway.
            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

pub(in crate::services::discord) fn lock_record_path(
    record_path: &Path,
) -> Result<DeliveryRecordLock, String> {
    let lock_path = record_path.with_extension("json.lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(DeliveryRecordLock { _file: file })
}

// ---------------------------------------------------------------------------
// Store core (path-based; the public provider/channel API delegates here)
// ---------------------------------------------------------------------------

/// I3 conservative: missing OR malformed → `None`, never an `Err` a caller might
/// misread as "delivered". A torn/garbage record reads as not-yet-delivered.
pub(in crate::services::discord) fn read_record_at(path: &Path) -> Option<DeliveryRecord> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_record_at(path: &Path, record: &DeliveryRecord) -> Result<(), String> {
    let data = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
    // Reuse the existing temp+rename+fsync helper (I4) — do not hand-roll.
    runtime_store::atomic_write(path, &data)
}

/// flock-guarded read-modify-write. Creates the record (default: both fields
/// `None`) if absent.
fn mutate_record_at(path: &Path, mutate: impl FnOnce(&mut DeliveryRecord)) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _lock = lock_record_path(path)?;
    let mut record = read_record_at(path).unwrap_or_default();
    mutate(&mut record);
    write_record_at(path, &record)
}

// ---------------------------------------------------------------------------
// Store mutators. The REAL mutation logic lives in the path-based `_at` core so
// the production provider/channel API AND the tests both drive the SAME logic —
// a mutation to a closure is then caught by the test (review fix: the earlier
// tests called `mutate_record_at` directly, so mutating the public fn slipped
// through). The thin provider/channel wrappers only resolve the runtime path.
// Dead until B1 wires the writers.
// ---------------------------------------------------------------------------

fn record_path_or_err(provider: &ProviderKind, channel_id: u64) -> Result<PathBuf, String> {
    delivery_record_path(provider, channel_id)
        .ok_or_else(|| "delivery_record: runtime root unavailable".to_string())
}

fn owner_context_path_or_err(provider: &ProviderKind, channel_id: u64) -> Result<PathBuf, String> {
    delivery_owner_context_path(provider, channel_id)
        .ok_or_else(|| "delivery_owner_context: runtime root unavailable".to_string())
}

/// Read the durable record. `None` when the runtime root is unavailable, the
/// file is missing, or the content is malformed (I3 conservative).
#[allow(dead_code)] // #3089 B0: read by B1 shadow assert / B2 read-authority.
pub(in crate::services::discord) fn read_record(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<DeliveryRecord> {
    read_record_at(&delivery_record_path(provider, channel_id)?)
}

/// Acquire/refresh the transient lease. Preserves any existing
/// `delivered_frontier` (never advances it — I2).
fn upsert_lease_at(path: &Path, lease: DurableLease) -> Result<(), String> {
    mutate_record_at(path, |record| record.delivery_lease = Some(lease))
}

#[allow(dead_code)] // #3089 B0: called by B1's lease acquire on cutover.
pub(in crate::services::discord) fn upsert_lease(
    provider: &ProviderKind,
    channel_id: u64,
    lease: DurableLease,
) -> Result<(), String> {
    upsert_lease_at(&record_path_or_err(provider, channel_id)?, lease)
}

/// Raw path writer retained for record-shape tests and narrowly-scoped repair
/// helpers. Production confirmed-delivery entry points must use the
/// generation-guarded writers below.
#[cfg(test)]
fn write_delivered_frontier_at(path: &Path, frontier: DeliveredCommit) -> Result<(), String> {
    mutate_record_at(path, |record| record.delivered_frontier = Some(frontier))
}

fn merge_confirmed_frontier(
    existing: Option<&DeliveredCommit>,
    incoming: DeliveredCommit,
) -> DeliveredCommit {
    // The highest END wins, with the existing commit winning ties. Keep that
    // winner whole: its START, attempts, and panel anchor describe one confirmed
    // delivery and must never be combined with an unrelated commit's fields.
    match existing {
        Some(existing)
            if existing.generation_mtime_ns == incoming.generation_mtime_ns
                && existing.range.1 >= incoming.range.1 =>
        {
            existing.clone()
        }
        _ => incoming,
    }
}

fn append_confirmed_receipt(record: &mut DeliveryRecord, receipt: ConfirmedDeliveryReceipt) {
    let receipts = &mut record.confirmed_deliveries;
    receipts.retain(|existing| existing != &receipt);
    receipts.push(receipt);
    let excess = receipts
        .len()
        .saturating_sub(CONFIRMED_DELIVERY_RECEIPT_LIMIT);
    if excess > 0 {
        receipts.drain(..excess);
    }
}
#[derive(Clone, Copy)]
enum EqualRangeAnchorPolicy {
    PreserveExisting,
    ReplaceProvenGone {
        expected_panel_channel_id: u64,
        expected_panel_msg_id: u64,
    },
}

#[derive(Clone, Copy)]
/// The identity a watcher/sink delivery is allowed to write under, revalidated
/// after the record flock. #4911 R10: the load-bearing authority is the
/// lease-time source generation plus the reset incarnation — deliberately NOT the
/// full frontier token. Requiring `committed_offset` equality here would let one
/// harmless concurrent advance discard an otherwise valid delivery.
struct WatcherFrontierLockAuthority<'a> {
    shared: &'a crate::services::discord::SharedData,
    channel: ChannelId,
    lease_reset_incarnation: u64,
    generation_mtime_ns: i64,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::services::discord) struct WatcherDeliveryRecordAuthority {
    pub(in crate::services::discord) lease_reset_incarnation: u64,
    pub(in crate::services::discord) generation_mtime_ns: i64,
    pub(in crate::services::discord) ledger_user_msg_id: Option<u64>,
}

/// #5071 T1 S7: what the RECOVERY caller must bring to the shadow-mirror funnel
/// that no other caller does, so that joining the funnel is not a behaviour
/// change dressed as a refactor.
///
/// `generation_mtime_ns` is the load-bearing field. Every other funnel caller
/// leaves the generation to be read from `coord.confirmed_end_generation_mtime_ns`,
/// the IN-MEMORY watermark's generation — and that value is only ever stored by
/// `tmux::advance_watcher_confirmed_end_for_generation` on a real in-memory
/// advance (plus the two watermark resets in `tmux_session_files.rs`). The
/// recovery path never advances the in-memory watermark at all (#5071 T1 S7b
/// owns that question), so on a freshly started process the coord's generation
/// is still its `AtomicI64::new(0)` initial value while the wrapper's generation
/// marker on disk is not. Reading the coord there would take the funnel's
/// unknown-generation early return on exactly the deliveries recovery exists to
/// make. This field therefore carries the FILESYSTEM generation the recovery
/// path already resolved (`current_generation_mtime_ns`), which
/// `write_confirmed_frontier_guarded_at_with_lock_authority` re-reads and
/// re-compares under the record flock regardless.
///
/// `attempts` is carried for the same reason in miniature: the funnel otherwise
/// re-reads `recovery_relay_attempts` from a fresh inflight load, and the
/// recovery caller's snapshot is the value the pre-S7 raw write recorded.
///
/// `expected_gone_anchor` is `Some` only on the re-anchor path that Discord has
/// proved the recorded anchor GONE for; it selects
/// [`EqualRangeAnchorPolicy::ReplaceProvenGone`] instead of the monotonic merge.
///
/// The remaining three fields are the delivery's own coordinates, carried here
/// rather than as loose parameters so `record_recovery_terminal_delivery` stays
/// inside the argument-count lint without an `allow` (the structural clippy
/// ratchet, #4519, counts those per file).
#[derive(Clone, Copy, Debug)]
pub(in crate::services::discord) struct RecoveryDeliveryRecordAuthority {
    pub(in crate::services::discord) generation_mtime_ns: i64,
    pub(in crate::services::discord) attempts: u32,
    pub(in crate::services::discord) expected_gone_anchor: Option<(u64, u64)>,
    pub(in crate::services::discord) range: (u64, u64),
    /// `(channel_id, msg_id)` of the Discord message this recovery delivery
    /// anchored on — same field order as `expected_gone_anchor`.
    pub(in crate::services::discord) terminal_anchor: (u64, u64),
    /// The inbound `user_msg_id` this turn answers, already `0`-filtered by the
    /// caller. Becomes the funnel's `ledger_user_msg_id`.
    pub(in crate::services::discord) ledger_user_msg_id: Option<u64>,
}

fn write_confirmed_frontier_guarded_at_with_before_lock(
    path: &Path,
    tmux_session_name: &str,
    frontier: DeliveredCommit,
    receipt: Option<ConfirmedDeliveryReceipt>,
    equal_range_anchor_policy: EqualRangeAnchorPolicy,
    before_lock: impl FnOnce(),
) -> Result<(), String> {
    write_confirmed_frontier_guarded_at_with_lock_authority(
        path,
        tmux_session_name,
        frontier,
        receipt,
        equal_range_anchor_policy,
        None,
        before_lock,
    )
}

fn write_confirmed_frontier_guarded_at_with_lock_authority(
    path: &Path,
    tmux_session_name: &str,
    frontier: DeliveredCommit,
    receipt: Option<ConfirmedDeliveryReceipt>,
    equal_range_anchor_policy: EqualRangeAnchorPolicy,
    lock_authority: Option<WatcherFrontierLockAuthority<'_>>,
    before_lock: impl FnOnce(),
) -> Result<(), String> {
    if tmux_session_name.is_empty()
        || frontier.generation_mtime_ns == 0
        || frontier.range.1 <= frontier.range.0
    {
        return Err("delivery_record: refusing incomplete confirmed frontier authority".into());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    before_lock();
    let _lock = lock_record_path(path)?;

    // Receipt construction (or the receipt-less frontier fallback) can race a
    // wrapper rotation while waiting for the record lock. Revalidate only after
    // acquiring that lock so a stale incarnation can never replace the current
    // one, regardless of which confirmed-delivery path reached this writer.
    let current_generation = current_generation_mtime_ns(tmux_session_name);
    if current_generation == 0 || current_generation != frontier.generation_mtime_ns {
        return Err("delivery_record: refusing frontier from a stale JSONL incarnation".into());
    }
    if let Some(authority) = lock_authority {
        let coord = authority.shared.tmux_relay_coord(authority.channel);
        let current_token = coord.frontier_token();
        let current_coord_generation = coord
            .confirmed_end_generation_mtime_ns
            .load(Ordering::Acquire);
        // #4911 R10: incarnation + generation only. A concurrent delivery that
        // advanced `committed_offset` between the caller's token read and this
        // flock is HARMLESS — it does not change which source incarnation these
        // bytes came from — so it must not discard this record.
        if current_token.reset_incarnation != authority.lease_reset_incarnation
            || current_coord_generation != authority.generation_mtime_ns
            || current_generation != authority.generation_mtime_ns
        {
            return Err(
                "delivery_record: watcher frontier mutation authority changed before record lock"
                    .into(),
            );
        }
    }

    let mut record = read_record_at(path).unwrap_or_default();
    match equal_range_anchor_policy {
        EqualRangeAnchorPolicy::PreserveExisting => {
            record.delivered_frontier = Some(merge_confirmed_frontier(
                record.delivered_frontier.as_ref(),
                frontier,
            ));
        }
        EqualRangeAnchorPolicy::ReplaceProvenGone {
            expected_panel_channel_id,
            expected_panel_msg_id,
        } => {
            let expected_anchor_still_current =
                record.delivered_frontier.as_ref().is_some_and(|existing| {
                    existing.generation_mtime_ns == frontier.generation_mtime_ns
                        && existing.range == frontier.range
                        && existing.panel_channel_id == Some(expected_panel_channel_id)
                        && existing.panel_msg_id == Some(expected_panel_msg_id)
                });
            if expected_anchor_still_current
                && frontier.panel_msg_id.is_some()
                && frontier.panel_channel_id.is_some()
            {
                // The exact anchor proved gone is still current under this lock.
                // Any concurrent re-anchor makes the comparison fail and leaves
                // that newer whole commit untouched.
                record.delivered_frontier = Some(frontier);
            } else {
                tracing::warn!(
                    record_path = %path.display(),
                    expected_generation_mtime_ns = frontier.generation_mtime_ns,
                    expected_range = ?frontier.range,
                    expected_panel_channel_id,
                    expected_panel_msg_id,
                    current_frontier = ?record.delivered_frontier,
                    outcome = "preserved_current_frontier",
                    "recovery proven-gone frontier CAS did not match"
                );
            }
        }
    }

    if let Some(receipt) = receipt {
        // A delayed, lower-range receipt remains exact proof for its own restored
        // seed. Append it even when the monotonic frontier correctly stays put.
        append_confirmed_receipt(&mut record, receipt);
    }
    write_record_at(path, &record)
}

fn write_confirmed_frontier_guarded_at(
    path: &Path,
    tmux_session_name: &str,
    frontier: DeliveredCommit,
    receipt: Option<ConfirmedDeliveryReceipt>,
) -> Result<(), String> {
    write_confirmed_frontier_guarded_at_with_before_lock(
        path,
        tmux_session_name,
        frontier,
        receipt,
        EqualRangeAnchorPolicy::PreserveExisting,
        || {},
    )
}

fn write_delivered_frontier_guarded_at_with_before_lock(
    path: &Path,
    tmux_session_name: &str,
    frontier: DeliveredCommit,
    before_lock: impl FnOnce(),
) -> Result<(), String> {
    write_confirmed_frontier_guarded_at_with_before_lock(
        path,
        tmux_session_name,
        frontier,
        None,
        EqualRangeAnchorPolicy::PreserveExisting,
        before_lock,
    )
}

/// Persist a confirmed frontier through the current tmux incarnation guard.
/// Delayed lower/equal-END writers preserve the existing winning commit whole.
pub(in crate::services::discord) fn write_delivered_frontier(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_name: &str,
    frontier: DeliveredCommit,
) -> Result<(), String> {
    write_delivered_frontier_guarded_at_with_before_lock(
        &record_path_or_err(provider, channel_id)?,
        tmux_session_name,
        frontier,
        || {},
    )
}

/// Recovery-only equal-range replacement after Discord has proved the recorded
/// anchor gone. All other range relationships retain the normal monotonic merge.
///
/// #5071 T1 S7 TOOK THIS OUT OF PRODUCTION. Its single production caller was the
/// recovery path's re-anchor arm, which now selects the same
/// [`EqualRangeAnchorPolicy::ReplaceProvenGone`] through
/// `shadow_mirror_delivered_frontier_inner` instead. It is kept, and kept
/// exercised, because its record-shape tests are the direct coverage of that CAS
/// — but it is `#[cfg(test)]` so that "no production caller" is held by the
/// compiler and not only by
/// `scripts/check_durable_frontier_writer_call_sites.py`.
#[cfg(test)]
pub(in crate::services::discord) fn write_proven_gone_equal_range_frontier(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_name: &str,
    expected_gone_anchor: (u64, u64),
    frontier: DeliveredCommit,
) -> Result<(), String> {
    write_proven_gone_equal_range_frontier_at_with_before_lock(
        &record_path_or_err(provider, channel_id)?,
        tmux_session_name,
        expected_gone_anchor,
        frontier,
        || {},
    )
}

#[cfg(test)]
fn write_proven_gone_equal_range_frontier_at_with_before_lock(
    path: &Path,
    tmux_session_name: &str,
    expected_gone_anchor: (u64, u64),
    frontier: DeliveredCommit,
    before_lock: impl FnOnce(),
) -> Result<(), String> {
    write_confirmed_frontier_guarded_at_with_before_lock(
        path,
        tmux_session_name,
        frontier,
        None,
        EqualRangeAnchorPolicy::ReplaceProvenGone {
            expected_panel_channel_id: expected_gone_anchor.0,
            expected_panel_msg_id: expected_gone_anchor.1,
        },
        before_lock,
    )
}

fn write_confirmed_delivery_at(
    path: &Path,
    frontier: DeliveredCommit,
    receipt: ConfirmedDeliveryReceipt,
) -> Result<(), String> {
    write_confirmed_delivery_at_with_lock_authority(
        path,
        frontier,
        receipt,
        EqualRangeAnchorPolicy::PreserveExisting,
        None,
    )
}

fn write_confirmed_delivery_at_with_lock_authority(
    path: &Path,
    frontier: DeliveredCommit,
    receipt: ConfirmedDeliveryReceipt,
    equal_range_anchor_policy: EqualRangeAnchorPolicy,
    lock_authority: Option<WatcherFrontierLockAuthority<'_>>,
) -> Result<(), String> {
    if !receipt.is_authoritative()
        || receipt.source.range != frontier.range
        || receipt.source.generation_mtime_ns != frontier.generation_mtime_ns
        || frontier.panel_msg_id != Some(receipt.message_id)
        || frontier.panel_channel_id != Some(receipt.delivery_channel_id)
    {
        return Err("delivery_record: refusing incomplete or frontier-mismatched receipt".into());
    }
    let tmux_session_name = receipt.source.tmux_session_name.clone();
    write_confirmed_frontier_guarded_at_with_lock_authority(
        path,
        &tmux_session_name,
        frontier,
        Some(receipt),
        equal_range_anchor_policy,
        lock_authority,
        || {},
    )
}

/// Atomically persist the monotonic frontier and the exact Discord receipt.
/// This is delivery authority, never rollout telemetry, so callers must invoke
/// it after a confirmed POST regardless of the shadow flag.
pub(in crate::services::discord) fn write_confirmed_delivery(
    provider: &ProviderKind,
    offset_authority_channel_id: u64,
    frontier: DeliveredCommit,
    receipt: ConfirmedDeliveryReceipt,
) -> Result<(), String> {
    if receipt.source.offset_authority_channel_id != offset_authority_channel_id {
        return Err("delivery_record: receipt owner does not match record key".into());
    }
    if receipt.source.provider != provider.as_str() {
        return Err("delivery_record: receipt provider does not match record key".into());
    }
    write_confirmed_delivery_at(
        &record_path_or_err(provider, offset_authority_channel_id)?,
        frontier,
        receipt,
    )
}

fn pinned_source_receipt(
    source: &ExactJsonlSourceIdentity,
    message_id: u64,
) -> Result<(ProviderKind, ConfirmedDeliveryReceipt), String> {
    let provider = ProviderKind::from_str(&source.provider)
        .filter(|_| source.is_authoritative() && message_id != 0)
        .ok_or_else(|| "delivery_record: incomplete pinned source receipt".to_string())?;
    Ok((
        provider,
        ConfirmedDeliveryReceipt {
            source: source.clone(),
            delivery_channel_id: source.delivery_channel_id,
            message_id,
        },
    ))
}
#[allow(dead_code)]
pub(in crate::services::discord) fn record_current_pinned_delivery(
    source: &ExactJsonlSourceIdentity,
    message_id: u64,
) -> Result<(), String> {
    let (provider, receipt) = pinned_source_receipt(source, message_id)?;
    let frontier = DeliveredCommit {
        range: source.range,
        generation_mtime_ns: source.generation_mtime_ns,
        attempts: 0,
        panel_msg_id: Some(message_id),
        panel_channel_id: Some(source.delivery_channel_id),
    };
    write_confirmed_delivery_at(
        &record_path_or_err(&provider, source.offset_authority_channel_id)?,
        frontier,
        receipt,
    )
}
/// Append only the exact historical receipt; never alter the current frontier.
pub(in crate::services::discord) fn record_historical_pinned_delivery(
    source: &ExactJsonlSourceIdentity,
    message_id: u64,
) -> Result<(), String> {
    let (provider, receipt) = pinned_source_receipt(source, message_id)?;
    let path = record_path_or_err(&provider, source.offset_authority_channel_id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let _lock = lock_record_path(&path)?;
    let mut record = read_record_at(&path).unwrap_or_default();
    append_confirmed_receipt(&mut record, receipt);
    write_record_at(&path, &record)
}
pub(in crate::services::discord) fn historical_pinned_delivery_exists(
    source: &ExactJsonlSourceIdentity,
    message_id: u64,
) -> bool {
    let Ok((provider, receipt)) = pinned_source_receipt(source, message_id) else {
        return false;
    };
    read_record(&provider, source.offset_authority_channel_id)
        .is_some_and(|record| record.confirmed_deliveries.contains(&receipt))
}
fn append_completed_turn_if_nonzero(provider: &ProviderKind, channel_id: u64, user_msg_id: u64) {
    if user_msg_id != 0 {
        completed_turn_ledger::append_completed_turn(provider, channel_id, user_msg_id);
    }
}
pub(in crate::services::discord) fn record_pinned_delivery_metadata(
    source: &ExactJsonlSourceIdentity,
    body: &str,
    user_msg_id: u64,
) {
    let Some(provider) = ProviderKind::from_str(&source.provider) else {
        return;
    };
    // #5264 PR-B: the fingerprint goes under the OFFSET-AUTHORITY (watcher owner) channel
    // and the ledger under the delivery channel, matching
    // `shadow_mirror_delivered_frontier_inner`. Every reader of the fingerprint
    // (`tmux_watcher/turn_identity.rs`, `tmux_watcher/terminal_preflight.rs`) looks it up
    // under the watcher's owner channel, and the record path plus the hash are both keyed
    // by channel, so writing it under the delivery channel strands it whenever owner and
    // destination differ — leaving the duplicate defence inert in exactly the split-channel
    // case it exists for.
    record_delivered_content_fingerprint_for_generation(
        &provider,
        source.offset_authority_channel_id,
        body,
        source.generation_mtime_ns,
    );
    append_completed_turn_if_nonzero(&provider, source.delivery_channel_id, user_msg_id);
}
fn commit_ordered_jsonl_range_at(
    path: &Path,
    tmux_session_name: &str,
    range: (u64, u64),
    generation_mtime_ns: i64,
) -> Result<bool, String> {
    commit_ordered_jsonl_range_at_with_before_lock(
        path,
        tmux_session_name,
        range,
        generation_mtime_ns,
        || {},
    )
}

fn commit_ordered_jsonl_range_at_with_before_lock(
    path: &Path,
    tmux_session_name: &str,
    range: (u64, u64),
    generation_mtime_ns: i64,
    before_lock: impl FnOnce(),
) -> Result<bool, String> {
    if generation_mtime_ns == 0 || range.1 <= range.0 {
        return Ok(false);
    }
    write_delivered_frontier_guarded_at_with_before_lock(
        path,
        tmux_session_name,
        DeliveredCommit {
            range,
            generation_mtime_ns,
            attempts: 1,
            panel_msg_id: None,
            panel_channel_id: None,
        },
        before_lock,
    )?;
    Ok(true)
}

/// Durably commit a confirmed idle/catch-up JSONL range. Unlike the rollout
/// shadow writer, this is delivery authority and is therefore flag-independent.
pub(in crate::services::discord) fn commit_ordered_jsonl_range(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    range: (u64, u64),
    generation_mtime_ns: i64,
) -> Result<bool, String> {
    commit_ordered_jsonl_range_at(
        &record_path_or_err(provider, channel.get())?,
        tmux_session_name,
        range,
        generation_mtime_ns,
    )
}

fn write_watcher_owner_context_at(
    path: &Path,
    watcher_owner_channel_id: u64,
    tmux_session_name: &str,
    generation_mtime_ns: i64,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _lock = lock_record_path(path)?;
    let context = WatcherOwnerContext {
        watcher_owner_channel_id,
        tmux_session_name: tmux_session_name.to_string(),
        generation_mtime_ns,
    };
    let data = serde_json::to_string_pretty(&context).map_err(|e| e.to_string())?;
    runtime_store::atomic_write(path, &data)
}

/// Mixed-binary-safe owner-channel index writer for #3751.
///
/// `InflightTurnState` carries the same field for fast in-row reads, but an old
/// binary can erase additive inflight fields by reserializing the row. This
/// sidecar writer records the owner under the delivery channel in the separate
/// `discord_delivery_owner_context` subtree that old binaries never scan and
/// known delivery-record mutators never rewrite.
pub(in crate::services::discord) fn record_watcher_owner_channel_context(
    provider: &ProviderKind,
    delivery_channel: ChannelId,
    watcher_owner_channel: ChannelId,
    tmux_session_name: &str,
) -> Result<(), String> {
    if tmux_session_name.is_empty() {
        return Ok(());
    }
    write_watcher_owner_context_at(
        &owner_context_path_or_err(provider, delivery_channel.get())?,
        watcher_owner_channel.get(),
        tmux_session_name,
        current_generation_mtime_ns(tmux_session_name),
    )
}

fn read_watcher_owner_context_at(path: &Path) -> Option<WatcherOwnerContext> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn watcher_owner_channel_from_context_at(
    path: &Path,
    tmux_session_name: &str,
    current_generation_mtime_ns: i64,
) -> Option<u64> {
    let context = read_watcher_owner_context_at(path)?;
    if context.watcher_owner_channel_id == 0 || context.tmux_session_name != tmux_session_name {
        return None;
    }
    if context.generation_mtime_ns == 0
        || context.generation_mtime_ns != current_generation_mtime_ns
    {
        return None;
    }
    Some(context.watcher_owner_channel_id)
}

pub(in crate::services::discord) fn watcher_owner_channel_for_delivery_channel(
    provider: &ProviderKind,
    delivery_channel: ChannelId,
    tmux_session_name: &str,
) -> Option<ChannelId> {
    if tmux_session_name.is_empty() {
        return None;
    }
    let path = delivery_owner_context_path(provider, delivery_channel.get())?;
    watcher_owner_channel_from_context_at(
        &path,
        tmux_session_name,
        current_generation_mtime_ns(tmux_session_name),
    )
    .map(ChannelId::new)
}

fn resolved_receipt_owner_channel(
    provider: &ProviderKind,
    delivery_channel: ChannelId,
    source: &ExactJsonlSourceIdentity,
) -> Option<ChannelId> {
    if !source.is_authoritative()
        || current_generation_mtime_ns(&source.tmux_session_name) != source.generation_mtime_ns
    {
        return None;
    }

    let captured_owner =
        crate::services::discord::inflight::opt_channel_id(source.offset_authority_channel_id)?;
    match watcher_owner_channel_for_delivery_channel(
        provider,
        delivery_channel,
        &source.tmux_session_name,
    ) {
        Some(resolved) if resolved == captured_owner => Some(resolved),
        // Same-channel delivery is the deterministic default and predates the
        // split-channel owner-context sidecar. A split receipt, however, is
        // never trusted without the exact current context mapping.
        None if captured_owner == delivery_channel => Some(delivery_channel),
        _ => None,
    }
}

/// Re-read the durable receipt at restored-seed consumption time and require an
/// exact match on both JSONL source identity and Discord destination identity.
/// Missing/legacy/partially populated records fail closed (seed is preserved).
pub(in crate::services::discord) fn confirmed_delivery_receipt_exists(
    provider: &ProviderKind,
    delivery_channel: ChannelId,
    message_id: u64,
    source: &ExactJsonlSourceIdentity,
) -> bool {
    if message_id == 0
        || provider.as_str() != source.provider
        || delivery_channel.get() != source.delivery_channel_id
    {
        return false;
    }
    if historical_pinned_delivery_exists(source, message_id) {
        return true;
    }
    let Some(owner_channel) = resolved_receipt_owner_channel(provider, delivery_channel, source)
    else {
        return false;
    };
    read_record(provider, owner_channel.get()).is_some_and(|record| {
        record.confirmed_deliveries.iter().any(|receipt| {
            receipt.is_authoritative()
                && receipt.source == *source
                && receipt.delivery_channel_id == delivery_channel.get()
                && receipt.message_id == message_id
        })
    })
}

/// Release: clear the lease ONLY. `delivered_frontier` survives (design §4.3).
fn clear_lease_at(path: &Path) -> Result<(), String> {
    mutate_record_at(path, |record| record.delivery_lease = None)
}

#[allow(dead_code)] // #3089 B0: called by B1's release path.
pub(in crate::services::discord) fn clear_lease(
    provider: &ProviderKind,
    channel_id: u64,
) -> Result<(), String> {
    clear_lease_at(&record_path_or_err(provider, channel_id)?)
}

/// Remove the whole record (turn-end GC). `true` if a file was removed.
fn delete_record_at(path: &Path) -> bool {
    fs::remove_file(path).is_ok()
}

#[allow(dead_code)] // #3089 B0: called by B1/B2 at turn finalize.
pub(in crate::services::discord) fn delete_record(
    provider: &ProviderKind,
    channel_id: u64,
) -> bool {
    match delivery_record_path(provider, channel_id) {
        Some(path) => delete_record_at(&path),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// #3089 B1 / #4911 Phase A — persist confirmed delivery authority.
//
// A confirmed `Delivered` commit always persists the generation-scoped frontier
// and, when complete turn/destination identity is available, an exact receipt.
// The legacy shadow flag now controls only the durable-END divergence signal;
// read-authority rollout remains separately gated below.
// ---------------------------------------------------------------------------

/// #5071 T1 S8-1r2: where a rollout flag's value came from, which is a different
/// question from what the value is. `CompiledDefault` means `std::env::var` did not
/// yield a Unicode value (absent and non-Unicode values both take this path), so the
/// compiled value was used. `EnvOverride` means a Unicode value was present —
/// whatever it said — so a
/// node-local untracked `launchd.env`, not the repository, decided it. #5262's
/// completion contract asks for repo-owned flags, which is a claim about
/// provenance that `shadow_enabled`/`authority_enabled` alone cannot answer: once
/// the compiled default matches the pin, the two are indistinguishable by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum FlagSource {
    CompiledDefault,
    EnvOverride,
}

impl FlagSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::CompiledDefault => "compiled_default",
            Self::EnvOverride => "env_override",
        }
    }
}

/// A rollout flag's resolved value paired with its provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) struct FlagResolution {
    pub(in crate::services::discord) enabled: bool,
    pub(in crate::services::discord) source: FlagSource,
}

impl FlagResolution {
    const fn compiled_default(enabled: bool) -> Self {
        Self {
            enabled,
            source: FlagSource::CompiledDefault,
        }
    }

    const fn env_override(enabled: bool) -> Self {
        Self {
            enabled,
            source: FlagSource::EnvOverride,
        }
    }
}

/// The opt-in reading shared by both rollout flags today: absent → compiled
/// default OFF, present → `EnvOverride` whose value is ON only for `1`/`true`
/// (trimmed, case-insensitive). Provenance is decided by presence ALONE, so an
/// explicit `=0` still reports `EnvOverride` — that is the whole point of the
/// distinction, since a pin that agrees with the compiled default is otherwise
/// invisible. The two named wrappers below exist so a later slice can move one
/// flag's default without touching the other.
fn resolve_opt_in_flag(raw: Option<&str>) -> FlagResolution {
    match raw {
        Some(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            FlagResolution::env_override(normalized == "1" || normalized == "true")
        }
        None => FlagResolution::compiled_default(false),
    }
}

/// Pure resolver for `AGENTDESK_DELIVERY_RECORD_SHADOW` (compiled default OFF).
fn resolve_shadow_flag(raw: Option<&str>) -> FlagResolution {
    resolve_opt_in_flag(raw)
}

/// Pure resolver for `AGENTDESK_DELIVERY_RECORD_AUTHORITY` (compiled default OFF).
fn resolve_authority_flag(raw: Option<&str>) -> FlagResolution {
    resolve_opt_in_flag(raw)
}

/// #3089 B1 shadow-write flag (`AGENTDESK_DELIVERY_RECORD_SHADOW`, OnceLock,
/// default OFF), with its provenance. Divergence telemetry only; confirmed
/// persistence never consults this flag.
fn delivery_record_shadow_resolution() -> FlagResolution {
    #[cfg(test)]
    if let Some(forced) = shadow_test_seam::current_override() {
        // The seam forces a value the environment did not supply, so it reports
        // `EnvOverride` rather than claiming the repository compiled it in.
        // Provenance assertions therefore test `resolve_shadow_flag` directly.
        return FlagResolution::env_override(forced);
    }
    static CACHED: OnceLock<FlagResolution> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let resolution = resolve_shadow_flag(
            std::env::var("AGENTDESK_DELIVERY_RECORD_SHADOW")
                .ok()
                .as_deref(),
        );
        if resolution.enabled {
            tracing::info!("  ✓ delivery_record_shadow: enabled");
        }
        resolution
    })
}

pub(in crate::services::discord) fn delivery_record_shadow_enabled() -> bool {
    delivery_record_shadow_resolution().enabled
}

/// #4130 test seam: a per-thread override of
/// `delivery_record_shadow_enabled()` so default-OFF tests stay deterministic
/// even when the developer shell exports `AGENTDESK_DELIVERY_RECORD_SHADOW=1`.
#[cfg(test)]
pub(in crate::services::discord) mod shadow_test_seam {
    use std::cell::Cell;

    thread_local! {
        static OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
    }

    pub(in crate::services::discord) fn current_override() -> Option<bool> {
        OVERRIDE.with(Cell::get)
    }

    #[must_use]
    pub(in crate::services::discord) fn force(value: bool) -> Guard {
        Guard {
            previous: OVERRIDE.with(|cell| cell.replace(Some(value))),
        }
    }

    pub(in crate::services::discord) struct Guard {
        previous: Option<bool>,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            OVERRIDE.with(|cell| cell.set(self.previous));
        }
    }
}

/// #3089 B2b read-authority flag (`AGENTDESK_DELIVERY_RECORD_AUTHORITY`, OnceLock,
/// default OFF). When OFF (default) the dedup gates read the legacy in-memory
/// `committed_relay_offset` verbatim → byte-identical, deploy no-op. When ON the
/// gates consult the durable `delivered_frontier` (fused with in-memory) so the
/// "already-relayed → skip" decision survives a restart / cross-actor boundary.
fn delivery_record_authority_resolution() -> FlagResolution {
    // #3933: a per-thread test override (see `authority_test_seam`) lets a unit
    // test drive the authority-ON enforce path (the release config) through the
    // real save path WITHOUT poisoning the env-global `OnceLock` cache for
    // sibling tests that assume the compiled-default OFF. Production strips this
    // branch entirely (`cfg(test)`), so the flag stays byte-identical at runtime.
    #[cfg(test)]
    if let Some(forced) = authority_test_seam::current_override() {
        // Reported as `EnvOverride` for the same reason as the shadow seam: the
        // value did not come from what this repository compiled in.
        return FlagResolution::env_override(forced);
    }
    static CACHED: OnceLock<FlagResolution> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let resolution = resolve_authority_flag(
            std::env::var("AGENTDESK_DELIVERY_RECORD_AUTHORITY")
                .ok()
                .as_deref(),
        );
        if resolution.enabled {
            tracing::info!("  ✓ delivery_record_authority: enabled");
        }
        resolution
    })
}

pub(in crate::services::discord) fn delivery_record_authority_enabled() -> bool {
    delivery_record_authority_resolution().enabled
}

/// #3933 test seam: a per-thread override of `delivery_record_authority_enabled()`
/// so a unit test can drive the authority-ON enforce path (the release config)
/// deterministically. Housed in a `#[cfg(test)] mod` (not inline `#[cfg(test)]`
/// items) so the seam counts as test — not production — review surface.
#[cfg(test)]
pub(in crate::services::discord) mod authority_test_seam {
    use std::cell::Cell;

    thread_local! {
        /// When `Some`, forces the flag on THIS thread only; `None` (default)
        /// falls through to the env-cached `OnceLock`.
        static OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
    }

    pub(in crate::services::discord) fn current_override() -> Option<bool> {
        OVERRIDE.with(Cell::get)
    }

    /// RAII: force `delivery_record_authority_enabled()` to `value` on the
    /// current thread until the returned guard drops (then restore the prior
    /// override). The env-cached `OnceLock` cannot be re-set once initialized,
    /// and mutating the process-global env would race sibling tests, so a
    /// thread-local + RAII keeps the authority-ON honor path order-independent
    /// and leak-free.
    #[must_use]
    pub(in crate::services::discord) fn force(value: bool) -> Guard {
        Guard {
            previous: OVERRIDE.with(|cell| cell.replace(Some(value))),
        }
    }

    pub(in crate::services::discord) struct Guard {
        previous: Option<bool>,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            OVERRIDE.with(|cell| cell.set(self.previous));
        }
    }
}

pub(super) fn delivery_record_rollout_health_json() -> serde_json::Value {
    delivery_record_rollout_health_json_for_flags(
        delivery_record_shadow_resolution(),
        delivery_record_authority_resolution(),
    )
}

fn delivery_record_rollout_health_json_for_flags(
    shadow: FlagResolution,
    authority: FlagResolution,
) -> serde_json::Value {
    let shadow_enabled = shadow.enabled;
    let authority_enabled = authority.enabled;
    let mode = match (shadow_enabled, authority_enabled) {
        (false, false) => "off",
        (true, false) => "shadow_only",
        (false, true) => "authority_only",
        (true, true) => "shadow_and_authority",
    };
    let dedup_authority = if authority_enabled {
        "durable_delivery_record_frontier"
    } else {
        "in_memory_committed_offset"
    };
    let same_turn_backward_write_enforcement = if authority_enabled {
        "enforcing"
    } else {
        "observe_only"
    };
    let mut configuration_warnings = Vec::new();
    if !authority_enabled {
        configuration_warnings.push(serde_json::json!(
            "delivery_record_authority_disabled: durable frontiers are not the default committed-offset authority"
        ));
    }
    if authority_enabled && !shadow_enabled {
        configuration_warnings.push(serde_json::json!(
            "delivery_record_shadow_disabled: authority is enabled without shadow mirror telemetry"
        ));
    }
    let warning_count = configuration_warnings.len();
    serde_json::json!({
        "shadow_enabled": shadow_enabled,
        "authority_enabled": authority_enabled,
        "mode": mode,
        "dedup_authority": dedup_authority,
        "same_turn_backward_write_enforcement": same_turn_backward_write_enforcement,
        // Provenance, not value: `compiled_default` on both axes is the operational
        // rollout signal for #5262's "repo-owned" requirement. A non-Unicode env
        // value is the documented ambiguity because `std::env::var` maps it to this
        // branch; `env_override` means a Unicode value was read from the environment.
        "flag_source": {
            "shadow": shadow.source.as_str(),
            "authority": authority.source.as_str(),
        },
        "warning_count": warning_count,
        "configuration_warnings": configuration_warnings,
    })
}

/// #3089 B3 / #3416 / #3933 (pure, testable): the same-turn monotonic guard's
/// enforce decision. Returns `true` (block the backward inflight write) ONLY when
/// the durable delivered-frontier authority is ON, a same-turn offset moved
/// backward (`response_sent_offset` or `last_offset`), **and** the backward move
/// is NOT a legitimate full reset. Authority OFF (default) → always `false` → the
/// guard stays observe-only and the write proceeds byte-identically (deploy
/// no-op). Gated by the SAME flag as the read-authority flip so the single offset
/// authority + its enforcement cut over atomically.
///
/// #3933: `is_legitimate_full_reset` carves the legitimate Gemini/Qwen
/// `RetryBoundary` rewind (turn_bridge/retry_state.rs clears `full_response` and
/// rewinds `response_sent_offset`→0 for the SAME turn identity to re-stream) out
/// of the coarse backward-write skip. The release runs
/// `AGENTDESK_DELIVERY_RECORD_AUTHORITY=1`, so before this carve-out the enforce
/// branch dropped the re-streamed body (live data loss). A genuine stale-snapshot
/// backward regression carries a NON-EMPTY body, so it never matches the reset
/// signature and stays blocked.
pub(in crate::services::discord) fn authority_blocks_backward_inflight_write(
    authority_enabled: bool,
    response_sent_offset_monotonic: bool,
    last_offset_monotonic: bool,
    is_legitimate_full_reset: bool,
) -> bool {
    authority_enabled
        && (!response_sent_offset_monotonic || !last_offset_monotonic)
        && !is_legitimate_full_reset
}

/// Pure fusion (testable): the effective committed offset under the flip. The
/// durable frontier END can only RAISE the dedup floor above what the in-memory
/// authority sees (a pre-restart / cross-actor delivery the live atomic missed);
/// it must NEVER lower it. So fuse with `max` — a missed durable write (the
/// coverage hazard) can therefore never drop the floor below in-memory and cause
/// a false skip / lost relay. `None` durable (I3 conservative) → pure in-memory =
/// exactly today's behavior. Pure-durable-only authority waits for B3 hydration.
fn fuse_committed_offset(durable_end: Option<u64>, in_memory: u64) -> u64 {
    match durable_end {
        Some(end) => end.max(in_memory),
        None => in_memory,
    }
}

/// #1270 generation guard (pure, testable): trust the durable frontier to RAISE
/// the dedup floor ONLY if it was written by the CURRENT wrapper generation. A
/// stale-high frontier from a PRIOR same-named tmux generation must NOT be
/// fused — the in-memory generation-reset already zeroed the live offset, and a
/// mismatched durable value would falsely skip the NEW generation's fresh output
/// (lost relay). `current_gen == 0` (no/unreadable `.generation` file) → cannot
/// validate → distrust. Write/read parity: the durable `generation_mtime_ns` is
/// the coord's `confirmed_end_generation_mtime_ns`, itself set from
/// `read_generation_file_mtime_ns` at advance time (tmux.rs), so equality holds
/// within a generation and breaks across one.
pub(in crate::services::discord) fn durable_frontier_generation_current(
    durable_mtime: i64,
    current_gen_mtime: i64,
) -> bool {
    current_gen_mtime != 0 && durable_mtime == current_gen_mtime
}

/// Path-based core (pure-ish, testable): the durable `delivered_frontier`, but
/// ONLY when it was written by the CURRENT wrapper generation (#1270 guard, via
/// [`durable_frontier_generation_current`]) AND its END is physically inside the
/// current transcript byte length (#4188 EOF guard). `None` when the record is
/// absent/malformed (I3 conservative), from a PRIOR generation (stale-high →
/// distrust), or when the current transcript EOF is unavailable (cannot
/// bound-check → distrust). `current_gen_mtime == 0` (no/unreadable
/// `.generation` file) → the guard distrusts everything → `None`.
pub(in crate::services::discord::outbound) fn current_generation_durable_frontier_at(
    path: &Path,
    current_gen_mtime: i64,
    current_transcript_eof: Option<u64>,
) -> Option<DeliveredCommit> {
    let frontier = read_record_at(path).and_then(|r| r.delivered_frontier)?;
    if !durable_frontier_generation_current(frontier.generation_mtime_ns, current_gen_mtime) {
        return None;
    }

    let Some(current_transcript_eof) = current_transcript_eof else {
        tracing::debug!(
            delivery_record_path = %path.display(),
            frontier_end = frontier.range.1,
            frontier_generation_mtime_ns = frontier.generation_mtime_ns,
            current_gen_mtime,
            "durable delivered_frontier current transcript EOF unavailable — distrusting unbounded frontier"
        );
        return None;
    };

    if frontier.range.1 > current_transcript_eof {
        tracing::warn!(
            delivery_record_path = %path.display(),
            frontier_end = frontier.range.1,
            current_transcript_eof,
            frontier_generation_mtime_ns = frontier.generation_mtime_ns,
            current_gen_mtime,
            "durable delivered_frontier end exceeds current transcript EOF — distrusting stale-length frontier (compaction/rotation)"
        );
        return None;
    }

    Some(frontier)
}

/// Path-based core (pure-ish, testable): the durable `delivered_frontier` END.
/// This is the SINGLE durable frontier END reader; the env-resolved
/// [`delivered_frontier_end_current_generation`] and the flag-gated
/// [`effective_committed_offset`] both funnel through it so the generation and
/// EOF-bounds gating logic exists in exactly one place.
fn current_generation_durable_frontier_end_at(
    path: &Path,
    current_gen_mtime: i64,
    current_transcript_eof: Option<u64>,
) -> Option<u64> {
    current_generation_durable_frontier_at(path, current_gen_mtime, current_transcript_eof)
        .map(|f| f.range.1)
}

/// Resolve the current wrapper-generation watermark for `tmux_session_name`. The
/// `tmux` module is `#[cfg(unix)]`; on non-unix targets (windows CI cross-compile
/// check) there is no wrapper generation file, so the generation is absent (`0`),
/// which [`durable_frontier_generation_current`] treats as "distrust".
pub(in crate::services::discord) fn current_generation_mtime_ns(tmux_session_name: &str) -> i64 {
    #[cfg(unix)]
    {
        crate::services::discord::tmux::read_generation_file_mtime_ns(tmux_session_name)
    }
    #[cfg(not(unix))]
    {
        let _ = tmux_session_name;
        0
    }
}

/// #3593 (flag-INDEPENDENT): the CURRENT-generation durable `delivered_frontier`
/// END, or `0` when there is none to trust (absent/malformed record, a stale
/// prior-generation frontier per the #1270 guard, missing transcript EOF, or a
/// frontier END beyond the current EOF). UNLIKE
/// [`effective_committed_offset`], this NEVER consults
/// `AGENTDESK_DELIVERY_RECORD_AUTHORITY` — it is the durable frontier the legacy
/// #3520 new-message floor read, surfaced so the synthetic-resume dedup gate can
/// fuse it (`max`) with the in-memory committed offset and remain a TRUE superset
/// of #3520 under BOTH authority states. Returning `0` (not `None`) keeps the
/// caller's `committed.max(this)` fusion a plain `u64` op; `0` is the safe floor
/// (`range_already_committed` suppresses NOTHING at `committed == 0`).
pub(in crate::services::discord) fn delivered_frontier_end_current_generation(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    current_transcript_eof: Option<u64>,
) -> u64 {
    let Some(path) = delivery_record_path(provider, channel.get()) else {
        return 0;
    };
    let current_gen = current_generation_mtime_ns(tmux_session_name);
    current_generation_durable_frontier_end_at(&path, current_gen, current_transcript_eof)
        .unwrap_or(0)
}

fn current_generation_frontier_exceeding_eof_at(
    path: &Path,
    current_gen_mtime: i64,
    current_transcript_eof: u64,
) -> Option<u64> {
    read_record_at(path)
        .and_then(|record| record.delivered_frontier)
        .filter(|frontier| {
            durable_frontier_generation_current(frontier.generation_mtime_ns, current_gen_mtime)
                && frontier.range.1 > current_transcript_eof
        })
        .map(|frontier| frontier.range.1)
}

/// #4549: detect the exact same-generation frontier/EOF regression that the
/// ordinary durable reader distrusts. The Claude idle scanner uses this signal
/// only on an unchanged transcript path to re-anchor its scan cursor at EOF after
/// an in-place `/compact` rewrite. A rotated UUID/path bypasses this predicate and
/// keeps the fresh-transcript lookback path, so replacement-file prompts are not
/// lost.
pub(in crate::services::discord) fn delivered_frontier_exceeding_current_eof(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    current_transcript_eof: u64,
) -> Option<u64> {
    let path = delivery_record_path(provider, channel.get())?;
    current_generation_frontier_exceeding_eof_at(
        &path,
        current_generation_mtime_ns(tmux_session_name),
        current_transcript_eof,
    )
}

fn reanchor_current_generation_frontier_at(
    path: &Path,
    current_gen_mtime: i64,
    expected_frontier_end: u64,
    reanchor_offset: u64,
) -> Result<bool, String> {
    let _lock = lock_record_path(path)?;
    let Some(mut record) = read_record_at(path) else {
        return Ok(false);
    };
    let Some(frontier) = record.delivered_frontier.as_mut() else {
        return Ok(false);
    };
    if !durable_frontier_generation_current(frontier.generation_mtime_ns, current_gen_mtime)
        || frontier.range.1 != expected_frontier_end
        || frontier.range.1 <= reanchor_offset
    {
        return Ok(false);
    }
    frontier.range.1 = reanchor_offset;
    if frontier.range.0 > reanchor_offset {
        frontier.range.0 = reanchor_offset;
    }
    write_record_at(path, &record)?;
    Ok(true)
}

/// #4841: converge a stale same-generation durable frontier after an in-place
/// `/compact`. This is a correction of already-confirmed coordinate space, not a
/// new delivery: it preserves the record's generation/attempt/anchor identity and
/// only lowers the END after the caller proved the current transcript boundary.
pub(in crate::services::discord) fn reanchor_current_generation_frontier(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    expected_frontier_end: u64,
    reanchor_offset: u64,
) -> Result<bool, String> {
    let Some(path) = delivery_record_path(provider, channel.get()) else {
        return Ok(false);
    };
    reanchor_current_generation_frontier_at(
        &path,
        current_generation_mtime_ns(tmux_session_name),
        expected_frontier_end,
        reanchor_offset,
    )
}

/// #3089 B2b: the effective "already-committed" offset the dedup/skip gates read.
/// Flag OFF (default) → the legacy in-memory `committed_relay_offset` verbatim
/// (no record read → deploy no-op). Flag ON → `max(delivered_frontier.end,
/// in_memory)` (I3: missing/malformed record → in-memory only, never assume
/// delivered), but ONLY when the durable frontier is from the CURRENT wrapper
/// generation (#1270 guard — a stale prior-generation frontier is treated as
/// `None`) and physically within the current transcript EOF (#4188 guard). The
/// in-memory authority is the relay coord's `confirmed_end_offset` for
/// `channel`; the durable record is keyed by `(provider, channel)`;
/// `tmux_session_name` resolves the current generation watermark, while
/// `current_transcript_eof` bounds the durable offset space.
pub(in crate::services::discord) fn effective_committed_offset(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    current_transcript_eof: Option<u64>,
) -> u64 {
    let in_memory = shared.committed_relay_offset(channel);
    if !delivery_record_authority_enabled() {
        return in_memory;
    }
    let durable_end = delivery_record_path(provider, channel.get()).and_then(|path| {
        current_generation_durable_frontier_end_at(
            &path,
            current_generation_mtime_ns(tmux_session_name),
            current_transcript_eof,
        )
    });
    fuse_committed_offset(durable_end, in_memory)
}

/// #3593 (codex HIGH): the committed floor the synthetic-resume re-send dedup gate
/// (`tmux_watcher.rs`, the non-reconciled `watcher_direct_fallback_intended` arm)
/// reads — the `max` of [`effective_committed_offset`] and the FLAG-INDEPENDENT
/// current-generation durable frontier ([`delivered_frontier_end_current_generation`]).
///
/// Why fuse here and not rely on `effective_committed_offset` alone: under
/// `AGENTDESK_DELIVERY_RECORD_AUTHORITY=OFF` (the default), `effective_committed_offset`
/// returns ONLY the in-memory `committed_relay_offset`. On a restart / synthetic
/// resume that in-memory value is reset to `0`, while the durable frontier still
/// holds the current-generation high watermark (e.g. 443154). With the in-memory
/// floor alone the gate would compute `range_already_committed(422855, 0) == false`
/// and RE-POST the already-delivered body — the legacy #3520 new-message guard read
/// the durable frontier flag-independently, so the new placeholder-path gate must
/// too to be a TRUE superset of #3520 under BOTH authority states.
///
/// Safety (no over-suppression): `max` only RAISES the floor, and the durable reader
/// is current-generation-only (#1270 guard → stale prior-generation frontier yields
/// `0`), so after a pane reset/respawn a genuinely-NEW answer — whose `range_end`
/// sits ABOVE both signals — is never wrongly suppressed.
pub(in crate::services::discord) fn committed_floor_for_resend_dedup(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    current_transcript_eof: Option<u64>,
) -> u64 {
    effective_committed_offset(
        shared,
        provider,
        channel,
        tmux_session_name,
        current_transcript_eof,
    )
    .max(delivered_frontier_end_current_generation(
        provider,
        channel,
        tmux_session_name,
        current_transcript_eof,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct EditFailureTranscriptIdentity {
    output_path: PathBuf,
    generation_mtime_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptFileSnapshot {
    eof: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn transcript_file_snapshot(path: &Path) -> Option<TranscriptFileSnapshot> {
    let metadata = fs::metadata(path).ok()?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Some(TranscriptFileSnapshot {
        eof: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    })
}

/// Capture the transcript identity before the Discord edit await. A missing
/// watcher path or generation marker cannot establish range identity and leaves
/// fallback delivery enabled.
pub(in crate::services::discord) fn capture_edit_failure_transcript_identity(
    shared: &crate::services::discord::SharedData,
    tmux_session_name: &str,
) -> Option<EditFailureTranscriptIdentity> {
    let output_path = PathBuf::from(
        shared
            .tmux_watchers
            .watcher_output_path(tmux_session_name)?,
    );
    let generation_mtime_ns = current_generation_mtime_ns(tmux_session_name);
    if generation_mtime_ns == 0 {
        return None;
    }
    Some(EditFailureTranscriptIdentity {
        output_path,
        generation_mtime_ns,
    })
}

fn stable_edit_failure_frontier_at<P, G, H>(
    record_path: &Path,
    expected: &EditFailureTranscriptIdentity,
    mut current_output_path: P,
    mut current_generation: G,
    after_first_snapshot: H,
) -> Option<DeliveredCommit>
where
    P: FnMut() -> Option<PathBuf>,
    G: FnMut() -> i64,
    H: FnOnce(),
{
    let _lock = lock_record_path(record_path).ok()?;
    if current_output_path().as_deref() != Some(expected.output_path.as_path())
        || current_generation() != expected.generation_mtime_ns
    {
        return None;
    }
    let before = transcript_file_snapshot(&expected.output_path)?;
    after_first_snapshot();

    let frontier = read_record_at(record_path)?.delivered_frontier?;
    if !durable_frontier_generation_current(
        frontier.generation_mtime_ns,
        expected.generation_mtime_ns,
    ) || frontier.range.1 > before.eof
    {
        return None;
    }

    let after_path = current_output_path()?;
    let after_generation = current_generation();
    let after = transcript_file_snapshot(&expected.output_path)?;
    if after_path != expected.output_path
        || after_generation != expected.generation_mtime_ns
        || after != before
    {
        return None;
    }
    Some(frontier)
}

/// Re-read a stable path+generation+EOF+frontier snapshot after an edit failure
/// and decide whether a fallback POST would duplicate a confirmed JSONL range.
/// The expected path and generation were captured before the edit await. The
/// record lock serializes conforming frontier writers, while the post-read
/// identity check detects wrapper rotation or transcript replacement during the
/// snapshot. Any missing or changing component fails open for delivery.
pub(in crate::services::discord) fn range_committed_after_edit_failure(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    expected: Option<&EditFailureTranscriptIdentity>,
    range_end: u64,
) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    let Some(record_path) = delivery_record_path(provider, channel.get()) else {
        return false;
    };
    let frontier = stable_edit_failure_frontier_at(
        &record_path,
        expected,
        || {
            shared
                .tmux_watchers
                .watcher_output_path(tmux_session_name)
                .map(PathBuf::from)
        },
        || current_generation_mtime_ns(tmux_session_name),
        || {},
    );
    range_already_committed(
        range_end,
        frontier.map(|frontier| frontier.range.1).unwrap_or(0),
    )
}

/// #3593 JSONL-space monotonic dedup predicate (pure, testable). `range_end` is the
/// END of the consumed JSONL byte range a watcher pass is about to relay;
/// `committed` is the already-delivered committed JSONL offset (the relay coord's
/// `confirmed_end_offset`, via [`effective_committed_offset`] — which also fuses the
/// current-generation durable frontier when read-authority is enabled). Returns `true` iff this range was ALREADY
/// delivered (`range_end <= committed`), i.e. a re-send would re-post text already
/// relayed (the #3593 synthetic-resume duplicate, where range_end=422855 sits at or
/// below committed=443154). MUST be the SAME JSONL byte-offset space on both sides
/// (never `response_sent_offset`, which is assistant-text-byte space — mixing them
/// is the category error warned about at the watcher resend site).
///
/// `range_end == 0` (empty/zero range) and `committed == 0` (e.g. just reset by a
/// generation change / pane reset, so nothing is known-delivered) both return
/// `false` — suppress NOTHING in those cases, so a genuinely-new answer is never
/// dropped (message-loss prevention dominates duplicate-suppression). Mirror of the
/// `already_relayed` decision both relay consumers make, with an explicit non-zero
/// `range_end` guard so an empty range can never be treated as "already committed".
pub(in crate::services::discord) fn range_already_committed(
    range_end: u64,
    committed: u64,
) -> bool {
    range_end > 0 && range_end <= committed
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn delivered_content_hash(channel_id: u64, body: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&channel_id.to_le_bytes());
    hasher.update(body.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn delivered_content_fingerprint(
    channel_id: u64,
    body: &str,
    generation_mtime_ns: i64,
    delivered_at_epoch_ms: u64,
) -> Option<DeliveredContentFingerprint> {
    (!body.trim().is_empty()).then(|| DeliveredContentFingerprint {
        channel_id,
        content_hash: delivered_content_hash(channel_id, body),
        content_len: body.len() as u64,
        generation_mtime_ns,
        delivered_at_epoch_ms,
    })
}

fn recent_content_fingerprint_matches(
    record: &DeliveryRecord,
    fingerprint: &DeliveredContentFingerprint,
    now_ms: u64,
) -> bool {
    record.recent_delivered_contents.iter().any(|recent| {
        recent.channel_id == fingerprint.channel_id
            && recent.content_len == fingerprint.content_len
            && recent.content_hash == fingerprint.content_hash
            && recent.generation_mtime_ns == fingerprint.generation_mtime_ns
            && now_ms.saturating_sub(recent.delivered_at_epoch_ms)
                <= RECENT_DELIVERED_CONTENT_WINDOW_MS
    })
}

fn prune_recent_content_fingerprints(entries: &mut Vec<DeliveredContentFingerprint>, now_ms: u64) {
    entries.retain(|entry| {
        now_ms.saturating_sub(entry.delivered_at_epoch_ms) <= RECENT_DELIVERED_CONTENT_WINDOW_MS
    });
    if entries.len() > RECENT_DELIVERED_CONTENT_LIMIT {
        entries.drain(0..entries.len() - RECENT_DELIVERED_CONTENT_LIMIT);
    }
}

fn record_delivered_content_fingerprint_at(
    path: &Path,
    channel_id: u64,
    body: &str,
    generation_mtime_ns: i64,
    delivered_at_epoch_ms: u64,
) -> Result<(), String> {
    let Some(fingerprint) =
        delivered_content_fingerprint(channel_id, body, generation_mtime_ns, delivered_at_epoch_ms)
    else {
        return Ok(());
    };
    mutate_record_at(path, |record| {
        prune_recent_content_fingerprints(
            &mut record.recent_delivered_contents,
            delivered_at_epoch_ms,
        );
        record.recent_delivered_contents.retain(|recent| {
            !(recent.channel_id == fingerprint.channel_id
                && recent.content_len == fingerprint.content_len
                && recent.content_hash == fingerprint.content_hash
                && recent.generation_mtime_ns == fingerprint.generation_mtime_ns)
        });
        record.recent_delivered_contents.push(fingerprint);
        prune_recent_content_fingerprints(
            &mut record.recent_delivered_contents,
            delivered_at_epoch_ms,
        );
    })
}

fn recent_delivered_content_matches_at(
    path: &Path,
    channel_id: u64,
    body: &str,
    generation_mtime_ns: i64,
    now_ms: u64,
) -> bool {
    let Some(fingerprint) =
        delivered_content_fingerprint(channel_id, body, generation_mtime_ns, now_ms)
    else {
        return false;
    };
    read_record_at(path)
        .as_ref()
        .is_some_and(|record| recent_content_fingerprint_matches(record, &fingerprint, now_ms))
}

// #4046 S1r-1 P3-2: module-local only — every caller is in this file. Reverted
// from the transient `pub(in ...::outbound)` widening (no caller outside this
// module; behavior unchanged). #5264 PR-B: there are now three call sites —
// `record_delivered_content_fingerprint` and `shadow_mirror_delivered_frontier_inner`
// (the two fresh-send record sites) plus `record_pinned_delivery_metadata`, which
// keys the pinned bridge's fingerprint by the offset-authority channel.
fn record_delivered_content_fingerprint_for_generation(
    provider: &ProviderKind,
    channel_id: u64,
    body: &str,
    generation_mtime_ns: i64,
) {
    let path = match record_path_or_err(provider, channel_id) {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(
                provider = provider.as_str(),
                channel_id,
                error = %error,
                "delivery content fingerprint path unavailable"
            );
            return;
        }
    };
    if let Err(error) = record_delivered_content_fingerprint_at(
        &path,
        channel_id,
        body,
        generation_mtime_ns,
        now_epoch_ms(),
    ) {
        tracing::warn!(
            provider = provider.as_str(),
            channel_id,
            error = %error,
            "delivery content fingerprint write failed"
        );
    }
}

pub(in crate::services::discord::outbound) fn record_fresh_send_content_fingerprint(
    provider: &ProviderKind,
    channel_id: u64,
    body: &str,
    generation_mtime_ns: i64,
) -> Result<(), String> {
    let path = fresh_send_record_path(provider, channel_id)
        .ok_or_else(|| "fresh_send_record: runtime root unavailable".to_string())?;
    record_delivered_content_fingerprint_at(
        &path,
        channel_id,
        body,
        generation_mtime_ns,
        now_epoch_ms(),
    )
}

pub(in crate::services::discord::outbound) fn recent_fresh_send_content_matches(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    body: &str,
) -> bool {
    let Some(path) = fresh_send_record_path(provider, channel.get()) else {
        return false;
    };
    recent_delivered_content_matches_at(
        &path,
        channel.get(),
        body,
        current_generation_mtime_ns(tmux_session_name),
        now_epoch_ms(),
    )
}

pub(in crate::services::discord) fn record_delivered_content_fingerprint(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    body: &str,
) {
    record_delivered_content_fingerprint_for_generation(
        provider,
        channel.get(),
        body,
        current_generation_mtime_ns(tmux_session_name),
    );
}

pub(in crate::services::discord) fn recent_delivered_content_matches(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    body: &str,
) -> bool {
    let Some(path) = delivery_record_path(provider, channel.get()) else {
        return false;
    };
    recent_delivered_content_matches_at(
        &path,
        channel.get(),
        body,
        current_generation_mtime_ns(tmux_session_name),
        now_epoch_ms(),
    )
}

/// I2 outcome map (pure, testable): the confirmed-delivery funnel fires ONLY for
/// `Delivered`. The historical function name is retained for hotfile callers.
/// Every other outcome means the controller did NOT advance the
/// offset — `NotDelivered` (identity gate refused), `Transient`/`Unknown`
/// (ambiguous), `Skipped` (no-op) — so the durable frontier must NOT advance
/// (I2, the #3143/#3416 class). This pins the owner call-site's outcome decision
/// (a frozen-file `matches!` was previously untested — broadening it to include
/// `NotDelivered` slipped through; the variant test now catches it).
pub(in crate::services::discord) fn outcome_is_shadow_delivered(outcome: &DeliveryOutcome) -> bool {
    matches!(outcome, DeliveryOutcome::Delivered { .. })
}

/// Divergence-telemetry gate. Confirmed frontier/receipt persistence does not
/// consult this flag; only the observe-only comparison does.
fn should_shadow_mirror(is_delivered: bool, enabled: bool) -> bool {
    is_delivered && enabled
}

/// M4 divergence predicate (pure, testable): the durable frontier END must equal
/// the in-memory `confirmed_end_offset` just advanced. END is the risky datum.
fn delivered_frontier_end_diverged(durable_end: u64, in_memory_confirmed_end: u64) -> bool {
    durable_end != in_memory_confirmed_end
}

/// Path-based core (testable): write the frontier and report whether its END
/// diverged from the in-memory authority. `Err` only when the durable write
/// itself failed. Caller invokes this ONLY for a confirmed `Delivered` (I2).
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn record_delivered_frontier_shadow_at(
    path: &Path,
    range: (u64, u64),
    generation_mtime_ns: i64,
    attempts: u32,
    panel_msg_id: Option<u64>,
    panel_channel_id: Option<u64>,
    in_memory_confirmed_end: u64,
) -> Result<bool, String> {
    write_delivered_frontier_at(
        path,
        DeliveredCommit {
            range,
            generation_mtime_ns,
            attempts,
            panel_msg_id,
            panel_channel_id,
        },
    )?;
    Ok(delivered_frontier_end_diverged(
        range.1,
        in_memory_confirmed_end,
    ))
}

fn exact_receipt_from_inflight(
    provider: &ProviderKind,
    fresh: Option<&crate::services::discord::inflight::InflightTurnState>,
    offset_authority_channel: ChannelId,
    delivery_channel: ChannelId,
    message_id: Option<u64>,
    range: (u64, u64),
    generation_mtime_ns: i64,
) -> Option<ConfirmedDeliveryReceipt> {
    let fresh = fresh?;
    let message_id = message_id.filter(|id| *id != 0)?;
    let tmux_session_name = fresh
        .tmux_session_name
        .as_deref()
        .filter(|name| !name.is_empty())?;
    let turn_nonce = fresh
        .turn_nonce
        .as_deref()
        .filter(|nonce| !nonce.is_empty())?;
    if fresh.provider_kind().as_ref() != Some(provider)
        || fresh.channel_id != delivery_channel.get()
        || fresh.delivery_record_owner_channel_id() != offset_authority_channel.get()
        || fresh.turn_start_offset != Some(range.0)
        || fresh.last_offset != range.1
        || range.1 <= range.0
        || generation_mtime_ns == 0
        || current_generation_mtime_ns(tmux_session_name) != generation_mtime_ns
    {
        return None;
    }
    Some(ConfirmedDeliveryReceipt {
        source: ExactJsonlSourceIdentity {
            provider: provider.as_str().to_string(),
            tmux_session_name: tmux_session_name.to_string(),
            turn_nonce: turn_nonce.to_string(),
            range,
            generation_mtime_ns,
            offset_authority_channel_id: offset_authority_channel.get(),
            delivery_channel_id: delivery_channel.get(),
        },
        delivery_channel_id: delivery_channel.get(),
        message_id,
    })
}

fn persist_confirmed_frontier_and_receipt(
    provider: &ProviderKind,
    offset_authority_channel: ChannelId,
    tmux_session_name: Option<&str>,
    frontier: DeliveredCommit,
    receipt: Option<ConfirmedDeliveryReceipt>,
    equal_range_anchor_policy: EqualRangeAnchorPolicy,
    lock_authority: Option<WatcherFrontierLockAuthority<'_>>,
) -> Result<(), String> {
    match receipt {
        Some(receipt) => {
            if receipt.source.offset_authority_channel_id != offset_authority_channel.get() {
                return Err("delivery_record: receipt owner does not match record key".into());
            }
            if receipt.source.provider != provider.as_str() {
                return Err("delivery_record: receipt provider does not match record key".into());
            }
            write_confirmed_delivery_at_with_lock_authority(
                &record_path_or_err(provider, offset_authority_channel.get())?,
                frontier,
                receipt,
                equal_range_anchor_policy,
                lock_authority,
            )
        }
        None => {
            let tmux_session_name = tmux_session_name
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    "delivery_record: receipt-less frontier lacks tmux incarnation identity"
                        .to_string()
                })?;
            write_confirmed_frontier_guarded_at_with_lock_authority(
                &record_path_or_err(provider, offset_authority_channel.get())?,
                tmux_session_name,
                frontier,
                None,
                equal_range_anchor_policy,
                lock_authority,
                || {},
            )
        }
    }
}

/// Integration wrapper for owner `Delivered` arms. A confirmed outcome always
/// persists the frontier, and atomically appends an exact receipt when the
/// delivery-channel inflight still exposes a complete turn identity. The
/// rollout flag controls only post-write divergence telemetry. Non-`Delivered`
/// outcomes never write delivery authority (I2).
///
/// #3610 (Phase B PR-1): `terminal_anchor_msg_id` is the durable TERMINAL ANCHOR
/// the caller resolved — the Discord message id terminal-replace edits in place
/// (= the placeholder/active-slot `current_msg_id`, the assistant response
/// message). It is recorded verbatim into [`DeliveredCommit::panel_msg_id`]. This
/// REPLACES the prior `fresh.status_message_id` read, which was the WRONG datum:
/// `status_message_id` is the status-panel-v2 id (inflight/model.rs: "current_msg_id
/// remains the assistant response"), not the terminal anchor, and is `null` on
/// channels that do not use the status panel — yielding `panel_msg_id = null` on
/// the very incidents PR-2's re-post will key off.
///
/// #3610 (Phase B PR-1b): `terminal_anchor_channel_id` is the channel that anchor
/// message LIVES IN — recorded into [`DeliveredCommit::panel_channel_id`]. The
/// frontier is KEYED by `channel` (the OFFSET-AUTHORITY channel,
/// `watcher_owner_channel_id` for the bridge cutover), which the cross-channel
/// cutover can resolve DIFFERENTLY from the edit-target channel the `msg_id`
/// belongs to. So PR-2 must record the (channel, msg) PAIR, not just the msg.
/// Same-channel callers (sink, watcher) pass `Some(channel.get())`; the bridge
/// cutover passes the edit-target `channel_id` it actually edits. `None`/`None`
/// writes a null anchor pair (unchanged from the absent-status-panel case), so
/// A missing anchor cannot produce an exact receipt, but the generation-scoped
/// frontier remains durable.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn shadow_mirror_delivered_frontier(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    authoritative_tmux_session_name: Option<&str>,
    range: (u64, u64),
    is_delivered: bool,
    terminal_anchor_msg_id: Option<u64>,
    terminal_anchor_channel_id: Option<u64>,
    delivered_body: Option<&str>,
    ledger_user_msg_id: Option<u64>,
) {
    let _ = shadow_mirror_delivered_frontier_inner(
        shared,
        provider,
        channel,
        authoritative_tmux_session_name,
        range,
        is_delivered,
        terminal_anchor_msg_id,
        terminal_anchor_channel_id,
        delivered_body,
        ledger_user_msg_id,
        None,
        None,
    );
}

/// The caller-PINNED half of the ledger-id safety rule: an id the caller bound to
/// THIS delivery — either the watcher's delivery-lease authority or the explicit
/// `ledger_user_msg_id` argument that every production terminal-delivery call site
/// passes from its own turn/inflight snapshot. Unlike the receipt-qualified
/// fresh-row fallback it needs neither an inflight re-read nor an authoritative
/// JSONL generation, so it is the only ledger-id source usable on the
/// unknown-generation path where no durable frontier is written (#5154).
fn pinned_ledger_user_msg_id(
    watcher_authority: Option<WatcherDeliveryRecordAuthority>,
    ledger_user_msg_id: Option<u64>,
) -> Option<u64> {
    watcher_authority
        .and_then(|authority| authority.ledger_user_msg_id)
        .or(ledger_user_msg_id)
        .filter(|user_msg_id| *user_msg_id != 0)
}

#[allow(clippy::too_many_arguments)]
fn shadow_mirror_delivered_frontier_inner(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    authoritative_tmux_session_name: Option<&str>,
    range: (u64, u64),
    is_delivered: bool,
    terminal_anchor_msg_id: Option<u64>,
    terminal_anchor_channel_id: Option<u64>,
    delivered_body: Option<&str>,
    ledger_user_msg_id: Option<u64>,
    watcher_authority: Option<WatcherDeliveryRecordAuthority>,
    recovery_authority: Option<RecoveryDeliveryRecordAuthority>,
) -> bool {
    let channel_id = channel.get();
    let coord = shared.tmux_relay_coord(channel);
    let generation_mtime_ns = watcher_authority
        .map(|authority| authority.generation_mtime_ns)
        .or_else(|| recovery_authority.map(|authority| authority.generation_mtime_ns))
        .unwrap_or_else(|| {
            coord
                .confirmed_end_generation_mtime_ns
                .load(Ordering::Acquire)
        });
    if !is_delivered {
        return false;
    }
    if generation_mtime_ns == 0 || range.1 <= range.0 {
        // #5154: the durable frontier is generation-scoped, so without an
        // authoritative JSONL incarnation/range it MUST stay unwritten — that guard
        // is unchanged and just as strict. But "this user message was answered" is
        // NOT a fact about a JSONL incarnation, and the single early return that
        // used to live here also skipped the completed-turn ledger append below.
        // `catch_up/settled_ledger_consult.rs` treats that ledger as the ONLY
        // settled-evidence source, so a confirmed, delivered turn was re-collected
        // by catch-up and its prompt re-submitted verbatim. Settlement recording is
        // therefore separated from frontier persistence.
        //
        // Only CALLER-PINNED ids are used here. The receipt-qualified fresh-row
        // fallback further below is deliberately NOT reproduced: it is qualified by
        // an exact receipt, which cannot be constructed without an authoritative
        // generation, and an unqualified current row could let a delayed A settle a
        // newer B.
        //
        // The #4081 delivered-content fingerprint is deliberately NOT recorded on
        // this path. Its match predicate requires exact generation equality
        // (`recent_content_fingerprint_matches`) against a reader-side FRESH marker
        // read (`recent_delivered_content_matches` → `current_generation_mtime_ns`),
        // so a fingerprint stored under the unknown-generation sentinel `0` becomes
        // permanently unmatchable the moment the generation is re-established, while
        // still consuming a slot in the bounded `recent_delivered_contents` ring. It
        // would look like a duplicate defense without being one. Duplicate defense on
        // this path stays with the ledger; the fingerprint is not restored here.
        let settled_ledger_user_msg_id =
            pinned_ledger_user_msg_id(watcher_authority, ledger_user_msg_id);
        if let Some(user_msg_id) = settled_ledger_user_msg_id {
            completed_turn_ledger::append_completed_turn(
                provider,
                terminal_anchor_channel_id.unwrap_or(channel_id),
                user_msg_id,
            );
        }
        tracing::warn!(
            provider = provider.as_str(),
            channel_id,
            range = ?range,
            generation_mtime_ns,
            ?settled_ledger_user_msg_id,
            "confirmed delivery lacks an authoritative JSONL incarnation/range; preserving prior durable frontier (completed-turn settlement still recorded when the caller pinned an inbound id)"
        );
        return false;
    }
    let in_memory_confirmed_end = coord.confirmed_end_offset.load(Ordering::Acquire);
    let delivery_channel_id = terminal_anchor_channel_id.unwrap_or(channel_id);
    let delivery_channel = ChannelId::new(delivery_channel_id);
    // Capture the delivered turn's nonce/session from its DELIVERY-channel row,
    // never from the offset-authority row. In a channel split the latter can be
    // a different, unanswered turn. This synchronous read+write happens before
    // the caller's inflight clear and closes the clear-before-seed-consume race.
    let fresh =
        crate::services::discord::inflight::load_inflight_state(provider, delivery_channel_id);
    let attempts = recovery_authority.map_or_else(
        || {
            fresh
                .as_ref()
                .map(|f| f.recovery_relay_attempts)
                .unwrap_or(0)
        },
        |authority| authority.attempts,
    );
    // The caller already owns the delivery incarnation while committing its
    // lease. Keep that identity independent of the inflight row: the row may be
    // cleared immediately after the confirmed POST. A fresh exact row remains
    // mandatory for receipt construction below; caller authority only supplies
    // the receipt-less frontier fallback and can never synthesize a receipt.
    let tmux_session_name = authoritative_tmux_session_name
        .filter(|name| !name.is_empty())
        .or_else(|| {
            fresh
                .as_ref()
                .and_then(|state| state.tmux_session_name.as_deref())
                .filter(|name| !name.is_empty())
        });
    let frontier = DeliveredCommit {
        range,
        generation_mtime_ns,
        attempts,
        panel_msg_id: terminal_anchor_msg_id,
        panel_channel_id: terminal_anchor_channel_id,
    };
    let receipt = exact_receipt_from_inflight(
        provider,
        fresh.as_ref(),
        channel,
        delivery_channel,
        terminal_anchor_msg_id,
        range,
        generation_mtime_ns,
    );
    // A ledger id is safe only when the caller pinned it to this delivery or
    // the same fresh row produced the exact receipt above. Never guess from an
    // unqualified current row: a delayed A could otherwise settle newer B.
    let safe_ledger_user_msg_id = pinned_ledger_user_msg_id(watcher_authority, ledger_user_msg_id)
        .or_else(|| {
            receipt
                .as_ref()
                .and_then(|_| fresh.as_ref().map(|state| state.user_msg_id))
                .filter(|user_msg_id| *user_msg_id != 0)
        });
    let lock_authority = watcher_authority.map(|authority| WatcherFrontierLockAuthority {
        shared,
        channel,
        lease_reset_incarnation: authority.lease_reset_incarnation,
        generation_mtime_ns: authority.generation_mtime_ns,
    });
    // #5071 T1 S7: the ONLY policy that is not `PreserveExisting` is the
    // recovery re-anchor Discord has proved GONE. It is selected by the caller's
    // authority rather than by a separate raw writer, so the equal-range CAS and
    // the monotonic merge now sit on one path through this funnel.
    let equal_range_anchor_policy = recovery_authority
        .and_then(|authority| authority.expected_gone_anchor)
        .map_or(
            EqualRangeAnchorPolicy::PreserveExisting,
            |(expected_panel_channel_id, expected_panel_msg_id)| {
                EqualRangeAnchorPolicy::ReplaceProvenGone {
                    expected_panel_channel_id,
                    expected_panel_msg_id,
                }
            },
        );
    if let Err(error) = persist_confirmed_frontier_and_receipt(
        provider,
        channel,
        tmux_session_name,
        frontier,
        receipt,
        equal_range_anchor_policy,
        lock_authority,
    ) {
        tracing::error!(
            provider = provider.as_str(),
            channel_id,
            error = %error,
            "confirmed delivery frontier/receipt write failed"
        );
        return false;
    }

    // Retry evidence and completed-turn settlement become visible only after
    // the guarded frontier/receipt mutation succeeds.
    if let Some(body) = delivered_body {
        record_delivered_content_fingerprint_for_generation(
            provider,
            channel_id,
            body,
            generation_mtime_ns,
        );
    }
    if let Some(user_msg_id) = safe_ledger_user_msg_id {
        append_completed_turn_if_nonzero(provider, delivery_channel_id, user_msg_id);
    }

    // The flag controls only divergence telemetry. The confirmed frontier and
    // exact receipt above are delivery authority and are always persisted.
    if should_shadow_mirror(true, delivery_record_shadow_enabled())
        && delivered_frontier_end_diverged(range.1, in_memory_confirmed_end)
    {
        tracing::error!(
            provider = provider.as_str(),
            channel_id,
            durable_end = range.1,
            in_memory_confirmed_end,
            generation_mtime_ns,
            "#3089 B1: delivered_frontier END diverged from in-memory confirmed_end_offset (observe-only)"
        );
    }
    true
}

pub(in crate::services::discord) fn record_delivered_frontier_with_body(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: Option<&str>,
    range: (u64, u64),
    terminal_anchor_msg_id: u64,
    terminal_anchor_channel_id: u64,
    body: &str,
    ledger_user_msg_id: Option<u64>,
) {
    shadow_mirror_delivered_frontier(
        shared,
        provider,
        channel,
        tmux_session_name,
        range,
        true,
        Some(terminal_anchor_msg_id),
        Some(terminal_anchor_channel_id),
        Some(body),
        ledger_user_msg_id,
    );
}

pub(in crate::services::discord) fn shadow_mirror_same_channel_frontier_with_body(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    range: (u64, u64),
    is_delivered: bool,
    terminal_anchor_msg_id: u64,
    body: &str,
    ledger_user_msg_id: u64,
) {
    shadow_mirror_delivered_frontier(
        shared,
        provider,
        channel,
        Some(tmux_session_name),
        range,
        is_delivered,
        Some(terminal_anchor_msg_id),
        Some(channel.get()),
        Some(body),
        (ledger_user_msg_id != 0).then_some(ledger_user_msg_id),
    );
}

/// #3610 PR-1c: record the durable terminal anchor for the BRIDGE long-chunk arm
/// (`turn_bridge/mod.rs` site 4 — `send_ordered_long_terminal_response`). PR-1/1b
/// instrumented only the short-replace sites (sink/watcher) and the bridge cutover,
/// so a LONG (`len > DISCORD_MSG_LIMIT`) terminal answer — which routes through the
/// send-new-chunks + placeholder-delete path — recorded NO anchor.
///
/// The caller invokes this ONLY on the FULL-COMMIT `Ok` arm of
/// `send_ordered_long_terminal_response` (the send is all-or-nothing — a partial
/// chunk failure rolls back and returns `Err`, never `Ok`) AND ONLY when
/// `lease.commit_and_advance(.., Delivered)` returned `true` — i.e. the in-memory
/// `confirmed_end_offset` actually advanced. That commit-success gate is the M4
/// invariant: a non-Leased / identity-mismatch / reclaimed cell makes
/// `commit_and_advance` return `false` WITHOUT advancing the offset, and recording
/// `delivered_frontier.range = end` in that case would leave the durable frontier
/// END ahead of `confirmed_end_offset` (M4 violation). With both gates satisfied
/// `is_delivered = true` is correct here (mirrors the cutover's
/// `outcome_is_shadow_delivered` gate, which for this arm is unconditionally
/// `Delivered`).
///
/// Channel split (same as the cutover): the frontier is KEYED by
/// `watcher_owner_channel_id` (the OFFSET-AUTHORITY channel where
/// `confirmed_end_offset` advanced — unchanged), while the recorded anchor PAIR is
/// `(panel_channel_id = delivery_channel_id, panel_msg_id = last_chunk_anchor_msg_id)`
/// — the channel/message the tail chunk actually lives in. These differ for a
/// recovered/reused-watcher bridge. `range` is the SAME `(start, end)` the bridge
/// delivery lease acquired/committed (offset-space consistent — never mix spaces).
/// `last_chunk_anchor_msg_id = None` (empty chunk Vec — impossible on the `Ok`
/// path, but type-honest) records the range with a null anchor, identical to the
/// absent-status-panel case. The delivered frontier is flag-independent; an
/// exact receipt additionally requires a real tail anchor and complete inflight
/// identity. The #4081 recent-content fingerprint remains non-authoritative
/// retry metadata and is never used for restored-seed removal.
pub(in crate::services::discord) fn record_long_chunk_terminal_delivery(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    watcher_owner_channel_id: ChannelId,
    delivery_channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    range: (u64, u64),
    last_chunk_anchor_msg_id: Option<u64>,
    delivered_body: &str,
    ledger_user_msg_id: Option<u64>,
) {
    shadow_mirror_delivered_frontier(
        shared,
        provider,
        watcher_owner_channel_id,
        tmux_session_name,
        range,
        true,
        last_chunk_anchor_msg_id,
        Some(delivery_channel_id.get()),
        Some(delivered_body),
        ledger_user_msg_id,
    );
}

/// Watcher-only terminal-delivery funnel. The caller captures generation and turn
/// identity with its delivery lease, while the wrapper holds the matching relay
/// frontier mutation guard through this durable mutation.
pub(in crate::services::discord) fn record_watcher_terminal_delivery(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    authority: WatcherDeliveryRecordAuthority,
    range: (u64, u64),
    last_chunk_anchor_msg_id: Option<u64>,
    delivered_body: &str,
) -> bool {
    shadow_mirror_delivered_frontier_inner(
        shared,
        provider,
        channel_id,
        Some(tmux_session_name),
        range,
        true,
        last_chunk_anchor_msg_id,
        Some(channel_id.get()),
        Some(delivered_body),
        None,
        Some(authority),
        None,
    )
}

/// Recovery-only terminal-delivery funnel (#5071 T1 S7).
///
/// WHAT THIS REPLACES, EXACTLY. Until S7 the recovery path did not reach this
/// funnel at all: `recovery_engine/terminal_text_idempotency.rs` called
/// `write_delivered_frontier` / `write_proven_gone_equal_range_frontier`
/// directly and appended the completed-turn ledger ITSELF, BEFORE either write.
/// Those three call sites are gone; this is the one that took their place. It is
/// a claim about those three writes, not about "every recovery write" — the
/// dormant `turn_output_controller/fresh_send.rs` sites are untouched by S7.
///
/// WHAT JOINING BUYS, MEASURED RATHER THAN ASSERTED:
///   * the delivered-content fingerprint (#4081) is now recorded for a recovery
///     delivery; before S7 nothing on this path wrote one;
///   * the completed-turn ledger append happens AFTER a successful frontier
///     persist on the recordable path, and on the unknown-generation path it
///     still happens — which is the #4564 guarantee the pre-S7 unconditional
///     leading append existed for, now expressed by the funnel's own rule;
///   * an exact `confirmed_deliveries` receipt is CONSTRUCTED THROUGH THE SAME
///     PREDICATE as every other caller (`exact_receipt_from_inflight`). It is
///     NOT thereby produced: that predicate requires the fresh inflight row's
///     `turn_start_offset` to equal `range.0`, and the recovery caller builds
///     its range as `(state.last_offset, confirmed_end)`. Recovery reaching the
///     receipt-less arm is therefore the expected outcome today, exactly as
///     before S7. Joining moved the decision into the funnel; it did not create
///     a receipt.
///
/// WHAT IT DOES NOT BUY. No `WatcherFrontierLockAuthority`. That authority
/// re-validates `coord.confirmed_end_generation_mtime_ns` against the caller's
/// generation under the record flock, and the recovery path never advances the
/// in-memory watermark that stores it, so supplying one would REFUSE precisely
/// the startup-recovery writes this path exists for. That asymmetry is owned by
/// #5071 T1 S7b together with the in-memory advance itself.
pub(in crate::services::discord) fn record_recovery_terminal_delivery(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    offset_authority_channel: ChannelId,
    tmux_session_name: Option<&str>,
    authority: RecoveryDeliveryRecordAuthority,
    delivered_body: &str,
) -> bool {
    let (terminal_anchor_channel_id, terminal_anchor_msg_id) = authority.terminal_anchor;
    shadow_mirror_delivered_frontier_inner(
        shared,
        provider,
        offset_authority_channel,
        tmux_session_name,
        authority.range,
        true,
        Some(terminal_anchor_msg_id),
        Some(terminal_anchor_channel_id),
        Some(delivered_body),
        authority.ledger_user_msg_id,
        None,
        Some(authority),
    )
}

#[cfg(test)]
mod relay_state_contract_refs {
    #[test]
    fn contract_symbols_exist() {
        use super::range_committed_after_edit_failure as _;
    }
}

#[cfg(test)]
mod tests {
    use super::super::delivery_frontier_probe::{
        CurrentGenerationAnchor, current_generation_delivered_anchor_at,
    };
    use super::*;

    fn sample_lease() -> DurableLease {
        DurableLease {
            holder_id: 7,
            attempt_id: 2,
            range: (10, 42),
            deadline_epoch_ms: 1_700_000_000_000,
            holder_generation: 3,
        }
    }

    fn sample_frontier() -> DeliveredCommit {
        DeliveredCommit {
            range: (0, 42),
            generation_mtime_ns: 123_456_789,
            attempts: 1,
            panel_msg_id: Some(999),
            panel_channel_id: Some(1234),
        }
    }

    fn set_phase_a_generation(tmux_session_name: &str, unix_secs: i64) -> i64 {
        let path = crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
        if let Some(parent) = Path::new(&path).parent() {
            fs::create_dir_all(parent).expect("create generation parent");
        }
        fs::write(&path, "phase-a-r2").expect("write generation marker");
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(unix_secs, 123))
            .expect("set deterministic generation mtime");
        current_generation_mtime_ns(tmux_session_name)
    }

    fn exact_delivery_fixture(
        provider: &ProviderKind,
        tmux_session_name: &str,
        turn_nonce: &str,
        range: (u64, u64),
        generation_mtime_ns: i64,
        channels: (u64, u64),
        message_id: u64,
    ) -> (DeliveredCommit, ConfirmedDeliveryReceipt) {
        let (owner_channel_id, delivery_channel_id) = channels;
        (
            DeliveredCommit {
                range,
                generation_mtime_ns,
                attempts: 1,
                panel_msg_id: Some(message_id),
                panel_channel_id: Some(delivery_channel_id),
            },
            ConfirmedDeliveryReceipt {
                source: ExactJsonlSourceIdentity {
                    provider: provider.as_str().to_string(),
                    tmux_session_name: tmux_session_name.to_string(),
                    turn_nonce: turn_nonce.to_string(),
                    range,
                    generation_mtime_ns,
                    offset_authority_channel_id: owner_channel_id,
                    delivery_channel_id,
                },
                delivery_channel_id,
                message_id,
            },
        )
    }

    #[test]
    fn codex_admitted_range_receipt_is_exact_and_idempotent_5264() {
        let _root = IsolatedRoot::new();
        let (provider, delivery, owner) = (ProviderKind::Codex, 5_264_001, 5_264_009);
        let tmux = "AgentDesk-codex-5264-pinned";
        let g1 = set_phase_a_generation(tmux, 1_700_526_400);
        #[rustfmt::skip]
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(), delivery, None, 1, 5_264_003, 5_264_002, "answer".into(),
            None, Some(tmux.into()), None, None, 0);
        (state.last_offset, state.turn_nonce) = (3, Some("turn-nonce".into()));
        #[rustfmt::skip]
        let receipt = |state: &crate::services::discord::inflight::InflightTurnState| exact_receipt_from_inflight(
            &provider, Some(state), ChannelId::new(delivery), ChannelId::new(delivery),
            Some(5_264_002), (0, 3), g1);
        let exact = receipt(&state).expect("exact current receipt");
        state.turn_start_offset = Some(1);
        assert!(receipt(&state).is_none(), "mismatched start is not exact");
        #[rustfmt::skip]
        let (frontier, _) = exact_delivery_fixture(&provider, tmux, "turn-nonce", (0, 3),
            g1, (delivery, delivery), 5_264_002);
        for _ in 0..2 {
            write_confirmed_delivery(&provider, delivery, frontier.clone(), exact.clone()).unwrap();
        }
        assert_eq!(
            read_record(&provider, delivery)
                .unwrap()
                .confirmed_deliveries,
            vec![exact]
        );
        #[rustfmt::skip]
        let (_, historical) = exact_delivery_fixture(&provider, tmux, "old-turn", (3, 6),
            g1, (owner, delivery), 5_264_006);
        let g2 = set_phase_a_generation(tmux, 1_800_526_400);
        #[rustfmt::skip]
        let (current, current_receipt) = exact_delivery_fixture(&provider, tmux, "new-turn", (0, 7),
            g2, (owner, delivery), 5_264_007);
        write_confirmed_delivery(&provider, owner, current.clone(), current_receipt).unwrap();
        record_historical_pinned_delivery(&historical.source, historical.message_id).unwrap();
        let record = read_record(&provider, owner).unwrap();
        assert_eq!(
            record.delivered_frontier,
            Some(current),
            "G2 frontier is preserved"
        );
        assert!(historical_pinned_delivery_exists(
            &historical.source,
            historical.message_id
        ));
        let source = &historical.source;
        let wrong = ExactJsonlSourceIdentity {
            generation_mtime_ns: g2,
            ..source.clone()
        };
        assert!(!historical_pinned_delivery_exists(
            &wrong,
            historical.message_id
        ));
        std::fs::remove_file(crate::services::tmux_common::session_temp_path(
            tmux,
            "generation",
        ))
        .unwrap();
        assert!(historical_pinned_delivery_exists(
            source,
            historical.message_id
        ));
    }

    /// #5264 PR-B: the two Discord-identity guards at the top of
    /// `confirmed_delivery_receipt_exists` now stand IN FRONT of the
    /// `historical_pinned_delivery_exists` fast path, and that fast path keys
    /// itself entirely off the caller-supplied `source` — it never looks at the
    /// `provider`/`delivery_channel` the caller is actually asking about. So
    /// deleting either guard makes the function answer "yes, this exact receipt
    /// is confirmed" for a receipt belonging to a DIFFERENT provider or a
    /// DIFFERENT destination channel, which is precisely the restored-seed
    /// consumption decision the doc comment promises fails closed. Both
    /// deletions were silent against the whole suite before this test.
    #[test]
    fn confirmed_receipt_identity_guards_gate_the_historical_fast_path_5264() {
        let _root = IsolatedRoot::new();
        let (provider, owner, delivery) = (ProviderKind::Codex, 5_264_201, 5_264_202);
        let tmux = "AgentDesk-codex-5264-receipt-guard";
        let generation = set_phase_a_generation(tmux, 1_700_526_402);
        #[rustfmt::skip]
        let (_, historical) = exact_delivery_fixture(&provider, tmux, "guard-turn", (0, 9),
            generation, (owner, delivery), 5_264_203);
        record_historical_pinned_delivery(&historical.source, historical.message_id)
            .expect("seed the exact historical receipt");
        let source = &historical.source;
        assert!(
            confirmed_delivery_receipt_exists(
                &provider,
                ChannelId::new(delivery),
                historical.message_id,
                source,
            ),
            "the matching query must still resolve, or the negatives below are vacuous"
        );
        assert!(
            !confirmed_delivery_receipt_exists(
                &ProviderKind::Claude,
                ChannelId::new(delivery),
                historical.message_id,
                source,
            ),
            "a receipt recorded for codex must not confirm a claude consumption"
        );
        assert!(
            !confirmed_delivery_receipt_exists(
                &provider,
                ChannelId::new(owner),
                historical.message_id,
                source,
            ),
            "a receipt whose destination was the delivery channel must not confirm a \
             consumption asking about the offset-authority channel"
        );
        assert!(
            !confirmed_delivery_receipt_exists(&provider, ChannelId::new(delivery), 0, source,),
            "message id 0 is not a delivery"
        );
    }
    #[test]
    fn ordered_jsonl_commit_is_generation_scoped_and_monotonic() {
        let _root = IsolatedRoot::new();
        let path = record_path_or_err(&ProviderKind::Claude, 2_115).expect("record path");
        let tmux = "AgentDesk-claude-ordered-monotonic";
        let generation_1 = set_phase_a_generation(tmux, 1_700_002_115);

        assert!(commit_ordered_jsonl_range_at(&path, tmux, (100, 200), generation_1).unwrap());
        assert!(commit_ordered_jsonl_range_at(&path, tmux, (150, 180), generation_1).unwrap());
        assert_eq!(
            read_record_at(&path).unwrap().delivered_frontier.unwrap(),
            DeliveredCommit {
                range: (100, 200),
                generation_mtime_ns: generation_1,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            }
        );

        let generation_2 = set_phase_a_generation(tmux, 1_700_002_116);
        assert_ne!(generation_1, generation_2);
        assert!(commit_ordered_jsonl_range_at(&path, tmux, (0, 50), generation_2).unwrap());
        assert_eq!(
            read_record_at(&path)
                .unwrap()
                .delivered_frontier
                .unwrap()
                .range,
            (0, 50),
            "a replacement wrapper starts a new coordinate space"
        );
        assert!(!commit_ordered_jsonl_range_at(&path, tmux, (50, 50), generation_2).unwrap());
        assert!(!commit_ordered_jsonl_range_at(&path, tmux, (50, 60), 0).unwrap());
    }

    #[test]
    fn delivery_record_serde_roundtrip() {
        let record = DeliveryRecord {
            delivery_lease: Some(sample_lease()),
            delivered_frontier: Some(sample_frontier()),
            recent_delivered_contents: Vec::new(),
            confirmed_deliveries: Vec::new(),
        };
        let json = serde_json::to_string_pretty(&record).unwrap();
        let back: DeliveryRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
    }

    #[test]
    fn empty_object_deserializes_to_none_none() {
        // #[serde(default)] lets an old/partial `{}` parse to both-None.
        let back: DeliveryRecord = serde_json::from_str("{}").unwrap();
        assert_eq!(back, DeliveryRecord::default());
        assert!(back.delivery_lease.is_none() && back.delivered_frontier.is_none());
    }

    #[test]
    fn atomic_write_then_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 444);
        let record = DeliveryRecord {
            delivery_lease: Some(sample_lease()),
            delivered_frontier: Some(sample_frontier()),
            recent_delivered_contents: Vec::new(),
            confirmed_deliveries: Vec::new(),
        };
        write_record_at(&path, &record).unwrap();
        assert_eq!(read_record_at(&path), Some(record));
    }

    #[test]
    fn degenerate_content_guard_matches_byte_identical_recent_delivery_4081() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 4081);
        let generation = 44_081_i64;
        let now = 1_700_000_000_000_u64;

        record_delivered_content_fingerprint_at(&path, 4081, "prior body", generation, now)
            .unwrap();

        assert!(recent_delivered_content_matches_at(
            &path,
            4081,
            "prior body",
            generation,
            now + 1,
        ));
        assert!(!recent_delivered_content_matches_at(
            &path,
            4081,
            "different body",
            generation,
            now + 1,
        ));
        assert!(!recent_delivered_content_matches_at(
            &path,
            4082,
            "prior body",
            generation,
            now + 1,
        ));
        assert!(!recent_delivered_content_matches_at(
            &path,
            4081,
            "prior body",
            generation + 1,
            now + 1,
        ));
    }

    #[test]
    fn recent_delivered_content_ring_is_bounded_4081() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 4082);
        let generation = 44_082_i64;
        let now = 1_700_000_000_000_u64;

        for idx in 0..(RECENT_DELIVERED_CONTENT_LIMIT + 4) {
            record_delivered_content_fingerprint_at(
                &path,
                4082,
                &format!("body-{idx}"),
                generation,
                now + idx as u64,
            )
            .unwrap();
        }

        let record = read_record_at(&path).unwrap();
        assert_eq!(
            record.recent_delivered_contents.len(),
            RECENT_DELIVERED_CONTENT_LIMIT
        );
        assert!(!recent_delivered_content_matches_at(
            &path,
            4082,
            "body-0",
            generation,
            now + 100,
        ));
        assert!(recent_delivered_content_matches_at(
            &path,
            4082,
            "body-19",
            generation,
            now + 100,
        ));
    }

    #[test]
    fn recent_delivered_content_window_expires_4081() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 4083);
        let generation = 44_083_i64;
        let now = 1_700_000_000_000_u64;

        record_delivered_content_fingerprint_at(&path, 4083, "prior body", generation, now)
            .unwrap();

        assert!(!recent_delivered_content_matches_at(
            &path,
            4083,
            "prior body",
            generation,
            now + RECENT_DELIVERED_CONTENT_WINDOW_MS + 1,
        ));
    }

    #[test]
    fn missing_record_reads_none_conservative() {
        // I3: an absent file is not-yet-delivered, not an error/assume-delivered.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Codex, 5);
        assert_eq!(read_record_at(&path), None);
    }

    #[test]
    fn malformed_record_reads_none_conservative() {
        // I3: a torn/garbage record reads as None (no panic, no Err).
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 6);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not valid json").unwrap();
        assert_eq!(read_record_at(&path), None);
    }

    #[test]
    fn clear_lease_preserves_frontier() {
        // §4.3: release clears the lease ONLY; the frontier survives. Drives the
        // PRODUCTION `clear_lease_at` (not `mutate_record_at`) so a mutation that
        // also cleared the frontier here is caught.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 7);
        write_delivered_frontier_at(&path, sample_frontier()).unwrap();
        upsert_lease_at(&path, sample_lease()).unwrap();
        clear_lease_at(&path).unwrap();
        let after = read_record_at(&path).unwrap();
        assert!(after.delivery_lease.is_none());
        assert_eq!(after.delivered_frontier, Some(sample_frontier()));
    }

    #[test]
    fn lease_upsert_preserves_existing_frontier() {
        // I2: acquiring a lease after a frontier exists never advances/clears it.
        // Drives the production `write_delivered_frontier_at` / `upsert_lease_at`.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 8);
        write_delivered_frontier_at(&path, sample_frontier()).unwrap();
        upsert_lease_at(&path, sample_lease()).unwrap();
        let after = read_record_at(&path).unwrap();
        assert_eq!(after.delivery_lease, Some(sample_lease()));
        assert_eq!(after.delivered_frontier, Some(sample_frontier()));
    }

    #[test]
    fn watcher_owner_context_preserves_frontier_and_resolves_current_generation_3751() {
        let dir = tempfile::tempdir().unwrap();
        let record_path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 123);
        let context_path =
            delivery_owner_context_path_in_root(dir.path(), &ProviderKind::Claude, 123);
        write_delivered_frontier_at(&record_path, sample_frontier()).unwrap();
        write_watcher_owner_context_at(&context_path, 456, "AgentDesk-claude-foo", 700).unwrap();

        let after = read_record_at(&record_path).unwrap();
        assert_eq!(after.delivered_frontier, Some(sample_frontier()));
        assert_eq!(
            watcher_owner_channel_from_context_at(&context_path, "AgentDesk-claude-foo", 700),
            Some(456)
        );
    }

    #[test]
    fn watcher_owner_context_rejects_stale_or_wrong_session_3751() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_owner_context_path_in_root(dir.path(), &ProviderKind::Claude, 123);
        write_watcher_owner_context_at(&path, 456, "AgentDesk-claude-foo", 700).unwrap();

        assert_eq!(
            watcher_owner_channel_from_context_at(&path, "AgentDesk-claude-bar", 700),
            None
        );
        assert_eq!(
            watcher_owner_channel_from_context_at(&path, "AgentDesk-claude-foo", 701),
            None
        );
    }

    #[test]
    fn watcher_owner_context_survives_delivery_record_rewrite_3751() {
        let dir = tempfile::tempdir().unwrap();
        let record_path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 123);
        let context_path =
            delivery_owner_context_path_in_root(dir.path(), &ProviderKind::Claude, 123);
        write_watcher_owner_context_at(&context_path, 456, "AgentDesk-claude-foo", 700).unwrap();

        // Simulates an old binary that knows only the lease/frontier record shape:
        // it rewrites the delivery record JSON object, but never opens the separate
        // owner-context subtree.
        write_delivered_frontier_at(&record_path, sample_frontier()).unwrap();
        upsert_lease_at(&record_path, sample_lease()).unwrap();
        clear_lease_at(&record_path).unwrap();

        assert_eq!(
            watcher_owner_channel_from_context_at(&context_path, "AgentDesk-claude-foo", 700),
            Some(456)
        );
    }

    #[test]
    fn only_frontier_writer_advances_frontier() {
        // I2: a lease-only sequence (acquire→release) never produces a frontier.
        // Drives the production `upsert_lease_at` / `clear_lease_at`.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 9);
        upsert_lease_at(&path, sample_lease()).unwrap();
        clear_lease_at(&path).unwrap();
        let after = read_record_at(&path).unwrap();
        assert!(after.delivered_frontier.is_none());
    }

    #[test]
    fn delete_record_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 10);
        write_record_at(&path, &DeliveryRecord::default()).unwrap();
        assert!(path.exists());
        assert!(delete_record_at(&path)); // production GC helper
        assert!(!path.exists());
    }

    /// THE B0 deliverable: the sidecar path is isolated from the old-binary
    /// reaper by DIRECTORY, not by extension. The reaper does a non-recursive
    /// `read_dir(runtime/discord_inflight/<provider>/)` and reaps `*.json`
    /// there; our record lives under a sibling `discord_delivery_records/`
    /// subtree it never descends into.
    #[test]
    fn sidecar_path_is_outside_inflight_scan_set() {
        let runtime_root = Path::new("/tmp/adk-test-runtime");
        let provider = ProviderKind::Claude;
        let record = delivery_record_path_in_root(runtime_root, &provider, 444);
        let owner_context = delivery_owner_context_path_in_root(runtime_root, &provider, 444);

        // The reaper's scan dir (it builds this exact path; we reconstruct it).
        let inflight_provider_dir = runtime_root
            .join("discord_inflight")
            .join(provider.as_str());

        let record_dir = record.parent().unwrap();

        // 1. The record is NOT inside the scanned inflight provider dir.
        assert_ne!(record_dir, inflight_provider_dir.as_path());
        // 2. And not anywhere beneath the inflight subtree (read_dir is
        //    non-recursive, but assert the stronger property anyway).
        assert!(!record.starts_with(runtime_root.join("discord_inflight")));
        // 3. The isolating segment directly under runtime/ is the dedicated
        //    sidecar dir, not the reaper's target.
        assert!(record.starts_with(runtime_root.join(DELIVERY_RECORDS_DIR)));
        assert!(owner_context.starts_with(runtime_root.join(DELIVERY_OWNER_CONTEXT_DIR)));
        assert_ne!(record.parent(), owner_context.parent());
        assert_ne!(DELIVERY_RECORDS_DIR, "discord_inflight");
        assert_ne!(DELIVERY_OWNER_CONTEXT_DIR, "discord_inflight");
        assert_ne!(DELIVERY_OWNER_CONTEXT_DIR, DELIVERY_RECORDS_DIR);
        // #4046 S1r-1 P3-1: pin the fresh-send sidecar dir out of the reaper's scan set
        // too. The reaper scans `discord_inflight`; a regression that renamed
        // `FRESH_SEND_RECORDS_DIR` to `"discord_inflight"` (or collided it with the
        // delivery-records dir) would make the reaper reap live fresh-send records, and
        // fresh_send_tests' behavioral isolation does NOT cover the dir-name identity.
        // This const pin does.
        assert_ne!(FRESH_SEND_RECORDS_DIR, "discord_inflight");
        assert_ne!(FRESH_SEND_RECORDS_DIR, DELIVERY_RECORDS_DIR);
    }

    // ---- #3089 B1 shadow-write ----------------------------------------------

    #[test]
    fn outcome_is_shadow_delivered_only_for_delivered() {
        // I2: ONLY a confirmed Delivered shadow-writes; every non-advancing
        // outcome (NotDelivered/Transient/Unknown/Skipped) is false. Pins the
        // sink's frozen-file outcome decision (broadening it to NotDelivered now
        // fails here).
        assert!(outcome_is_shadow_delivered(&DeliveryOutcome::Delivered {
            committed_to: 5,
            replace_kind: None,
            new_chunks: None,
        }));
        assert!(!outcome_is_shadow_delivered(
            &DeliveryOutcome::FreshDelivered {
                committed_to: Some(5),
                persistence_recorded: false,
            }
        ));
        assert!(!outcome_is_shadow_delivered(
            &DeliveryOutcome::NotDelivered { committed_from: 5 }
        ));
        assert!(!outcome_is_shadow_delivered(&DeliveryOutcome::Transient {
            retry_from_offset: 0
        }));
        assert!(!outcome_is_shadow_delivered(&DeliveryOutcome::Unknown {
            fell_back: false
        }));
        assert!(!outcome_is_shadow_delivered(&DeliveryOutcome::Skipped));
    }

    #[test]
    fn should_shadow_mirror_requires_delivered_and_enabled() {
        // The legacy flag gates only divergence telemetry. Confirmed authority
        // persistence is pinned independently by the Phase A test below.
        assert!(should_shadow_mirror(true, true));
        assert!(!should_shadow_mirror(false, true));
        assert!(!should_shadow_mirror(true, false));
        assert!(!should_shadow_mirror(false, false));
    }

    #[test]
    fn authority_blocks_backward_inflight_write_truth_table() {
        // #3416 (#3089 B3): ENFORCE only when authority is ON AND a same-turn
        // offset moved backward. OFF → never blocks (observe-only, no-op deploy).
        // authority OFF → false regardless of the monotonic flags. The 4th arg
        // (#3933 is_legitimate_full_reset) is `false` for every genuine-regression
        // row here — it only ever RELAXES, never tightens.
        assert!(!authority_blocks_backward_inflight_write(
            false, false, false, false
        ));
        assert!(!authority_blocks_backward_inflight_write(
            false, true, true, false
        ));
        // authority ON, both monotonic OK → permit the write.
        assert!(!authority_blocks_backward_inflight_write(
            true, true, true, false
        ));
        // authority ON, a backward move on EITHER offset → block (pins the OR).
        assert!(authority_blocks_backward_inflight_write(
            true, false, true, false
        ));
        assert!(authority_blocks_backward_inflight_write(
            true, true, false, false
        ));
        assert!(authority_blocks_backward_inflight_write(
            true, false, false, false
        ));

        // #3933: a legitimate full reset (empty body + rso→0, same turn) must be
        // PERMITTED even under authority-ON with a backward move — otherwise the
        // Gemini/Qwen retry re-stream is dropped. The carve-out only fires for a
        // backward move; a fully-monotonic forward write is unaffected either way.
        assert!(!authority_blocks_backward_inflight_write(
            true, false, true, true
        ));
        assert!(!authority_blocks_backward_inflight_write(
            true, true, false, true
        ));
        assert!(!authority_blocks_backward_inflight_write(
            true, false, false, true
        ));
        // The reset flag never turns a permitted write into a block.
        assert!(!authority_blocks_backward_inflight_write(
            true, true, true, true
        ));
        // Authority OFF stays a no-op regardless of the reset flag.
        assert!(!authority_blocks_backward_inflight_write(
            false, false, false, true
        ));
    }

    #[test]
    fn delivery_record_rollout_health_reports_off_as_observable_warning() {
        let json = delivery_record_rollout_health_json_for_flags(
            FlagResolution::compiled_default(false),
            FlagResolution::compiled_default(false),
        );
        assert_eq!(json["mode"], "off");
        assert_eq!(json["shadow_enabled"], false);
        assert_eq!(json["authority_enabled"], false);
        assert_eq!(json["dedup_authority"], "in_memory_committed_offset");
        assert_eq!(json["same_turn_backward_write_enforcement"], "observe_only");
        assert_eq!(json["warning_count"], 1);
        assert!(
            json["configuration_warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning
                    .as_str()
                    .unwrap()
                    .starts_with("delivery_record_authority_disabled"))
        );
    }

    #[test]
    fn delivery_record_rollout_health_reports_enforcing_modes() {
        let shadow_only = delivery_record_rollout_health_json_for_flags(
            FlagResolution::compiled_default(true),
            FlagResolution::compiled_default(false),
        );
        assert_eq!(shadow_only["mode"], "shadow_only");
        assert_eq!(shadow_only["warning_count"], 1);

        let authority_only = delivery_record_rollout_health_json_for_flags(
            FlagResolution::compiled_default(false),
            FlagResolution::compiled_default(true),
        );
        assert_eq!(authority_only["mode"], "authority_only");
        assert_eq!(
            authority_only["same_turn_backward_write_enforcement"],
            "enforcing"
        );
        assert_eq!(authority_only["warning_count"], 1);

        let enforcing = delivery_record_rollout_health_json_for_flags(
            FlagResolution::compiled_default(true),
            FlagResolution::compiled_default(true),
        );
        assert_eq!(enforcing["mode"], "shadow_and_authority");
        assert_eq!(
            enforcing["dedup_authority"],
            "durable_delivery_record_frontier"
        );
        assert_eq!(
            enforcing["same_turn_backward_write_enforcement"],
            "enforcing"
        );
        assert_eq!(enforcing["warning_count"], 0);
    }

    #[test]
    fn rollout_flag_resolvers_separate_provenance_from_value() {
        // #5071 T1 S8-1r2 gate (a): absence and an explicit off are the SAME
        // value and DIFFERENT provenance. Once the compiled default flips ON, a
        // stale `=1` pin becomes invisible to `enabled` alone, so provenance is
        // the only thing that can still name it.
        assert_eq!(
            resolve_authority_flag(None),
            FlagResolution::compiled_default(false)
        );
        assert_eq!(
            resolve_authority_flag(Some("1")),
            FlagResolution::env_override(true)
        );
        assert_eq!(
            resolve_authority_flag(Some("0")),
            FlagResolution::env_override(false)
        );

        // Shadow resolves identically today; S8-2r2 moves only the authority axis.
        assert_eq!(
            resolve_shadow_flag(None),
            FlagResolution::compiled_default(false)
        );
        assert_eq!(
            resolve_shadow_flag(Some("1")),
            FlagResolution::env_override(true)
        );
        assert_eq!(
            resolve_shadow_flag(Some("0")),
            FlagResolution::env_override(false)
        );

        // Value parsing is unchanged from the pre-extraction inline reads: trimmed,
        // case-insensitive, and ON only for `1`/`true`.
        assert_eq!(
            resolve_authority_flag(Some(" TRUE ")),
            FlagResolution::env_override(true)
        );
        assert_eq!(
            resolve_authority_flag(Some(" 1 ")),
            FlagResolution::env_override(true)
        );
        // Every other present value is off BUT still an override, including the
        // empty string that an exported-but-blank `launchd.env` line produces.
        for raw in ["", "  ", "yes", "false", "2"] {
            assert_eq!(
                resolve_authority_flag(Some(raw)),
                FlagResolution::env_override(false),
                "present value {raw:?} must stay an override"
            );
        }
    }

    #[test]
    fn delivery_record_rollout_health_reports_flag_source_across_truth_table() {
        // #5071 T1 S8-1r2 gate (b): provenance is emitted per axis, independently
        // of the mode the two values combine into.
        for (shadow_enabled, authority_enabled) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let compiled = delivery_record_rollout_health_json_for_flags(
                FlagResolution::compiled_default(shadow_enabled),
                FlagResolution::compiled_default(authority_enabled),
            );
            assert_eq!(
                compiled["flag_source"],
                serde_json::json!({"shadow": "compiled_default", "authority": "compiled_default"}),
                "compiled-branch row ({shadow_enabled}, {authority_enabled})"
            );

            let pinned = delivery_record_rollout_health_json_for_flags(
                FlagResolution::env_override(shadow_enabled),
                FlagResolution::env_override(authority_enabled),
            );
            assert_eq!(
                pinned["flag_source"],
                serde_json::json!({"shadow": "env_override", "authority": "env_override"}),
                "pinned row ({shadow_enabled}, {authority_enabled})"
            );

            // The two axes are reported independently, which is what lets health
            // name mac-book's half-recovered state after only one pin is dropped.
            let mixed = delivery_record_rollout_health_json_for_flags(
                FlagResolution::env_override(shadow_enabled),
                FlagResolution::compiled_default(authority_enabled),
            );
            assert_eq!(
                mixed["flag_source"],
                serde_json::json!({"shadow": "env_override", "authority": "compiled_default"}),
                "mixed row ({shadow_enabled}, {authority_enabled})"
            );

            // Provenance never perturbs the five rollout facts it accompanies.
            assert_eq!(compiled["mode"], pinned["mode"]);
            assert_eq!(compiled["dedup_authority"], pinned["dedup_authority"]);
            assert_eq!(
                compiled["same_turn_backward_write_enforcement"],
                pinned["same_turn_backward_write_enforcement"]
            );
            assert_eq!(compiled["warning_count"], pinned["warning_count"]);
        }
    }

    #[test]
    fn delivered_frontier_end_divergence_predicate() {
        // M4: END must equal the in-memory authority.
        assert!(!delivered_frontier_end_diverged(42, 42));
        assert!(delivered_frontier_end_diverged(42, 41));
        assert!(delivered_frontier_end_diverged(0, 7));
    }

    #[test]
    fn shadow_writes_frontier_and_reports_match() {
        // flag-ON Delivered path: writes the exact frontier and reports no
        // divergence when the durable END equals the in-memory authority.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 11);
        let diverged =
            record_delivered_frontier_shadow_at(&path, (3, 10), 111, 2, Some(9), Some(8), 10)
                .unwrap();
        assert!(!diverged);
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(
            written,
            DeliveredCommit {
                range: (3, 10),
                generation_mtime_ns: 111,
                attempts: 2,
                panel_msg_id: Some(9),
                panel_channel_id: Some(8),
            }
        );
    }

    #[test]
    fn replace_mirror_gate_records_only_delivered_committed_3630() {
        let dir = tempfile::tempdir().unwrap();
        let delivered_path =
            delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36_300);
        let not_delivered_path =
            delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36_301);

        let replace_committed = true;
        let committed = true;
        if replace_committed && committed {
            record_delivered_frontier_shadow_at(
                &delivered_path,
                (7, 77),
                123,
                0,
                Some(555),
                Some(999),
                77,
            )
            .unwrap();
        }
        assert_eq!(
            read_record_at(&delivered_path)
                .unwrap()
                .delivered_frontier
                .unwrap()
                .range,
            (7, 77)
        );

        // `commit_and_advance(.., NotDelivered)` can still return true, but the
        // in-memory confirmed_end_offset did not advance, so M4 requires no record.
        let replace_committed = false;
        let committed = true;
        if replace_committed && committed {
            record_delivered_frontier_shadow_at(
                &not_delivered_path,
                (7, 77),
                123,
                0,
                Some(555),
                Some(999),
                0,
            )
            .unwrap();
        }
        assert!(read_record_at(&not_delivered_path).is_none());
    }

    #[test]
    fn shadow_reports_divergence_but_still_writes_and_never_panics() {
        // A durable-vs-memory END mismatch is observe-only: it reports `true`
        // (logged upstream) and STILL writes the frontier — no panic.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 12);
        let diverged =
            record_delivered_frontier_shadow_at(&path, (3, 10), 222, 0, None, None, 9).unwrap();
        assert!(diverged); // 10 (durable end) != 9 (in-memory)
        assert_eq!(
            read_record_at(&path)
                .unwrap()
                .delivered_frontier
                .unwrap()
                .range,
            (3, 10)
        );
    }

    #[test]
    fn shadow_write_preserves_existing_lease() {
        // B1 shadow-writes ONLY the frontier (I2): a pre-existing lease survives.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 13);
        upsert_lease_at(&path, sample_lease()).unwrap();
        record_delivered_frontier_shadow_at(&path, (0, 5), 5, 0, None, None, 5).unwrap();
        let after = read_record_at(&path).unwrap();
        assert_eq!(after.delivery_lease, Some(sample_lease()));
        assert_eq!(after.delivered_frontier.unwrap().range, (0, 5));
    }

    // ---- #3089 B2b authority flip (fusion) ----------------------------------

    #[test]
    fn fuse_committed_offset_conservative_max() {
        // The durable frontier can only RAISE the dedup floor above in-memory,
        // never lower it. A missed durable write (None, I3) → pure in-memory =
        // today's behavior. Mutation target: flipping max→min or dropping the
        // in-memory fusion would surface here.
        assert_eq!(fuse_committed_offset(None, 20), 20); // I3: missing → in-memory
        assert_eq!(fuse_committed_offset(Some(30), 20), 30); // durable raises floor
        assert_eq!(fuse_committed_offset(Some(10), 20), 20); // stale-low durable never lowers
        assert_eq!(fuse_committed_offset(Some(0), 0), 0);
    }

    #[test]
    fn generation_guard_distrusts_prior_and_unknown_generations() {
        // #1270: trust the durable frontier ONLY if its generation_mtime_ns equals
        // the CURRENT wrapper generation. A prior-generation (stale-high) frontier
        // must be distrusted → not fused → no false skip of fresh output.
        assert!(durable_frontier_generation_current(123, 123)); // same generation → trust
        assert!(!durable_frontier_generation_current(100, 123)); // prior gen → distrust
        assert!(!durable_frontier_generation_current(123, 0)); // no .generation file → distrust
        assert!(!durable_frontier_generation_current(0, 0)); // both unknown → distrust
    }

    // ---- #3593 JSONL-space monotonic resend dedup (range_already_committed) ----

    #[test]
    fn range_already_committed_suppresses_synthetic_resume_dup_3593() {
        // The observed #3593 case: a synthetic-resume pass tries to re-relay the
        // prior turn's body whose consumed JSONL range ends at 422855, but the
        // committed/delivered JSONL offset has already advanced to 443154. The
        // range is entirely below the committed floor → already delivered → suppress
        // (no duplicate).
        assert!(range_already_committed(422_855, 443_154));
    }

    #[test]
    fn range_already_committed_allows_new_response_after_resume() {
        // Over-suppression no-regression: a genuinely-NEW response produced AFTER the
        // resume ends past the committed floor (range_end 443200 > committed 443154)
        // → NOT already delivered → must be relayed.
        assert!(!range_already_committed(443_200, 443_154));
    }

    #[test]
    fn range_already_committed_boundary_equal_is_committed() {
        // The boundary: range_end == committed means the committed floor already
        // covers this exact range end → already delivered → suppress (inclusive `<=`,
        // matching the `already_relayed` consumers' `committed >= range_end`).
        assert!(range_already_committed(443_154, 443_154));
    }

    #[test]
    fn range_already_committed_zero_range_never_suppressed() {
        // Empty/zero consumed range (range_end == 0) must NEVER be treated as already
        // committed regardless of the committed value — suppressing it could drop a
        // message; message-loss prevention dominates duplicate-suppression.
        assert!(!range_already_committed(0, 443_154));
        assert!(!range_already_committed(0, 0));
    }

    #[test]
    fn range_already_committed_reset_committed_never_suppresses() {
        // After a generation change / pane reset the committed offset is reset to 0
        // (nothing is known-delivered in the fresh generation). A real range_end then
        // sits ABOVE the 0 floor → NOT already committed → fresh output is relayed,
        // never wrongly suppressed by a stale-high pre-reset value.
        assert!(!range_already_committed(422_855, 0));
        assert!(!range_already_committed(1, 0));
    }

    // ---- #3593 flag-independent current-generation durable frontier reader ----
    // (codex HIGH: the synthetic-resume dedup gate must fuse the durable frontier
    // EVEN when AGENTDESK_DELIVERY_RECORD_AUTHORITY is OFF — `effective_committed_offset`
    // hides it under the flag, so the gate reads this flag-INDEPENDENT path instead.)

    /// The exact #3593 AUTHORITY-OFF regression codex flagged: a restart / synthetic
    /// resume reset the in-memory `committed_relay_offset` to 0, but the durable
    /// frontier still holds the CURRENT-generation high watermark (443154). The
    /// flag-independent reader surfaces it, so the caller's `0.max(443154)=443154`
    /// fusion makes `range_already_committed(422855, 443154)=true` → the already-
    /// delivered body is suppressed (no #3520 new-message-guard regression).
    #[test]
    fn current_gen_durable_frontier_covers_authority_off_resume_dup_3593() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 3593);
        let gen_ns = 700_000_000_i64;
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 443_154),
                generation_mtime_ns: gen_ns,
                attempts: 1,
                panel_msg_id: Some(1),
                panel_channel_id: None,
            },
        )
        .unwrap();
        // Current generation matches the frontier's → trust the durable END.
        let durable_end = current_generation_durable_frontier_end_at(&path, gen_ns, Some(443_154));
        assert_eq!(durable_end, Some(443_154));
        // In-memory committed reset to 0 (the AUTHORITY-OFF restart hazard); the
        // fused floor rises to the durable value → the synthetic-resume range is
        // recognized as already-delivered.
        let in_memory_committed = 0_u64;
        let fused = in_memory_committed.max(durable_end.unwrap_or(0));
        assert_eq!(fused, 443_154);
        assert!(range_already_committed(422_855, fused));
    }

    /// Over-suppression no-regression: even with the durable frontier fused in, a
    /// genuinely-NEW answer (range_end ABOVE the durable high watermark) is NOT
    /// suppressed — the `max` only raises the floor to the known-delivered offset,
    /// never above a fresh range_end.
    #[test]
    fn current_gen_durable_frontier_does_not_oversuppress_new_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 35930);
        let gen_ns = 700_000_000_i64;
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 443_154),
                generation_mtime_ns: gen_ns,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();
        let fused = 0_u64.max(
            current_generation_durable_frontier_end_at(&path, gen_ns, Some(443_154)).unwrap_or(0),
        );
        // A new answer produced AFTER the durable high watermark.
        assert!(!range_already_committed(443_200, fused));
    }

    /// #1270 generation gating: a STALE prior-generation durable frontier is
    /// distrusted (→ `None` → fused floor stays at in-memory). This is what keeps
    /// the fusion safe after a pane reset/respawn — a stale-high value can NEVER
    /// over-suppress the new generation's fresh output.
    #[test]
    fn current_gen_durable_frontier_distrusts_stale_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 35931);
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 443_154),
                generation_mtime_ns: 100, // written by a PRIOR generation
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();
        // Current generation differs → distrust → None.
        assert_eq!(
            current_generation_durable_frontier_end_at(&path, 999, Some(443_154)),
            None
        );
        // `current_gen == 0` (no .generation file) → also distrust.
        assert_eq!(
            current_generation_durable_frontier_end_at(&path, 0, Some(443_154)),
            None
        );
        // Fused floor stays at in-memory (here 0) → a real range above 0 is NOT
        // suppressed (the new generation's fresh output is relayed).
        let fused = 0_u64.max(
            current_generation_durable_frontier_end_at(&path, 999, Some(443_154)).unwrap_or(0),
        );
        assert!(!range_already_committed(422_855, fused));
    }

    /// #4188: `/compact` can shrink the transcript without changing the wrapper
    /// `.generation` file. Even with a CURRENT generation, a durable END beyond
    /// the current EOF is physically stale and must not raise the committed floor.
    #[test]
    fn current_gen_durable_frontier_distrusts_end_beyond_current_eof_4188() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 4188);
        let gen_ns = 700_000_000_i64;
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 20_536_793),
                generation_mtime_ns: gen_ns,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();

        assert_eq!(
            current_generation_durable_frontier_end_at(&path, gen_ns, Some(16_545_127)),
            None
        );
        assert_eq!(
            current_generation_durable_frontier_end_at(&path, gen_ns, Some(20_536_793)),
            Some(20_536_793)
        );
        assert_eq!(
            current_generation_durable_frontier_end_at(&path, gen_ns, None),
            None
        );
    }

    #[test]
    fn current_generation_frontier_eof_regression_signal_is_exact_4549() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 4549);
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 900),
                generation_mtime_ns: 700,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();

        assert_eq!(
            current_generation_frontier_exceeding_eof_at(&path, 700, 250),
            Some(900)
        );
        assert_eq!(
            current_generation_frontier_exceeding_eof_at(&path, 701, 250),
            None
        );
        assert_eq!(
            current_generation_frontier_exceeding_eof_at(&path, 700, 900),
            None
        );
    }

    #[test]
    fn same_generation_frontier_reanchor_converges_once_4841() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 4841);
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (80, 900),
                generation_mtime_ns: 700,
                attempts: 3,
                panel_msg_id: Some(42),
                panel_channel_id: Some(84),
            },
        )
        .unwrap();

        assert!(reanchor_current_generation_frontier_at(&path, 700, 900, 250).unwrap());
        let rewritten = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(rewritten.range, (80, 250));
        assert_eq!(rewritten.generation_mtime_ns, 700);
        assert_eq!(rewritten.attempts, 3);
        assert_eq!(rewritten.panel_msg_id, Some(42));
        assert_eq!(rewritten.panel_channel_id, Some(84));
        assert_eq!(
            current_generation_frontier_exceeding_eof_at(&path, 700, 250),
            None
        );
        assert!(!reanchor_current_generation_frontier_at(&path, 700, 900, 250).unwrap());
    }

    #[test]
    fn frontier_reanchor_defers_when_concurrent_commit_wins_lock_4841() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 48_411);
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 900),
                generation_mtime_ns: 700,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();
        let observed_frontier =
            current_generation_frontier_exceeding_eof_at(&path, 700, 250).unwrap();

        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (250, 400),
                generation_mtime_ns: 700,
                attempts: 2,
                panel_msg_id: Some(42),
                panel_channel_id: Some(84),
            },
        )
        .unwrap();

        assert!(
            !reanchor_current_generation_frontier_at(&path, 700, observed_frontier, 250).unwrap()
        );
        let winner = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(winner.range, (250, 400));
        assert_eq!(winner.attempts, 2);
        assert_eq!(winner.panel_msg_id, Some(42));
    }

    #[test]
    fn frontier_reanchor_rejects_stale_generation_4841() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 48_412);
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 900),
                generation_mtime_ns: 700,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();

        assert!(!reanchor_current_generation_frontier_at(&path, 701, 900, 250).unwrap());
        assert_eq!(
            read_record_at(&path)
                .unwrap()
                .delivered_frontier
                .unwrap()
                .range,
            (0, 900)
        );
    }

    /// I3 conservatism: an absent or malformed record yields no durable floor
    /// (`None` → fuse contributes 0), so the fusion degrades to pure in-memory —
    /// never an "assume delivered" that would drop a message.
    #[test]
    fn current_gen_durable_frontier_missing_or_malformed_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = delivery_record_path_in_root(dir.path(), &ProviderKind::Codex, 35932);
        assert_eq!(
            current_generation_durable_frontier_end_at(&missing, 5, Some(0)),
            None
        );

        let malformed = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 35933);
        fs::create_dir_all(malformed.parent().unwrap()).unwrap();
        fs::write(&malformed, "{ not json").unwrap();
        assert_eq!(
            current_generation_durable_frontier_end_at(&malformed, 5, Some(0)),
            None
        );
    }

    // ---- #3610 PR-2 (anchor-repost stale-anchor guard) -------------------------
    // `current_generation_delivered_anchor_at` is the structural guard that gates
    // the recovery anchor-repost fallback. It must return the anchor ONLY for a
    // CURRENT-generation, fully-populated frontier — funneling through the same
    // #1270 generation gate so a stale prior-generation anchor (from a same-named
    // tmux respawn / a previous turn) can never drive a repost.

    #[test]
    fn current_gen_anchor_returns_populated_current_generation_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36101);
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (10, 443_154),
                generation_mtime_ns: 700,
                attempts: 1,
                panel_msg_id: Some(555),
                panel_channel_id: Some(777),
            },
        )
        .unwrap();
        // Matching generation + fully-populated anchor → Some.
        assert_eq!(
            current_generation_delivered_anchor_at(&path, 700, Some(443_154)),
            Some(CurrentGenerationAnchor {
                panel_msg_id: 555,
                panel_channel_id: 777,
                range: (10, 443_154),
            })
        );
    }

    #[test]
    fn current_gen_anchor_distrusts_stale_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36102);
        write_delivered_frontier_at(
            &path,
            DeliveredCommit {
                range: (0, 100),
                generation_mtime_ns: 100, // PRIOR generation
                attempts: 1,
                panel_msg_id: Some(555),
                panel_channel_id: Some(777),
            },
        )
        .unwrap();
        // Current generation differs → distrust → None (no repost of a stale turn).
        assert_eq!(
            current_generation_delivered_anchor_at(&path, 999, Some(100)),
            None
        );
        // No `.generation` file (current_gen == 0) → also distrust.
        assert_eq!(
            current_generation_delivered_anchor_at(&path, 0, Some(100)),
            None
        );
    }

    #[test]
    fn current_gen_anchor_rejects_zero_or_missing_anchor_pair() {
        let dir = tempfile::tempdir().unwrap();
        // panel_msg_id == 0 (un-anchored / TUI-direct sentinel) → None.
        let zero_msg = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36103);
        write_delivered_frontier_at(
            &zero_msg,
            DeliveredCommit {
                range: (0, 42),
                generation_mtime_ns: 700,
                attempts: 1,
                panel_msg_id: Some(0),
                panel_channel_id: Some(777),
            },
        )
        .unwrap();
        assert_eq!(
            current_generation_delivered_anchor_at(&zero_msg, 700, Some(42)),
            None
        );

        // panel_channel_id == 0 → None.
        let zero_ch = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36104);
        write_delivered_frontier_at(
            &zero_ch,
            DeliveredCommit {
                range: (0, 42),
                generation_mtime_ns: 700,
                attempts: 1,
                panel_msg_id: Some(555),
                panel_channel_id: Some(0),
            },
        )
        .unwrap();
        assert_eq!(
            current_generation_delivered_anchor_at(&zero_ch, 700, Some(42)),
            None
        );

        // Absent anchor pair (legacy short-replace frontier w/o anchor) → None.
        let no_anchor = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36105);
        write_delivered_frontier_at(
            &no_anchor,
            DeliveredCommit {
                range: (0, 42),
                generation_mtime_ns: 700,
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();
        assert_eq!(
            current_generation_delivered_anchor_at(&no_anchor, 700, Some(42)),
            None
        );
    }

    #[test]
    fn current_gen_anchor_missing_or_malformed_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = delivery_record_path_in_root(dir.path(), &ProviderKind::Codex, 36106);
        assert_eq!(
            current_generation_delivered_anchor_at(&missing, 5, Some(0)),
            None
        );

        let malformed = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36107);
        fs::create_dir_all(malformed.parent().unwrap()).unwrap();
        fs::write(&malformed, "{ not json").unwrap();
        assert_eq!(
            current_generation_delivered_anchor_at(&malformed, 5, Some(0)),
            None
        );
    }

    // ---- #3610 (Phase B PR-1 / PR-1b) terminal-anchor recording ----------------
    // `shadow_mirror_delivered_frontier` forwards the caller's
    // `terminal_anchor_msg_id` + `terminal_anchor_channel_id` verbatim as the
    // `panel_msg_id` / `panel_channel_id` arguments to
    // `record_delivered_frontier_shadow_at` (the path-based testable core), so
    // these exercise that core with the anchor semantics the three call sites use:
    //   - sink/watcher (same-channel) → (Some(channel), Some(current_msg_id))
    //   - bridge cutover (PR-1b, cross-channel) → (Some(edit channel), Some(msg))
    // The status-panel id (`status_message_id`) is no longer read; the recorded
    // anchor is now the terminal `current_msg_id` the replace edits in place, and
    // PR-1b also records WHICH channel that anchor message lives in.

    #[test]
    fn anchor_msg_id_recorded_as_panel_msg_id_3610() {
        // The sink/watcher path: the caller resolves the terminal anchor =
        // `current_msg_id` (the active-slot / PlaceholderEdit-target message) and
        // passes it as the `panel_msg_id` argument, with `panel_channel_id` = the
        // (same) channel. Both land in `DeliveredCommit` verbatim — NOT the
        // status-panel id.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36100);
        // Anchor (current_msg_id) is a DIFFERENT value than any status-panel id
        // would be — pinning that the recorded datum is the terminal anchor.
        let terminal_anchor: u64 = 555_111_222;
        let anchor_channel: u64 = 444_000_111;
        record_delivered_frontier_shadow_at(
            &path,
            (0, 100),
            700,
            0,
            Some(terminal_anchor),
            Some(anchor_channel),
            100,
        )
        .unwrap();
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(written.panel_msg_id, Some(terminal_anchor));
        assert_eq!(written.panel_channel_id, Some(anchor_channel));
        assert_eq!(written.range, (0, 100));
    }

    #[test]
    fn cutover_records_cross_channel_anchor_pair_3610b() {
        // #3610 PR-1b — the REAL prod/incident terminal path (bridge cutover). The
        // frontier is KEYED by the offset-authority channel (`watcher_owner_channel_id`),
        // but the anchor message lives in the (possibly DIFFERENT) edit-target channel.
        // The cutover therefore records the (edit channel, edit msg) PAIR while the
        // frontier key stays the owner channel — so the recorded `panel_msg_id` is NO
        // LONGER null (the bug PR-1 left), and `panel_channel_id` tells PR-2 which
        // channel to edit. This pins that the pair is recorded verbatim and non-null.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36104);
        let edit_msg: u64 = 700_111_222; // current_msg_id (edit target)
        let edit_channel: u64 = 800_333_444; // delivery channel != owner channel
        record_delivered_frontier_shadow_at(
            &path,
            (0, 100),
            700,
            0,
            Some(edit_msg),
            Some(edit_channel),
            100,
        )
        .unwrap();
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(written.panel_msg_id, Some(edit_msg)); // NOT null (PR-1's bug)
        assert_eq!(written.panel_channel_id, Some(edit_channel));
        assert_eq!(written.range, (0, 100));
    }

    #[test]
    fn anchor_none_records_null_panel_pair_3610() {
        // The defensive `None`/`None` path (no resolvable anchor): the durable anchor
        // pair stays null (unchanged from the absent-status-panel case), and the
        // frontier still advances normally.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36101);
        record_delivered_frontier_shadow_at(&path, (0, 100), 700, 0, None, None, 100).unwrap();
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(written.panel_msg_id, None);
        assert_eq!(written.panel_channel_id, None);
        assert_eq!(written.range, (0, 100));
    }

    #[test]
    fn panel_channel_id_backward_compatible_serde_default_3610b() {
        // #3610 PR-1b backward compat: a PR-1 record (or any pre-#3610b record) has
        // NO `panel_channel_id` field. `#[serde(default)]` must deserialize it to
        // `None` without error, leaving the rest of the frontier intact.
        let legacy_json = r#"{
            "delivery_lease": null,
            "delivered_frontier": {
                "range": [0, 443154],
                "generation_mtime_ns": 700000000,
                "attempts": 1,
                "panel_msg_id": 555111222
            }
        }"#;
        let record: DeliveryRecord = serde_json::from_str(legacy_json).unwrap();
        let frontier = record.delivered_frontier.unwrap();
        assert_eq!(frontier.panel_msg_id, Some(555_111_222));
        assert_eq!(frontier.panel_channel_id, None); // serde default — no field present
        assert_eq!(frontier.range, (0, 443_154));
    }

    #[test]
    fn anchor_value_is_dedup_byte_invariant_3610() {
        // ★ #3593 byte-impact 0: the recorded anchor PAIR (`panel_msg_id` +
        // #3610b `panel_channel_id`) is NEVER read by the offset-dedup path — the
        // single durable-frontier reader (`current_generation_durable_frontier_end_at`)
        // reads only `.range.1`. So two records with the SAME frontier END but
        // DIFFERENT anchor pairs ((Some(msg), Some(channel)) vs (None, None), the old
        // status_message_id=null incident case) MUST yield byte-identical dedup
        // decisions.
        let dir = tempfile::tempdir().unwrap();
        let gen_ns = 700_000_000_i64;

        let path_with_anchor =
            delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36102);
        write_delivered_frontier_at(
            &path_with_anchor,
            DeliveredCommit {
                range: (0, 443_154),
                generation_mtime_ns: gen_ns,
                attempts: 1,
                panel_msg_id: Some(999_888_777), // #3610: terminal anchor present
                panel_channel_id: Some(111_222_333), // #3610b: anchor channel present
            },
        )
        .unwrap();

        let path_null_anchor =
            delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36103);
        write_delivered_frontier_at(
            &path_null_anchor,
            DeliveredCommit {
                range: (0, 443_154),
                generation_mtime_ns: gen_ns,
                attempts: 1,
                panel_msg_id: None, // pre-fix incident shape (status_message_id=null)
                panel_channel_id: None,
            },
        )
        .unwrap();

        // Same durable frontier END read out regardless of anchor.
        let end_with =
            current_generation_durable_frontier_end_at(&path_with_anchor, gen_ns, Some(443_154));
        let end_null =
            current_generation_durable_frontier_end_at(&path_null_anchor, gen_ns, Some(443_154));
        assert_eq!(end_with, end_null);
        assert_eq!(end_with, Some(443_154));

        // Same dedup decision regardless of anchor (the #3593 suppress + the
        // over-suppression no-regression both unchanged).
        let fused_with = 0_u64.max(end_with.unwrap_or(0));
        let fused_null = 0_u64.max(end_null.unwrap_or(0));
        assert_eq!(fused_with, fused_null);
        assert_eq!(
            range_already_committed(422_855, fused_with),
            range_already_committed(422_855, fused_null)
        );
        assert!(range_already_committed(422_855, fused_with)); // suppressed
        assert_eq!(
            range_already_committed(443_200, fused_with),
            range_already_committed(443_200, fused_null)
        );
        assert!(!range_already_committed(443_200, fused_with)); // new answer relayed
    }

    #[test]
    fn shadow_off_is_anchor_noop_3610() {
        // Phase A: the old gate now controls divergence telemetry only. It must
        // remain false under OFF, but confirmed delivery authority is written by
        // the flag-independent funnel tested below.
        assert!(!should_shadow_mirror(true, false)); // no divergence telemetry
        assert!(!should_shadow_mirror(false, true)); // not delivered → no anchor write (I2)
    }

    #[test]
    fn shadow_off_confirmed_delivery_writes_exact_split_channel_authority_4911() {
        let root = IsolatedRoot::new();
        let _shadow_off = shadow_test_seam::force(false);
        let provider = ProviderKind::Claude;
        let owner = ChannelId::new(4_911_001);
        let delivery = ChannelId::new(4_911_002);
        let message_id = 4_911_003;
        let tmux = "AgentDesk-claude-phase-a-flag-off";
        let gen_path = crate::services::tmux_common::session_temp_path(tmux, "generation");
        if let Some(parent) = Path::new(&gen_path).parent() {
            fs::create_dir_all(parent).expect("create generation parent");
        }
        fs::write(&gen_path, "phase-a").expect("write generation marker");
        let generation = current_generation_mtime_ns(tmux);
        assert_ne!(generation, 0);

        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(),
            delivery.get(),
            Some("phase-a".into()),
            7,
            49_110,
            message_id,
            "flag independent".into(),
            Some("session-4911".into()),
            Some(tmux.into()),
            Some("/tmp/phase-a-flag-off.jsonl".into()),
            None,
            100,
        );
        state.last_offset = 200;
        state.turn_nonce = Some("flag-off-turn-nonce".into());
        state.set_watcher_owner_channel_id(owner.get());
        crate::services::discord::inflight::save_inflight_state(&state)
            .expect("save delivered turn identity");

        let shared = crate::services::discord::make_shared_data_for_tests();
        let coord = shared.tmux_relay_coord(owner);
        coord.confirmed_end_offset.store(200, Ordering::Release);
        coord
            .confirmed_end_generation_mtime_ns
            .store(generation, Ordering::Release);
        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            owner,
            Some(tmux),
            (100, 200),
            true,
            Some(message_id),
            Some(delivery.get()),
            Some("same body is not authority"),
            Some(49_110),
        );

        let record = read_record(&provider, owner.get()).expect("flag-off record");
        assert_eq!(record.delivered_frontier.unwrap().range, (100, 200));
        assert_eq!(record.confirmed_deliveries.len(), 1);
        let source = &record.confirmed_deliveries[0].source;
        assert_eq!(source.offset_authority_channel_id, owner.get());
        assert_eq!(source.delivery_channel_id, delivery.get());
        assert!(confirmed_delivery_receipt_exists(
            &provider, delivery, message_id, source,
        ));
        assert!(
            delivery_record_path(&provider, owner.get())
                .expect("owner record path")
                .starts_with(root.path())
        );
    }

    #[test]
    fn confirmed_receipts_reverse_order_merge_without_frontier_regression_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = 4_911_101;
        let delivery = 4_911_102;
        let tmux = "AgentDesk-claude-phase-a-r2-reverse";
        let generation = set_phase_a_generation(tmux, 1_700_491_101);

        let (newer_frontier, newer_receipt) = exact_delivery_fixture(
            &provider,
            tmux,
            "newer-turn",
            (100, 300),
            generation,
            (owner, delivery),
            4_911_103,
        );
        write_confirmed_delivery(&provider, owner, newer_frontier, newer_receipt)
            .expect("write newer receipt first");

        let (older_frontier, older_receipt) = exact_delivery_fixture(
            &provider,
            tmux,
            "delayed-older-turn",
            (0, 200),
            generation,
            (owner, delivery),
            4_911_104,
        );
        write_confirmed_delivery(&provider, owner, older_frontier, older_receipt)
            .expect("append delayed lower receipt without regressing frontier");

        let (equal_frontier, equal_receipt) = exact_delivery_fixture(
            &provider,
            tmux,
            "delayed-equal-end-turn",
            (50, 300),
            generation,
            (owner, delivery),
            4_911_105,
        );
        write_confirmed_delivery(&provider, owner, equal_frontier, equal_receipt)
            .expect("append delayed equal-end receipt without replacing frontier metadata");

        let record = read_record(&provider, owner).expect("merged record");
        let frontier = record.delivered_frontier.expect("monotonic frontier");
        assert_eq!(frontier.range, (100, 300));
        assert_eq!(frontier.panel_msg_id, Some(4_911_103));
        assert_eq!(record.confirmed_deliveries.len(), 3);
        assert_eq!(
            record
                .confirmed_deliveries
                .iter()
                .map(|receipt| receipt.message_id)
                .collect::<Vec<_>>(),
            vec![4_911_103, 4_911_104, 4_911_105]
        );
    }

    #[test]
    fn receiptless_confirmed_frontiers_reverse_order_keep_winner_identity_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = ChannelId::new(4_911_401);
        let tmux = "AgentDesk-claude-phase-a-r3-receiptless-reverse";
        let generation = set_phase_a_generation(tmux, 1_700_491_401);
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(),
            owner.get(),
            Some("phase-a-r3".into()),
            7,
            49_114,
            4_911_402,
            "receipt unavailable".into(),
            Some("session-r3".into()),
            Some(tmux.into()),
            Some("/tmp/phase-a-r3-receiptless.jsonl".into()),
            None,
            100,
        );
        state.last_offset = 300;
        state.turn_nonce = None;
        crate::services::discord::inflight::save_inflight_state(&state)
            .expect("persist receipt-less delivery incarnation");
        let shared = crate::services::discord::make_shared_data_for_tests();
        let coord = shared.tmux_relay_coord(owner);
        coord.confirmed_end_offset.store(300, Ordering::Release);
        coord
            .confirmed_end_generation_mtime_ns
            .store(generation, Ordering::Release);
        let newer = DeliveredCommit {
            range: (100, 300),
            generation_mtime_ns: generation,
            attempts: 0,
            panel_msg_id: Some(4_911_402),
            panel_channel_id: Some(owner.get()),
        };
        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            owner,
            Some(tmux),
            newer.range,
            true,
            newer.panel_msg_id,
            newer.panel_channel_id,
            None,
            Some(state.user_msg_id),
        );

        for (range, message_id) in [((0, 200), 4_911_404), ((50, 300), 4_911_406)] {
            shadow_mirror_delivered_frontier(
                &shared,
                &provider,
                owner,
                Some(tmux),
                range,
                true,
                Some(message_id),
                Some(owner.get()),
                None,
                Some(state.user_msg_id),
            );
        }

        let record = read_record(&provider, owner.get()).expect("receipt-less record");
        assert_eq!(record.delivered_frontier, Some(newer));
        assert!(record.confirmed_deliveries.is_empty());
    }

    #[test]
    fn cleared_inflight_current_generation_persists_frontier_without_receipt_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = ChannelId::new(4_911_601);
        let delivery = ChannelId::new(4_911_602);
        let tmux = "AgentDesk-claude-phase-a-r4-cleared-current";
        let generation = set_phase_a_generation(tmux, 1_700_491_601);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let coord = shared.tmux_relay_coord(owner);
        coord.confirmed_end_offset.store(240, Ordering::Release);
        coord
            .confirmed_end_generation_mtime_ns
            .store(generation, Ordering::Release);

        // No inflight row exists: the caller-held incarnation is sufficient for
        // the frontier, but cannot manufacture exact turn/nonce receipt proof.
        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            owner,
            Some(tmux),
            (120, 240),
            true,
            Some(4_911_603),
            Some(delivery.get()),
            Some("confirmed after inflight clear"),
            Some(49_116),
        );

        let record = read_record(&provider, owner.get()).expect("receipt-less frontier record");
        assert_eq!(
            record.delivered_frontier,
            Some(DeliveredCommit {
                range: (120, 240),
                generation_mtime_ns: generation,
                attempts: 0,
                panel_msg_id: Some(4_911_603),
                panel_channel_id: Some(delivery.get()),
            })
        );
        assert!(
            record.confirmed_deliveries.is_empty(),
            "caller-held session identity must not synthesize an exact receipt"
        );
    }

    #[test]
    fn cleared_inflight_stale_caller_generation_rejects_frontier_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Codex;
        let owner = ChannelId::new(4_911_611);
        let tmux = "AgentDesk-codex-phase-a-r4-cleared-stale";
        let stale_generation = set_phase_a_generation(tmux, 1_700_491_611);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let coord = shared.tmux_relay_coord(owner);
        coord.confirmed_end_offset.store(200, Ordering::Release);
        coord
            .confirmed_end_generation_mtime_ns
            .store(stale_generation, Ordering::Release);
        let current_generation = set_phase_a_generation(tmux, 1_700_491_612);
        assert_ne!(stale_generation, current_generation);

        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            owner,
            Some(tmux),
            (0, 200),
            true,
            Some(4_911_612),
            Some(owner.get()),
            None,
            Some(49_117),
        );

        assert!(
            read_record(&provider, owner.get()).is_none(),
            "post-lock generation validation must reject stale caller authority"
        );
    }

    #[test]
    fn legacy_frontier_reverse_order_split_channel_keeps_winner_whole_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = 4_911_621;
        let tmux = "AgentDesk-claude-phase-a-r4-legacy-reverse";
        let generation = set_phase_a_generation(tmux, 1_700_491_621);
        let winner = DeliveredCommit {
            range: (100, 300),
            generation_mtime_ns: generation,
            attempts: 9,
            panel_msg_id: Some(4_911_622),
            panel_channel_id: Some(4_911_623),
        };
        write_delivered_frontier(&provider, owner, tmux, winner.clone())
            .expect("write winning split-channel frontier");

        for frontier in [
            DeliveredCommit {
                range: (0, 200),
                generation_mtime_ns: generation,
                attempts: 2,
                panel_msg_id: Some(4_911_624),
                panel_channel_id: Some(4_911_625),
            },
            DeliveredCommit {
                range: (50, 300),
                generation_mtime_ns: generation,
                attempts: 3,
                panel_msg_id: Some(4_911_626),
                panel_channel_id: Some(4_911_627),
            },
        ] {
            write_delivered_frontier(&provider, owner, tmux, frontier)
                .expect("merge delayed legacy frontier");
        }

        assert_eq!(
            read_record(&provider, owner)
                .expect("legacy record")
                .delivered_frontier,
            Some(winner),
            "lower/equal legacy writes must not union START or replace split-channel anchor"
        );
    }

    #[test]
    fn ordered_jsonl_reverse_order_preserves_split_channel_anchor_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Codex;
        let owner = ChannelId::new(4_911_631);
        let tmux = "AgentDesk-codex-phase-a-r4-ordered-reverse";
        let generation = set_phase_a_generation(tmux, 1_700_491_631);
        let winner = DeliveredCommit {
            range: (100, 300),
            generation_mtime_ns: generation,
            attempts: 7,
            panel_msg_id: Some(4_911_632),
            panel_channel_id: Some(4_911_633),
        };
        write_delivered_frontier(&provider, owner.get(), tmux, winner.clone())
            .expect("seed split-channel winner");

        for range in [(0, 200), (50, 300)] {
            assert!(
                commit_ordered_jsonl_range(&provider, owner, tmux, range, generation)
                    .expect("merge delayed ordered range")
            );
        }

        assert_eq!(
            read_record(&provider, owner.get())
                .expect("ordered record")
                .delivered_frontier,
            Some(winner),
            "ordered commits must not union ranges or clear an existing anchor"
        );
    }

    #[test]
    fn legacy_frontier_blocked_writer_revalidates_after_rotation_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = 4_911_641;
        let tmux = "AgentDesk-claude-phase-a-r4-legacy-race";
        let stale_generation = set_phase_a_generation(tmux, 1_700_491_641);
        let path = record_path_or_err(&provider, owner).expect("record path");
        let held_lock = lock_record_path(&path).expect("hold record lock");
        let stale_path = path.clone();
        let stale_tmux = tmux.to_string();
        let (about_to_lock_tx, about_to_lock_rx) = std::sync::mpsc::channel();
        let stale_writer = std::thread::spawn(move || {
            write_delivered_frontier_guarded_at_with_before_lock(
                &stale_path,
                &stale_tmux,
                DeliveredCommit {
                    range: (0, 900),
                    generation_mtime_ns: stale_generation,
                    attempts: 5,
                    panel_msg_id: Some(4_911_642),
                    panel_channel_id: Some(4_911_643),
                },
                || about_to_lock_tx.send(()).expect("signal lock attempt"),
            )
        });
        about_to_lock_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("legacy writer reached record lock");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!stale_writer.is_finished(), "legacy writer must be blocked");

        let current_generation = set_phase_a_generation(tmux, 1_700_491_642);
        let current_record = DeliveryRecord {
            delivered_frontier: Some(DeliveredCommit {
                range: (100, 200),
                generation_mtime_ns: current_generation,
                attempts: 1,
                panel_msg_id: Some(4_911_644),
                panel_channel_id: Some(4_911_645),
            }),
            ..DeliveryRecord::default()
        };
        write_record_at(&path, &current_record).expect("install rotated record");
        drop(held_lock);

        assert!(stale_writer.join().expect("join legacy writer").is_err());
        assert_eq!(read_record_at(&path), Some(current_record));
    }

    #[test]
    fn proven_gone_reanchor_blocked_writer_preserves_concurrent_anchor_swap_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = 4_911_661;
        let tmux = "AgentDesk-claude-phase-a-r5-reanchor-race";
        let generation = set_phase_a_generation(tmux, 1_700_491_661);
        let path = record_path_or_err(&provider, owner).expect("record path");
        let range = (100, 300);
        let gone_anchor = (4_911_662, 4_911_663);
        write_delivered_frontier(
            &provider,
            owner,
            tmux,
            DeliveredCommit {
                range,
                generation_mtime_ns: generation,
                attempts: 1,
                panel_channel_id: Some(gone_anchor.0),
                panel_msg_id: Some(gone_anchor.1),
            },
        )
        .expect("seed anchor later proved gone");

        let held_lock = lock_record_path(&path).expect("hold record lock");
        let stale_path = path.clone();
        let stale_tmux = tmux.to_string();
        let (about_to_lock_tx, about_to_lock_rx) = std::sync::mpsc::channel();
        let stale_writer = std::thread::spawn(move || {
            write_proven_gone_equal_range_frontier_at_with_before_lock(
                &stale_path,
                &stale_tmux,
                gone_anchor,
                DeliveredCommit {
                    range,
                    generation_mtime_ns: generation,
                    attempts: 2,
                    panel_channel_id: Some(4_911_664),
                    panel_msg_id: Some(4_911_665),
                },
                || about_to_lock_tx.send(()).expect("signal lock attempt"),
            )
        });
        about_to_lock_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("re-anchor writer reached record lock");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!stale_writer.is_finished(), "re-anchor writer must block");

        let swapped_frontier = DeliveredCommit {
            range,
            generation_mtime_ns: generation,
            attempts: 3,
            panel_channel_id: Some(4_911_666),
            panel_msg_id: Some(4_911_667),
        };
        let swapped_record = DeliveryRecord {
            delivered_frontier: Some(swapped_frontier),
            ..DeliveryRecord::default()
        };
        write_record_at(&path, &swapped_record).expect("install concurrent anchor swap");
        drop(held_lock);

        stale_writer
            .join()
            .expect("join re-anchor writer")
            .expect("anchor mismatch is a conservative no-op");
        assert_eq!(
            read_record_at(&path),
            Some(swapped_record),
            "the under-lock anchor CAS must preserve the concurrent whole commit"
        );
    }

    #[test]
    fn proven_gone_reanchor_missing_frontier_is_conservative_noop_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = 4_911_681;
        let tmux = "AgentDesk-claude-phase-a-r6-reanchor-missing";
        let generation = set_phase_a_generation(tmux, 1_700_491_681);
        let path = record_path_or_err(&provider, owner).expect("record path");
        let record_without_frontier = DeliveryRecord::default();
        write_record_at(&path, &record_without_frontier).expect("seed record without frontier");

        write_proven_gone_equal_range_frontier(
            &provider,
            owner,
            tmux,
            (4_911_682, 4_911_683),
            DeliveredCommit {
                range: (100, 300),
                generation_mtime_ns: generation,
                attempts: 1,
                panel_channel_id: Some(4_911_684),
                panel_msg_id: Some(4_911_685),
            },
        )
        .expect("missing frontier remains a diagnosed conservative no-op");
        assert_eq!(read_record_at(&path), Some(record_without_frontier));
    }

    #[test]
    fn ordered_jsonl_blocked_writer_revalidates_after_rotation_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Codex;
        let owner = 4_911_651;
        let tmux = "AgentDesk-codex-phase-a-r4-ordered-race";
        let stale_generation = set_phase_a_generation(tmux, 1_700_491_651);
        let path = record_path_or_err(&provider, owner).expect("record path");
        let held_lock = lock_record_path(&path).expect("hold record lock");
        let stale_path = path.clone();
        let stale_tmux = tmux.to_string();
        let (about_to_lock_tx, about_to_lock_rx) = std::sync::mpsc::channel();
        let stale_writer = std::thread::spawn(move || {
            commit_ordered_jsonl_range_at_with_before_lock(
                &stale_path,
                &stale_tmux,
                (0, 900),
                stale_generation,
                || about_to_lock_tx.send(()).expect("signal lock attempt"),
            )
        });
        about_to_lock_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("ordered writer reached record lock");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !stale_writer.is_finished(),
            "ordered writer must be blocked"
        );

        let current_generation = set_phase_a_generation(tmux, 1_700_491_652);
        let current_record = DeliveryRecord {
            delivered_frontier: Some(DeliveredCommit {
                range: (100, 200),
                generation_mtime_ns: current_generation,
                attempts: 1,
                panel_msg_id: Some(4_911_652),
                panel_channel_id: Some(4_911_653),
            }),
            ..DeliveryRecord::default()
        };
        write_record_at(&path, &current_record).expect("install rotated record");
        drop(held_lock);

        assert!(stale_writer.join().expect("join ordered writer").is_err());
        assert_eq!(read_record_at(&path), Some(current_record));
    }

    #[test]
    fn receiptless_blocked_writer_revalidates_after_rotation_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Codex;
        let owner = 4_911_501;
        let delivery = 4_911_502;
        let tmux = "AgentDesk-codex-phase-a-r3-receiptless-race";
        let old_generation = set_phase_a_generation(tmux, 1_700_491_501);
        let path = record_path_or_err(&provider, owner).expect("record path");
        let held_lock = lock_record_path(&path).expect("hold record lock");
        let stale_frontier = DeliveredCommit {
            range: (0, 900),
            generation_mtime_ns: old_generation,
            attempts: 7,
            panel_msg_id: Some(4_911_503),
            panel_channel_id: Some(delivery),
        };
        let stale_path = path.clone();
        let stale_tmux = tmux.to_string();
        let (about_to_lock_tx, about_to_lock_rx) = std::sync::mpsc::channel();
        let stale_writer = std::thread::spawn(move || {
            write_confirmed_frontier_guarded_at_with_before_lock(
                &stale_path,
                &stale_tmux,
                stale_frontier,
                None,
                EqualRangeAnchorPolicy::PreserveExisting,
                || about_to_lock_tx.send(()).expect("signal lock attempt"),
            )
        });
        about_to_lock_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("stale writer reached the record lock");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !stale_writer.is_finished(),
            "stale writer must be lock-blocked"
        );

        let current_generation = set_phase_a_generation(tmux, 1_700_491_502);
        let (current_frontier, current_receipt) = exact_delivery_fixture(
            &provider,
            tmux,
            "current-after-rotation",
            (100, 200),
            current_generation,
            (owner, delivery),
            4_911_504,
        );
        let current_record = DeliveryRecord {
            delivered_frontier: Some(current_frontier),
            confirmed_deliveries: vec![current_receipt],
            ..DeliveryRecord::default()
        };
        write_record_at(&path, &current_record).expect("install current incarnation under lock");
        drop(held_lock);

        assert!(
            stale_writer.join().expect("join stale writer").is_err(),
            "stale receipt-less writer must reject after acquiring the lock"
        );
        assert_eq!(read_record_at(&path), Some(current_record));
    }

    #[test]
    fn rotate_after_receipt_validation_preserves_current_incarnation_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Codex;
        let owner = 4_911_201;
        let delivery = 4_911_202;
        let tmux = "AgentDesk-codex-phase-a-r2-rotate";
        let old_generation = set_phase_a_generation(tmux, 1_700_491_201);
        let (delayed_frontier, delayed_receipt) = exact_delivery_fixture(
            &provider,
            tmux,
            "validated-before-rotate",
            (0, 400),
            old_generation,
            (owner, delivery),
            4_911_203,
        );
        assert!(delayed_receipt.is_authoritative());

        let current_generation = set_phase_a_generation(tmux, 1_700_491_202);
        assert_ne!(old_generation, current_generation);
        let (current_frontier, current_receipt) = exact_delivery_fixture(
            &provider,
            tmux,
            "current-incarnation",
            (0, 100),
            current_generation,
            (owner, delivery),
            4_911_204,
        );
        write_confirmed_delivery(&provider, owner, current_frontier, current_receipt)
            .expect("write current incarnation");
        let before = read_record(&provider, owner).expect("current record");

        assert!(
            write_confirmed_delivery(&provider, owner, delayed_frontier, delayed_receipt).is_err(),
            "the guarded writer must recheck generation after receipt construction"
        );
        assert_eq!(read_record(&provider, owner), Some(before));
    }

    #[test]
    fn confirmed_receipt_write_rejects_destination_owner_and_provider_mismatch_4911() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = 4_911_301;
        let delivery = 4_911_302;
        let tmux = "AgentDesk-claude-phase-a-r2-mismatch";
        let generation = set_phase_a_generation(tmux, 1_700_491_301);
        let (frontier, receipt) = exact_delivery_fixture(
            &provider,
            tmux,
            "mismatch-turn",
            (0, 100),
            generation,
            (owner, delivery),
            4_911_303,
        );

        let mut wrong_destination = receipt.clone();
        wrong_destination.delivery_channel_id = delivery + 1;
        assert!(
            write_confirmed_delivery(&provider, owner, frontier.clone(), wrong_destination,)
                .is_err()
        );
        let mut missing_anchor_channel = frontier.clone();
        missing_anchor_channel.panel_channel_id = None;
        assert!(
            write_confirmed_delivery(&provider, owner, missing_anchor_channel, receipt.clone(),)
                .is_err()
        );
        assert!(
            write_confirmed_delivery(&provider, owner + 1, frontier.clone(), receipt.clone())
                .is_err()
        );
        assert!(write_confirmed_delivery(&ProviderKind::Codex, owner, frontier, receipt).is_err());
        assert!(read_record(&provider, owner).is_none());
    }

    // ---- #3610 PR-1c: long-chunk terminal arm anchor recording -----------------
    // `record_long_chunk_terminal_delivery` is the BRIDGE long-chunk arm's anchor
    // helper (turn_bridge/mod.rs site 4). It is a thin pass-through to
    // `shadow_mirror_delivered_frontier` with `is_delivered = true` hardcoded
    // (the caller invokes it ONLY on the full-commit `Ok` arm of
    // `send_ordered_long_terminal_response`, which is all-or-nothing — a partial
    // chunk failure rolls back and returns `Err`, so an `Ok` means every chunk
    // committed). These pin the recorded shape via the path-based core the helper
    // funnels into (same approach as the PR-1/PR-1b tests above), since the full
    // helper's flag (OnceLock) + runtime-root resolution are env-global.

    #[test]
    fn long_chunk_full_commit_records_last_chunk_anchor_3610c() {
        // The long-chunk arm deletes the placeholder, so the LAST sent chunk's
        // message id is the only stable terminal anchor. On full commit the helper
        // records (panel_msg_id = last_chunk, panel_channel_id = delivery channel)
        // — proving panel_msg_id is NON-null on the long-message path PR-1/1b left
        // uncovered. The frontier END is the lease range's `.1`.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36110);
        let last_chunk_anchor: u64 = 912_345_678; // tail chunk msg id (not the first)
        let delivery_channel: u64 = 444_555_666;
        // Mirrors the args record_long_chunk_terminal_delivery forwards: range =
        // lease (start,end), panel_msg_id = last chunk, panel_channel_id = delivery.
        record_delivered_frontier_shadow_at(
            &path,
            (0, 4096),
            700,
            0,
            Some(last_chunk_anchor),
            Some(delivery_channel),
            4096,
        )
        .unwrap();
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(written.panel_msg_id, Some(last_chunk_anchor)); // NON-null
        assert_eq!(written.panel_channel_id, Some(delivery_channel));
        assert_eq!(written.range, (0, 4096));
    }

    #[test]
    fn long_chunk_cross_channel_separates_owner_and_delivery_3610c() {
        // Gate (C): the frontier is KEYED by `watcher_owner_channel_id` (offset
        // authority — unchanged), while the recorded anchor PAIR points at the
        // delivery `channel_id` (where the tail chunk lives). For a reused-watcher
        // bridge these DIFFER; the helper must NOT swap them. The frontier key is the
        // record PATH's channel (owner); the recorded `panel_channel_id` is delivery.
        let dir = tempfile::tempdir().unwrap();
        let owner_channel: u64 = 100_200_300; // watcher_owner_channel_id = frontier key
        let delivery_channel: u64 = 900_800_700; // edit/delivery channel = anchor home
        assert_ne!(owner_channel, delivery_channel);
        // The record lives under the OWNER channel (the frontier key) ...
        let owner_path =
            delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, owner_channel);
        // ... and its recorded anchor channel is the DELIVERY channel.
        record_delivered_frontier_shadow_at(
            &owner_path,
            (10, 8192),
            700,
            0,
            Some(777_111_222),
            Some(delivery_channel),
            8192,
        )
        .unwrap();
        let written = read_record_at(&owner_path)
            .unwrap()
            .delivered_frontier
            .unwrap();
        assert_eq!(written.panel_channel_id, Some(delivery_channel)); // delivery, not owner
        assert_eq!(written.range, (10, 8192));
        // No record was written under the DELIVERY channel (the frontier key is owner).
        let delivery_path =
            delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, delivery_channel);
        assert_eq!(read_record_at(&delivery_path), None);
    }

    #[test]
    fn long_chunk_anchor_none_records_range_only_3610c() {
        // Gate (D): an empty chunk Vec (impossible on the full-commit `Ok` path, but
        // type-honest) → last = None → the helper records the range with a null
        // anchor, identical to the absent-status-panel case. The frontier still
        // advances (END = range.1) so the dedup floor is correct.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 36111);
        record_delivered_frontier_shadow_at(&path, (0, 2048), 700, 0, None, Some(444), 2048)
            .unwrap();
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(written.panel_msg_id, None);
        assert_eq!(written.range, (0, 2048));
    }

    #[test]
    fn long_chunk_partial_or_err_never_records_anchor_3610c() {
        // Gate (A): the helper hardcodes `is_delivered = true` because the mod.rs
        // call site lives ONLY inside the full-commit `Ok` arm — the long-chunk send
        // (`send_long_message_with_rollback`) rolls back and returns `Err` on ANY
        // chunk failure, so a partial delivery NEVER reaches the helper. The shadow
        // outcome (modelled here as `is_delivered = false`) can never reach either
        // persistence or divergence telemetry.
        assert!(!should_shadow_mirror(false, true));
    }

    #[test]
    fn record_long_chunk_terminal_delivery_off_is_noop_3610c() {
        // Legacy test name retained: the synthetic coord has no generation, so
        // Phase A must fail closed without overwriting a prior authoritative
        // frontier. The shadow flag itself controls only telemetry.
        // The scoped root holds the crate-wide test-env lock, so both the helper and
        // the assertions stay inside one throw-away tree and can never inspect the
        // operator's runtime.
        let root = IsolatedRoot::new();
        let _shadow_off = shadow_test_seam::force(false);
        let shared = crate::services::discord::make_shared_data_for_tests();
        // Does not panic; missing generation → writes nothing.
        super::record_long_chunk_terminal_delivery(
            &shared,
            &ProviderKind::Claude,
            ChannelId::new(100_200_300), // owner (frontier key)
            ChannelId::new(900_800_700), // delivery (anchor home)
            None,
            (0, 4096),
            Some(912_345_678),
            "",
            None,
        );
        // No durable record was created for either channel under the test root.
        for channel_id in [100_200_300, 900_800_700] {
            let path = delivery_record_path(&ProviderKind::Claude, channel_id)
                .expect("isolated delivery-record path");
            assert!(
                path.starts_with(root.path()),
                "delivery-record path escaped isolated root: {}",
                path.display()
            );
            assert!(read_record(&ProviderKind::Claude, channel_id).is_none());
        }
    }

    #[test]
    fn ledger_append_keys_by_delivery_channel_not_watcher_owner_4564() {
        // #4564 channel-split silent-loss regression (counter-model finding). A
        // recovered/reused-watcher bridge delivers channel Y's answered turn while
        // reusing channel X's tmux session, so the funnel is invoked with
        // `channel = watcher_owner_channel_id = X` even though the delivered turn
        // belongs to Y. X still holds a PRESERVED, UNANSWERED inflight row
        // (user_msg_id = B). The completed-turn ledger MUST key by the delivery
        // channel Y and record Y's EXPLICIT user_msg_id (A) — it must NOT reload X's
        // row and false-Settle B (which would make catch-up skip B's recovery on the
        // next restart = silent loss, the #4600 class re-introduced by a new vector).
        let _root = IsolatedRoot::new();
        let _shadow_off = shadow_test_seam::force(false);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let watcher_owner_x = ChannelId::new(4_564_100); // offset-authority / session owner
        let delivery_y = ChannelId::new(4_564_200); // where the answered user msg lives
        let unanswered_b: u64 = 88_000_001; // X's preserved, un-delivered turn
        let answered_a: u64 = 99_000_002; // Y's delivered turn

        // X carries a preserved UNANSWERED inflight row for B.
        let mut x_state = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(),
            watcher_owner_x.get(),
            Some("adk-test".to_string()),
            343_742_347_365_974_026,
            unanswered_b,
            0,
            "미답변 메시지".to_string(),
            Some("session-x".to_string()),
            Some("AgentDesk-codex-adk-test".to_string()),
            Some("/tmp/4564-split.jsonl".to_string()),
            None,
            128,
        );
        x_state.turn_start_offset = Some(0);
        crate::services::discord::inflight::save_inflight_state(&x_state).expect("save X inflight");
        let tmux_session_name = x_state.tmux_session_name.as_deref().expect("tmux session");
        let generation_mtime_ns = set_phase_a_generation(tmux_session_name, 1_700_456_400);
        let coord = shared.tmux_relay_coord(watcher_owner_x);
        coord
            .confirmed_end_generation_mtime_ns
            .store(generation_mtime_ns, Ordering::Release);
        coord.confirmed_end_offset.store(128, Ordering::Release);

        // Y's answered turn commits terminally while reusing X's session.
        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            watcher_owner_x, // funnel `channel` = offset-authority X
            Some(tmux_session_name),
            (0, 128),
            true, // is_delivered
            Some(77_001),
            Some(delivery_y.get()), // terminal_anchor_channel_id = delivery channel Y
            Some("answer"),
            Some(answered_a), // explicit inbound turn id from Y's snapshot
        );

        // (b) Y's answered turn is recorded under Y.
        assert!(
            completed_turn_ledger::settled_user_msg_ids(&provider, delivery_y.get())
                .contains(&answered_a),
            "the answered turn's user_msg_id must be recorded under the delivery channel Y"
        );
        // (a) X's UNANSWERED turn must NOT be false-Settled.
        let x_settled =
            completed_turn_ledger::settled_user_msg_ids(&provider, watcher_owner_x.get());
        assert!(
            !x_settled.contains(&unanswered_b),
            "MUTATION: reverting to a commit-time reload of `channel`=X would append X's \
             unanswered B here — silently losing B on restart (#4600 class)"
        );
        assert!(
            x_settled.is_empty(),
            "no ledger entry may be written under the offset-authority channel X"
        );
    }

    /// #5154 regression. A confirmed terminal delivery whose JSONL incarnation is
    /// UNKNOWN (`generation_mtime_ns == 0`) must still leave settled evidence that
    /// the user message was answered, while the generation-scoped durable frontier
    /// stays unwritten.
    ///
    /// Production shape being reproduced: the turn-bridge funnel
    /// (`shadow_mirror_delivered_frontier`) passes `watcher_authority = None`, so the
    /// incarnation is read from `coord.confirmed_end_generation_mtime_ns`. A stale
    /// tmux-watermark reset leaves that atomic at its initial `0` and the only
    /// re-establishment site refuses to write a `0`, so confirmed deliveries kept
    /// arriving with an unknown incarnation. The old single early return dropped the
    /// completed-turn ledger append along with the frontier, and
    /// `catch_up/settled_ledger_consult.rs` treats that ledger as the ONLY
    /// settled-evidence source — so catch-up re-collected already-answered messages
    /// and re-submitted their prompts verbatim.
    ///
    /// The `range`/`user_msg_id`/`channel_id` values below are the ones observed in
    /// the reported incident (issue #5154 WARN line at 20:53:32.847Z, channel
    /// 1479671298497183835, `range=(0, 42675) generation_mtime_ns=0`).
    #[test]
    fn unknown_generation_confirmed_delivery_settles_ledger_without_frontier_5154() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = ChannelId::new(1_479_671_298_497_183_835);
        let delivery = ChannelId::new(1_479_671_298_497_183_835);
        let tmux = "AgentDesk-claude-5154-unknown-incarnation";
        let pinned_user_msg_id = 1_534_665_637_186_765_043_u64;

        let shared = crate::services::discord::make_shared_data_for_tests();
        let coord = shared.tmux_relay_coord(owner);
        // The stale-watermark reset left the incarnation atomic at its initial `0`
        // and no writer re-established it. Deliberately NOT stored: `0` IS the
        // initial value, and the test must exercise exactly that state.
        assert_eq!(
            coord
                .confirmed_end_generation_mtime_ns
                .load(Ordering::Acquire),
            0,
            "precondition: the unknown-incarnation state under test"
        );
        coord.confirmed_end_offset.store(42_675, Ordering::Release);

        // Confirmed delivery: `is_delivered == true`, range strictly growing
        // (42675 > 0), incarnation unknown. Only the third condition is degenerate.
        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            owner,
            Some(tmux),
            (0, 42_675),
            true,
            Some(1_534_665_637_186_765_044),
            Some(delivery.get()),
            Some("confirmed terminal answer"),
            Some(pinned_user_msg_id),
        );

        let record = read_record(&provider, owner.get());
        // The frontier guard is UNCHANGED and just as strict: a frontier without an
        // authoritative JSONL incarnation is meaningless and must never be durable.
        assert!(
            record
                .as_ref()
                .and_then(|record| record.delivered_frontier.as_ref())
                .is_none(),
            "#5154: an unknown JSONL incarnation must never produce a durable frontier"
        );
        // The #4081 content fingerprint is generation-scoped by its match predicate
        // and is deliberately NOT recorded under the unknown-incarnation sentinel.
        assert!(
            record
                .as_ref()
                .map(|record| record.recent_delivered_contents.is_empty())
                .unwrap_or(true),
            "#5154: no fingerprint may be stored under the unknown-incarnation sentinel"
        );

        // MUTATION TARGET: "this user message was answered" is not a fact about a
        // JSONL incarnation. Collapsing the settlement append back into the frontier
        // guard's early return fails HERE, on this assertion.
        assert!(
            completed_turn_ledger::settled_user_msg_ids(&provider, delivery.get())
                .contains(&pinned_user_msg_id),
            "#5154 MUTATION: the answered user message must remain settled evidence \
             even when the JSONL incarnation is unknown — otherwise catch-up \
             re-collects it and re-submits the same turn_id verbatim"
        );
    }

    /// #5154 companion: the settlement carve-out must NOT weaken the ledger-id
    /// safety rule. With no caller-pinned inbound id there is nothing safe to
    /// settle — the receipt-qualified fresh-row fallback cannot run without an
    /// authoritative incarnation, and guessing from an unqualified current row
    /// would let a delayed A settle a newer B. Nothing is written at all.
    #[test]
    fn unknown_generation_without_pinned_inbound_id_settles_nothing_5154() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let owner = ChannelId::new(5_154_201);
        let delivery = ChannelId::new(5_154_202);
        let tmux = "AgentDesk-claude-5154-unpinned";

        let shared = crate::services::discord::make_shared_data_for_tests();

        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            owner,
            Some(tmux),
            (0, 42_675),
            true,
            Some(5_154_203),
            Some(delivery.get()),
            Some("confirmed terminal answer"),
            None, // caller pinned nothing
        );

        assert!(
            completed_turn_ledger::settled_user_msg_ids(&provider, delivery.get()).is_empty(),
            "#5154: an unpinned delivery must never invent a settled user_msg_id"
        );
        assert!(
            read_record(&provider, owner.get())
                .and_then(|record| record.delivered_frontier)
                .is_none(),
            "#5154: the frontier guard still holds"
        );
    }

    /// #5154: `is_delivered == false` is still an absolute bar. The settlement
    /// carve-out sits BELOW the `!is_delivered` return, so a non-delivered outcome
    /// records nothing even with a pinned inbound id.
    #[test]
    fn not_delivered_never_settles_the_ledger_5154() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(5_154_301);
        let pinned_user_msg_id = 5_154_300_u64;

        let shared = crate::services::discord::make_shared_data_for_tests();

        shadow_mirror_delivered_frontier(
            &shared,
            &provider,
            channel,
            Some("AgentDesk-claude-5154-not-delivered"),
            (0, 42_675),
            false, // NOT delivered
            Some(5_154_302),
            Some(channel.get()),
            Some("undelivered body"),
            Some(pinned_user_msg_id),
        );

        assert!(
            completed_turn_ledger::settled_user_msg_ids(&provider, channel.get()).is_empty(),
            "#5154: a non-delivered outcome must never produce settled evidence"
        );
    }

    // ---- #3610 PR-1d: WATCHER legacy long-chunk arm (same-channel) --------------
    // The watcher long-chunk fallback arm (tmux_watcher.rs — the
    // `watcher_should_send_ordered_new_chunks_for_terminal_fallback` branch) is the
    // watcher-owned counterpart of the bridge arm above. Its sibling helper
    // `terminal_long_chunks::record_watcher_terminal_delivery` is SAME-CHANNEL:
    // it preserves `watcher_owner_channel_id == delivery_channel_id == channel_id`.
    // So the frontier key (record path)
    // and the recorded `panel_channel_id` are the SAME channel — UNLIKE the bridge's
    // cross-channel `long_chunk_cross_channel_separates_owner_and_delivery_3610c`.
    // Pinned here via the path-core (the helper's flag is env-global OnceLock).

    #[test]
    fn watcher_long_chunk_same_channel_anchor_pair_3610d() {
        // Gate (C) for the watcher arm: same-channel ⇒ panel_channel_id equals the
        // frontier-key channel (the record path's channel), and panel_msg_id is the
        // NON-null last-chunk anchor. Mirrors the args the watcher helper forwards:
        // range = (watcher_lease_start, watcher_lease_end), both channels = channel_id.
        let dir = tempfile::tempdir().unwrap();
        let watcher_channel: u64 = 778_899_001; // owner + delivery (same for watcher)
        let last_chunk_anchor: u64 = 654_321_987; // tail chunk msg id
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, watcher_channel);
        // The watcher wrapper passes channel_id as BOTH the path channel (frontier
        // key) and panel_channel_id; the path-core models that with the same value.
        record_delivered_frontier_shadow_at(
            &path,
            (10, 16384),
            700,
            0,
            Some(last_chunk_anchor),
            Some(watcher_channel), // == the record path channel (same-channel)
            16384,
        )
        .unwrap();
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(written.panel_msg_id, Some(last_chunk_anchor)); // NON-null
        // The recorded anchor channel IS the frontier-key channel (same-channel).
        assert_eq!(written.panel_channel_id, Some(watcher_channel));
        assert_eq!(written.range, (10, 16384));
    }

    #[test]
    fn watcher_long_chunk_partial_or_unadvanced_never_records_3610d() {
        // Gates (A)+(M4) for the watcher arm: the helper is invoked ONLY inside the
        // `if committed && Delivered { advance(); … }` block in tmux_watcher.rs, AND
        // only when the anchor is `Some` (the full-commit `Ok` arm of
        // `send_long_message_raw_with_rollback`, which is all-or-nothing — a partial
        // chunk failure rolls back and returns `Err`). So a non-advanced commit
        // (modelled as `is_delivered = false`) NEVER reaches the durable write.
        assert!(!should_shadow_mirror(false, true));
    }

    // ---- #3933 item 1: read-authority (authority-ON) end-to-end wiring --------
    // The pure helpers above (fuse / #1270 generation gate / range_already_committed)
    // are covered, but no test drove the ENV-RESOLVED public gates
    // (`effective_committed_offset` / `committed_floor_for_resend_dedup`) with the
    // flag FORCED ON — the release config (AGENTDESK_DELIVERY_RECORD_AUTHORITY=1)
    // the compiled default (OFF) never exercises. These tests force it ON via the
    // #3993 per-thread seam and verify the whole wiring end-to-end (not by
    // hand-computing the fusion): the dedup floor is `max(durable, in_memory)` so it
    // never over-suppresses, the #1270 gate distrusts a stale generation, and the
    // #3871 / #3885 duplicate-relay scenarios are correctly suppressed. This closes
    // #3933 prerequisite #2 (independent read-authority verification).

    /// RAII: point `AGENTDESK_ROOT_DIR` at an isolated tempdir for a test
    /// (restoring the prior value on drop) while holding the process-global env
    /// lock. BOTH the delivery-record path and the tmux `.generation` marker path
    /// resolve through this root, so the whole read-authority wiring runs against a
    /// throw-away tree with zero cross-test interference.
    struct IsolatedRoot {
        _env: crate::config::TestEnvVarGuard,
        _dir: tempfile::TempDir,
    }

    impl IsolatedRoot {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("isolated runtime root");
            let env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", dir.path());
            Self {
                _env: env,
                _dir: dir,
            }
        }

        fn path(&self) -> &Path {
            self._dir.path()
        }
    }

    /// Seed a CURRENT-generation durable frontier for `(provider, channel)` under
    /// the isolated root plus its matching `.generation` marker, and return the
    /// generation mtime the record was stamped with. The record's
    /// `generation_mtime_ns` is set to the marker's REAL on-disk mtime so the #1270
    /// generation gate TRUSTS it — mirroring the production write/read parity.
    fn seed_current_generation_frontier(
        provider: &ProviderKind,
        channel: ChannelId,
        tmux_session_name: &str,
        durable_end: u64,
    ) -> i64 {
        let gen_path =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
        if let Some(parent) = Path::new(&gen_path).parent() {
            fs::create_dir_all(parent).expect("create sessions dir");
        }
        fs::write(&gen_path, b"1").expect("write generation marker");
        let gen_ns = current_generation_mtime_ns(tmux_session_name);
        assert_ne!(
            gen_ns, 0,
            "seeded .generation marker must have a readable mtime"
        );
        let record_path =
            delivery_record_path(provider, channel.get()).expect("env-resolved record path");
        write_delivered_frontier_at(
            &record_path,
            DeliveredCommit {
                range: (0, durable_end),
                generation_mtime_ns: gen_ns,
                attempts: 1,
                panel_msg_id: Some(1),
                panel_channel_id: None,
            },
        )
        .expect("seed durable frontier");
        gen_ns
    }

    fn seed_edit_failure_transcript(
        root: &IsolatedRoot,
        tmux_session_name: &str,
        generation_mtime_ns: i64,
        eof: u64,
    ) -> EditFailureTranscriptIdentity {
        let output_path = root.path().join(format!("{tmux_session_name}.jsonl"));
        fs::write(&output_path, vec![b'x'; eof as usize]).expect("seed transcript");
        EditFailureTranscriptIdentity {
            output_path,
            generation_mtime_ns,
        }
    }

    fn stable_edit_failure_committed_at(
        provider: &ProviderKind,
        channel: ChannelId,
        expected: &EditFailureTranscriptIdentity,
        range_end: u64,
    ) -> bool {
        let record_path =
            delivery_record_path(provider, channel.get()).expect("delivery record path");
        let frontier = stable_edit_failure_frontier_at(
            &record_path,
            expected,
            || Some(expected.output_path.clone()),
            || expected.generation_mtime_ns,
            || {},
        );
        range_already_committed(
            range_end,
            frontier.map(|frontier| frontier.range.1).unwrap_or(0),
        )
    }

    fn shared_with_committed(
        channel: ChannelId,
        in_memory: u64,
    ) -> std::sync::Arc<crate::services::discord::SharedData> {
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared
            .tmux_relay_coord(channel)
            .confirmed_end_offset
            .store(in_memory, Ordering::Release);
        shared
    }

    /// authority-ON fuses the CURRENT-generation durable frontier into the dedup
    /// floor (`max(durable, in_memory)`) through the ENV-RESOLVED public reader,
    /// while authority-OFF returns the in-memory value verbatim (deploy no-op).
    /// This proves the flag actually gates the wiring — not just the pure `fuse`
    /// arithmetic already covered above.
    #[test]
    fn effective_committed_offset_authority_on_fuses_durable_3933() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(39_330_401);
        let tmux = "AgentDesk-claude-3933fuse";
        let durable_end = 443_154_u64;
        let in_memory = 100_u64;
        seed_current_generation_frontier(&provider, channel, tmux, durable_end);
        let shared = shared_with_committed(channel, in_memory);

        {
            // authority ON → durable frontier RAISES the floor above in-memory.
            let _authority = authority_test_seam::force(true);
            assert_eq!(
                effective_committed_offset(
                    shared.as_ref(),
                    &provider,
                    channel,
                    tmux,
                    Some(u64::MAX)
                ),
                durable_end.max(in_memory),
            );
            // `committed_floor_for_resend_dedup` = effective.max(flag-independent
            // durable) → the same fused value (floor never drops below in-memory).
            assert_eq!(
                committed_floor_for_resend_dedup(
                    shared.as_ref(),
                    &provider,
                    channel,
                    tmux,
                    Some(u64::MAX)
                ),
                durable_end.max(in_memory),
            );
        }
        {
            // authority OFF (forced) → in-memory verbatim; the durable frontier is
            // hidden by the flag. Pins the flag-gated branch.
            let _authority = authority_test_seam::force(false);
            assert_eq!(
                effective_committed_offset(
                    shared.as_ref(),
                    &provider,
                    channel,
                    tmux,
                    Some(u64::MAX)
                ),
                in_memory,
            );
        }
    }

    /// #3871 (rollover dup relay): after a JSONL rollover a re-observing pass would
    /// re-relay the frozen PREFIX range (ends BELOW the durable committed floor →
    /// already delivered → suppressed), while a genuinely-NEW tail produced after
    /// the rollover ends ABOVE the floor → NOT suppressed → relayed. The floor is
    /// `max(durable, in_memory)`, so raising it to the known-delivered watermark
    /// never over-suppresses fresh output. Rides the in-memory=0 restart hazard.
    #[test]
    fn committed_floor_authority_on_does_not_oversuppress_rollover_resend_3871() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(39_330_402);
        let tmux = "AgentDesk-claude-3871";
        let durable_end = 443_154_u64;
        seed_current_generation_frontier(&provider, channel, tmux, durable_end);
        // In-memory reset to 0 (the restart / synthetic-resume hazard #3871 rides).
        let shared = shared_with_committed(channel, 0);

        let _authority = authority_test_seam::force(true);
        let floor = committed_floor_for_resend_dedup(
            shared.as_ref(),
            &provider,
            channel,
            tmux,
            Some(u64::MAX),
        );
        assert_eq!(
            floor, durable_end,
            "durable frontier must lift the reset in-memory floor"
        );
        // Frozen prefix already delivered → suppressed (no dup relay).
        assert!(range_already_committed(422_855, floor));
        // New tail past the durable high watermark → relayed (no over-suppression).
        assert!(!range_already_committed(443_500, floor));
    }

    /// #3885 (no-response watchdog re-relay): the streaming-aware watchdog can
    /// trigger a re-observing pass over an ALREADY-delivered range. Under
    /// authority-ON the durable frontier makes the committed floor recognize that
    /// range as delivered (`range_already_committed == true`) → the watchdog
    /// re-relay is suppressed (no duplicate) even though the in-memory offset was
    /// reset. The boundary (range_end == floor) is inclusive.
    #[test]
    fn committed_floor_authority_on_suppresses_watchdog_rerelay_3885() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(39_330_403);
        let tmux = "AgentDesk-codex-3885";
        let durable_end = 512_000_u64;
        seed_current_generation_frontier(&provider, channel, tmux, durable_end);
        let shared = shared_with_committed(channel, 0); // watchdog fires post in-memory reset

        let _authority = authority_test_seam::force(true);
        let floor = committed_floor_for_resend_dedup(
            shared.as_ref(),
            &provider,
            channel,
            tmux,
            Some(u64::MAX),
        );
        assert_eq!(floor, durable_end);
        // Watchdog re-relay of the delivered body → suppressed (dup guard).
        assert!(range_already_committed(500_000, floor));
        assert!(range_already_committed(durable_end, floor)); // inclusive boundary
    }

    #[test]
    fn edit_failure_recheck_suppresses_only_stable_fresh_bounded_commit_4508() {
        let root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(45_080_001);
        let tmux = "AgentDesk-claude-4508";
        let committed_end = 900_u64;
        let generation = seed_current_generation_frontier(&provider, channel, tmux, committed_end);
        let expected = seed_edit_failure_transcript(&root, tmux, generation, committed_end);

        assert!(stable_edit_failure_committed_at(
            &provider,
            channel,
            &expected,
            committed_end,
        ));
        assert!(!stable_edit_failure_committed_at(
            &provider,
            channel,
            &expected,
            committed_end + 1,
        ));

        fs::write(
            &expected.output_path,
            vec![b'x'; (committed_end - 1) as usize],
        )
        .expect("truncate transcript");
        assert!(!stable_edit_failure_committed_at(
            &provider,
            channel,
            &expected,
            committed_end,
        ));
    }

    #[test]
    fn edit_failure_recheck_distrusts_prior_generation_4508() {
        let root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(45_080_002);
        let tmux = "AgentDesk-claude-4508-stale";
        let committed_end = 900_u64;
        let generation = seed_current_generation_frontier(&provider, channel, tmux, committed_end);
        let expected = seed_edit_failure_transcript(&root, tmux, generation, committed_end);
        let record_path = delivery_record_path(&provider, channel.get()).unwrap();
        write_delivered_frontier_at(
            &record_path,
            DeliveredCommit {
                range: (0, committed_end),
                generation_mtime_ns: generation.saturating_add(1),
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();

        assert!(!stable_edit_failure_committed_at(
            &provider,
            channel,
            &expected,
            committed_end,
        ));
    }

    #[test]
    fn edit_failure_recheck_requires_durable_proof_despite_high_memory_floor_4508() {
        let root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(45_080_003);
        let tmux = "AgentDesk-claude-4508-unverifiable";
        let committed_end = 900_u64;
        let shared = shared_with_committed(channel, committed_end);

        assert!(!range_committed_after_edit_failure(
            shared.as_ref(),
            &provider,
            channel,
            tmux,
            None,
            committed_end,
        ));

        let generation = seed_current_generation_frontier(&provider, channel, tmux, committed_end);
        let missing_output = EditFailureTranscriptIdentity {
            output_path: root.path().join("missing.jsonl"),
            generation_mtime_ns: generation,
        };
        assert!(!stable_edit_failure_committed_at(
            &provider,
            channel,
            &missing_output,
            committed_end,
        ));
    }

    #[test]
    fn edit_failure_recheck_detects_rotate_between_eof_and_frontier_4508() {
        let root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(45_080_004);
        let tmux = "AgentDesk-claude-4508-rotate";
        let committed_end = 900_u64;
        let old_generation =
            seed_current_generation_frontier(&provider, channel, tmux, committed_end);
        let expected = seed_edit_failure_transcript(&root, tmux, old_generation, committed_end);
        let new_path = root.path().join("replacement.jsonl");
        fs::write(&new_path, vec![b'n'; committed_end as usize]).expect("seed replacement");
        let record_path = delivery_record_path(&provider, channel.get()).unwrap();
        let current_path = std::cell::RefCell::new(expected.output_path.clone());
        let current_generation = std::cell::Cell::new(old_generation);

        let frontier = stable_edit_failure_frontier_at(
            &record_path,
            &expected,
            || Some(current_path.borrow().clone()),
            || current_generation.get(),
            || {
                current_path.replace(new_path.clone());
                current_generation.set(old_generation.saturating_add(1));
                write_record_at(
                    &record_path,
                    &DeliveryRecord {
                        delivery_lease: None,
                        delivered_frontier: Some(DeliveredCommit {
                            range: (0, committed_end),
                            generation_mtime_ns: old_generation.saturating_add(1),
                            attempts: 1,
                            panel_msg_id: None,
                            panel_channel_id: None,
                        }),
                        recent_delivered_contents: Vec::new(),
                        confirmed_deliveries: Vec::new(),
                    },
                )
                .expect("seed unrelated new frontier");
            },
        );
        assert!(
            frontier.is_none(),
            "wrapper rotation after EOF sampling must fail open for fallback delivery"
        );
    }

    /// Same-name transcript truncate/replace WITHOUT a generation-marker bump:
    /// the durable frontier still carries the expected generation and stays
    /// within the pre-truncate EOF, so ONLY the post-read file-identity check
    /// can detect the race. Mutation-sensitive for the second snapshot
    /// comparison in `stable_edit_failure_frontier_at`.
    #[test]
    fn edit_failure_recheck_detects_same_generation_truncate_during_snapshot_4508() {
        let root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(45_080_005);
        let tmux = "AgentDesk-claude-4508-truncate-race";
        let committed_end = 900_u64;
        let generation = seed_current_generation_frontier(&provider, channel, tmux, committed_end);
        let expected = seed_edit_failure_transcript(&root, tmux, generation, committed_end);
        let record_path = delivery_record_path(&provider, channel.get()).unwrap();

        let frontier = stable_edit_failure_frontier_at(
            &record_path,
            &expected,
            || Some(expected.output_path.clone()),
            || expected.generation_mtime_ns,
            || {
                fs::write(
                    &expected.output_path,
                    vec![b't'; (committed_end - 1) as usize],
                )
                .expect("truncate transcript during snapshot");
            },
        );
        assert!(
            frontier.is_none(),
            "same-generation transcript truncation during the snapshot must fail open"
        );
    }

    /// Even with authority ON, a STALE prior-generation durable frontier is
    /// distrusted (#1270 gate) → it does NOT raise the floor → a genuinely-new
    /// answer after a pane reset / same-named respawn is never over-suppressed.
    /// Proves the generation gate is honored through the ENV-RESOLVED wiring, not
    /// only in the pure helper.
    #[test]
    fn effective_committed_offset_authority_on_distrusts_stale_generation_3933() {
        let _root = IsolatedRoot::new();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(39_330_404);
        let tmux = "AgentDesk-claude-3933stale";
        // Seed marker + a current-gen frontier, then OVERWRITE the record with a
        // PRIOR-generation stamp (mtime 1 ≠ the marker's real nanosecond mtime).
        seed_current_generation_frontier(&provider, channel, tmux, 443_154);
        let record_path = delivery_record_path(&provider, channel.get()).unwrap();
        write_delivered_frontier_at(
            &record_path,
            DeliveredCommit {
                range: (0, 443_154),
                generation_mtime_ns: 1, // PRIOR generation → distrusted
                attempts: 1,
                panel_msg_id: None,
                panel_channel_id: None,
            },
        )
        .unwrap();
        let shared = shared_with_committed(channel, 0);

        let _authority = authority_test_seam::force(true);
        // Stale frontier distrusted → floor stays at the in-memory value (0).
        assert_eq!(
            effective_committed_offset(shared.as_ref(), &provider, channel, tmux, Some(u64::MAX)),
            0,
        );
        assert_eq!(
            committed_floor_for_resend_dedup(
                shared.as_ref(),
                &provider,
                channel,
                tmux,
                Some(u64::MAX)
            ),
            0,
        );
        // A fresh answer above 0 is NOT over-suppressed.
        assert!(!range_already_committed(422_855, 0));
    }
}
