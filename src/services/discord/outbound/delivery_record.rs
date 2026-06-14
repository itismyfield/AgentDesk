//! #3089 Phase B0 — durable delivery-record sidecar store (mixed-binary safe).
//!
//! Phase B gives turn-output delivery a **durable** authority that survives a
//! dcserver restart (AC6). Before any of that authority can be persisted, B0
//! resolves design §4.4's hard blocker: **mixed-binary safety**.
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
//! This store therefore lives in a **dedicated sibling subtree**
//! `runtime/discord_delivery_records/<provider>/<channel_id>.json` — a peer of
//! `runtime/discord_inflight/`, matching the existing sidecar-root convention in
//! `runtime_store.rs` (`discord_status_panel_orphans`,
//! `discord_queued_placeholders`, …). Because the reaper's `read_dir` is
//! non-recursive and only ever opens `discord_inflight/<provider>/`, it never
//! descends into `discord_delivery_records/`. The directory — not the `.json`
//! extension — is the isolator (`sidecar_path_is_outside_inflight_scan_set`
//! pins this). No other component (standby heartbeat rewrites only
//! `{channel_id}.json` via `inflight::save_inflight_state`; the sweeper /
//! recovery read via the inflight loaders) knows this path exists.
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

use super::turn_output_controller::DeliveryOutcome;
use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

