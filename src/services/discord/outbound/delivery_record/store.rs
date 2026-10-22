//! Delivery record shapes, paths, locking, and generation-guarded writers.

use super::*;

/// Sidecar subtree name — a sibling of `discord_inflight/` under `runtime/`.
/// MUST NOT be `discord_inflight` (that dir is the old-binary reaper's scan
/// target); the isolation proof test asserts this segment differs.
pub(super) const DELIVERY_RECORDS_DIR: &str = "discord_delivery_records";
pub(super) const FRESH_SEND_RECORDS_DIR: &str = "discord_fresh_send_records";
pub(super) const DELIVERY_OWNER_CONTEXT_DIR: &str = "discord_delivery_owner_context";
pub(super) const RECENT_DELIVERED_CONTENT_LIMIT: usize = 16;
pub(super) const RECENT_DELIVERED_CONTENT_WINDOW_MS: u64 = 15 * 60 * 1000;
const CONFIRMED_DELIVERY_RECEIPT_LIMIT: usize = 32;

/// Durable per-turn delivery record (design §4.3). Two **independent** durable
/// fields, deliberately not folded into one state machine: the lease is the
/// transient in-flight claim (cleared on release), the frontier is the
/// release-surviving delivered offset (only a `Delivered` outcome writes it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct DeliveryRecord {
    /// Live/in-flight claim. CLEARED on release (`clear_lease_at`); a leftover
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
    pub(super) fn is_authoritative(&self) -> bool {
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

pub(in crate::services::discord) fn delivery_record_path(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<PathBuf> {
    delivery_records_root().map(|root| {
        root.join(provider.as_str())
            .join(format!("{channel_id}.json"))
    })
}

pub(super) fn fresh_send_record_path(provider: &ProviderKind, channel_id: u64) -> Option<PathBuf> {
    fresh_send_records_root().map(|root| {
        root.join(provider.as_str())
            .join(format!("{channel_id}.json"))
    })
}

#[cfg(test)]
pub(super) fn delivery_owner_context_path_in_root(
    runtime_root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
) -> PathBuf {
    runtime_root
        .join(DELIVERY_OWNER_CONTEXT_DIR)
        .join(provider.as_str())
        .join(format!("{channel_id}.json"))
}

pub(super) fn delivery_owner_context_path(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<PathBuf> {
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

pub(super) fn write_record_at(path: &Path, record: &DeliveryRecord) -> Result<(), String> {
    let data = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
    // Durable temp+rename+fsync helper (I4), also flushing the directories — do not hand-roll.
    runtime_store::durable_publish::publish_store_row(path, &data)
}

/// flock-guarded read-modify-write. Creates the record (default: both fields
/// `None`) if absent.
pub(super) fn mutate_record_at(
    path: &Path,
    mutate: impl FnOnce(&mut DeliveryRecord),
) -> Result<(), String> {
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

pub(super) fn record_path_or_err(
    provider: &ProviderKind,
    channel_id: u64,
) -> Result<PathBuf, String> {
    delivery_record_path(provider, channel_id)
        .ok_or_else(|| "delivery_record: runtime root unavailable".to_string())
}

pub(super) fn owner_context_path_or_err(
    provider: &ProviderKind,
    channel_id: u64,
) -> Result<PathBuf, String> {
    delivery_owner_context_path(provider, channel_id)
        .ok_or_else(|| "delivery_owner_context: runtime root unavailable".to_string())
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

pub(super) fn append_confirmed_receipt(
    record: &mut DeliveryRecord,
    receipt: ConfirmedDeliveryReceipt,
) {
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
pub(super) enum EqualRangeAnchorPolicy {
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
pub(super) struct WatcherFrontierLockAuthority<'a> {
    pub(super) shared: &'a crate::services::discord::SharedData,
    pub(super) channel: ChannelId,
    pub(super) lease_reset_incarnation: u64,
    pub(super) generation_mtime_ns: i64,
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

pub(super) fn write_confirmed_frontier_guarded_at_with_before_lock(
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

pub(super) fn write_confirmed_frontier_guarded_at_with_lock_authority(
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
                tracing::warn!(target: "agentdesk::services::discord::outbound::delivery_record",
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

pub(super) fn write_delivered_frontier_guarded_at_with_before_lock(
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
#[cfg(all(test, unix))]
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

#[cfg(all(test, unix))]
pub(super) fn write_proven_gone_equal_range_frontier_at_with_before_lock(
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

pub(super) fn write_confirmed_delivery_at(
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

pub(super) fn write_confirmed_delivery_at_with_lock_authority(
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

/// Acquire/refresh the transient lease. Preserves any existing
/// `delivered_frontier` (never advances it — I2). Test-only: the B1 lease
/// cutover (#3089 B0) never landed, so nothing in production writes the field.
#[cfg(test)]
pub(super) fn upsert_lease_at(path: &Path, lease: DurableLease) -> Result<(), String> {
    mutate_record_at(path, |record| record.delivery_lease = Some(lease))
}

/// Release: clear the lease ONLY. `delivered_frontier` survives (design §4.3).
/// Test-only, like `upsert_lease_at`.
#[cfg(test)]
pub(super) fn clear_lease_at(path: &Path) -> Result<(), String> {
    mutate_record_at(path, |record| record.delivery_lease = None)
}