/// Sidecar subtree name — a sibling of `discord_inflight/` under `runtime/`.
/// MUST NOT be `discord_inflight` (that dir is the old-binary reaper's scan
/// target); the isolation proof test asserts this segment differs.
const DELIVERY_RECORDS_DIR: &str = "discord_delivery_records";

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
    pub panel_msg_id: Option<u64>,
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// `runtime/discord_delivery_records/` — the sidecar root, a sibling of
/// `discord_inflight/`. `None` when the runtime root is unavailable.
fn delivery_records_root() -> Option<PathBuf> {
    runtime_store::runtime_root().map(|root| root.join(DELIVERY_RECORDS_DIR))
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

fn delivery_record_path(provider: &ProviderKind, channel_id: u64) -> Option<PathBuf> {
    delivery_records_root().map(|root| {
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

struct DeliveryRecordLock {
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

fn lock_record_path(record_path: &Path) -> Result<DeliveryRecordLock, String> {
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
fn read_record_at(path: &Path) -> Option<DeliveryRecord> {
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

/// Advance the delivered frontier. The ONLY writer of `delivered_frontier`
/// (I2 — only a `Delivered` outcome calls this). Preserves the current lease.
fn write_delivered_frontier_at(path: &Path, frontier: DeliveredCommit) -> Result<(), String> {
    mutate_record_at(path, |record| record.delivered_frontier = Some(frontier))
}

#[allow(dead_code)] // #3089 B1 uses the `_at` core directly; the provider/channel form is wired in B2.
pub(in crate::services::discord) fn write_delivered_frontier(
    provider: &ProviderKind,
    channel_id: u64,
    frontier: DeliveredCommit,
) -> Result<(), String> {
    write_delivered_frontier_at(&record_path_or_err(provider, channel_id)?, frontier)
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
// #3089 B1 — shadow-write the delivered frontier (observe-only, default OFF).
//
// B1 mirrors the in-memory `confirmed_end_offset` authority into the durable
// sidecar AFTER a confirmed `Delivered` commit, and asserts the durable END
// tracks it (design §5 / M4 — END is the risky datum). Read-authority STAYS the
// legacy markers; this is a parallel "shadow" so B2 can later flip to it with
// confidence. OFF (default) → zero extraction, zero write → behavioral no-op.
// ---------------------------------------------------------------------------

/// #3089 B1 shadow-write flag (`AGENTDESK_DELIVERY_RECORD_SHADOW`, OnceLock,
/// default OFF). Telemetry ONLY when enabled (the default-OFF first eval has no
/// observable side effect — deploy no-op), mirroring the A-phase flag idiom.
pub(in crate::services::discord) fn delivery_record_shadow_enabled() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let on = std::env::var("AGENTDESK_DELIVERY_RECORD_SHADOW")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .is_some_and(|v| v == "1" || v == "true");
        if on {
            tracing::info!("  ✓ delivery_record_shadow: enabled");
        }
        on
    })
}

/// I2 outcome map (pure, testable): the shadow-write fires ONLY for a confirmed
/// `Delivered`. Every other outcome means the controller did NOT advance the
/// offset — `NotDelivered` (identity gate refused), `Transient`/`Unknown`
/// (ambiguous), `Skipped` (no-op) — so the durable frontier must NOT advance
/// (I2, the #3143/#3416 class). This pins the owner call-site's outcome decision
/// (a frozen-file `matches!` was previously untested — broadening it to include
/// `NotDelivered` slipped through; the variant test now catches it).
pub(in crate::services::discord) fn outcome_is_shadow_delivered(outcome: &DeliveryOutcome) -> bool {
    matches!(outcome, DeliveryOutcome::Delivered { .. })
}

/// I2 gate (pure, testable): shadow-mirror ONLY for a confirmed `Delivered`
/// outcome AND only when the flag is enabled. Dropping the `is_delivered`
/// conjunct would let an ambiguous outcome advance the durable frontier — the
/// exact #3143/#3416 class — so the test pins this conjunction.
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
fn record_delivered_frontier_shadow_at(
    path: &Path,
    range: (u64, u64),
    generation_mtime_ns: i64,
    attempts: u32,
    panel_msg_id: Option<u64>,
    in_memory_confirmed_end: u64,
) -> Result<bool, String> {
    write_delivered_frontier_at(
        path,
        DeliveredCommit {
            range,
            generation_mtime_ns,
            attempts,
            panel_msg_id,
        },
    )?;
    Ok(delivered_frontier_end_diverged(
        range.1,
        in_memory_confirmed_end,
    ))
}

/// provider/channel core: resolve the sidecar path, shadow-write, and emit the
/// observe-only signals. NEVER panics, NEVER changes delivery (the relay had
/// incidents; B1 only observes). Caller invokes this ONLY for `Delivered` (I2).
fn record_delivered_frontier_shadow(
    provider: &ProviderKind,
    channel_id: u64,
    range: (u64, u64),
    generation_mtime_ns: i64,
    attempts: u32,
    panel_msg_id: Option<u64>,
    in_memory_confirmed_end: u64,
) {
    let path = match record_path_or_err(provider, channel_id) {
        Ok(path) => path,
        Err(error) => {
            tracing::error!(
                provider = provider.as_str(),
                channel = channel_id,
                error = %error,
                "#3089 B1: shadow delivery-record path unavailable (observe-only)"
            );
            return;
        }
    };
    match record_delivered_frontier_shadow_at(
        &path,
        range,
        generation_mtime_ns,
        attempts,
        panel_msg_id,
        in_memory_confirmed_end,
    ) {
        Ok(false) => {}
        Ok(true) => tracing::error!(
            provider = provider.as_str(),
            channel = channel_id,
            durable_end = range.1,
            in_memory_confirmed_end,
            generation_mtime_ns,
            "#3089 B1: shadow delivered_frontier END diverged from in-memory confirmed_end_offset (observe-only)"
        ),
        Err(error) => tracing::error!(
            provider = provider.as_str(),
            channel = channel_id,
            error = %error,
            "#3089 B1: shadow delivery-record write failed (observe-only)"
        ),
    }
}

/// Integration wrapper for owner `Delivered` arms. Gated by [`should_shadow_mirror`]
/// (flag ON AND `is_delivered`, I2). When it fires it extracts the in-memory
/// authority (`confirmed_end_offset` + `confirmed_end_generation_mtime_ns`) from
/// the relay coord and the `panel_msg_id`/`attempts` mirror from the fresh
/// inflight, then shadow-writes. OFF or non-`Delivered` → returns immediately
/// (no coord/inflight access, no write) → behavioral no-op.
pub(in crate::services::discord) fn shadow_mirror_delivered_frontier(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    range: (u64, u64),
    is_delivered: bool,
) {
    if !should_shadow_mirror(is_delivered, delivery_record_shadow_enabled()) {
        return;
    }
    let channel_id = channel.get();
    let coord = shared.tmux_relay_coord(channel);
    let in_memory_confirmed_end = coord.confirmed_end_offset.load(Ordering::Acquire);
    let generation_mtime_ns = coord
        .confirmed_end_generation_mtime_ns
        .load(Ordering::Acquire);
    let fresh = crate::services::discord::inflight::load_inflight_state(provider, channel_id);
    let attempts = fresh
        .as_ref()
        .map(|f| f.recovery_relay_attempts)
        .unwrap_or(0);
    let panel_msg_id = fresh.as_ref().and_then(|f| f.status_message_id);
    record_delivered_frontier_shadow(
        provider,
        channel_id,
        range,
        generation_mtime_ns,
        attempts,
        panel_msg_id,
        in_memory_confirmed_end,
    );
}

#[cfg(test)]
mod tests {
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
        }
    }

    #[test]
    fn delivery_record_serde_roundtrip() {
        let record = DeliveryRecord {
            delivery_lease: Some(sample_lease()),
            delivered_frontier: Some(sample_frontier()),
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
        };
        write_record_at(&path, &record).unwrap();
        assert_eq!(read_record_at(&path), Some(record));
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
        assert_ne!(DELIVERY_RECORDS_DIR, "discord_inflight");
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
        }));
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
        // I2: a non-Delivered outcome must NEVER advance the durable frontier,
        // and OFF is a no-op. Pins the AND of both conjuncts.
        assert!(should_shadow_mirror(true, true));
        assert!(!should_shadow_mirror(false, true)); // not delivered → no write (I2)
        assert!(!should_shadow_mirror(true, false)); // flag OFF → no write
        assert!(!should_shadow_mirror(false, false));
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
            record_delivered_frontier_shadow_at(&path, (3, 10), 111, 2, Some(9), 10).unwrap();
        assert!(!diverged);
        let written = read_record_at(&path).unwrap().delivered_frontier.unwrap();
        assert_eq!(
            written,
            DeliveredCommit {
                range: (3, 10),
                generation_mtime_ns: 111,
                attempts: 2,
                panel_msg_id: Some(9),
            }
        );
    }

    #[test]
    fn shadow_reports_divergence_but_still_writes_and_never_panics() {
        // A durable-vs-memory END mismatch is observe-only: it reports `true`
        // (logged upstream) and STILL writes the frontier — no panic.
        let dir = tempfile::tempdir().unwrap();
        let path = delivery_record_path_in_root(dir.path(), &ProviderKind::Claude, 12);
        let diverged =
            record_delivered_frontier_shadow_at(&path, (3, 10), 222, 0, None, 9).unwrap();
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
        record_delivered_frontier_shadow_at(&path, (0, 5), 5, 0, None, 5).unwrap();
        let after = read_record_at(&path).unwrap();
        assert_eq!(after.delivery_lease, Some(sample_lease()));
        assert_eq!(after.delivered_frontier.unwrap().range, (0, 5));
    }
}
