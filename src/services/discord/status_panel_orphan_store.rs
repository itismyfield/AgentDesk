//! #3003: durable retry store for orphaned status-panel-v2 message deletes.
//!
//! The watcher reclaims a TUI-direct status panel inline when its turn ends, and
//! the placeholder sweeper reclaims panels left on lingering inflight rows. Both
//! fast paths can fail transiently (Discord 5xx / rate-limit / transport), and
//! when the owning inflight row has *already been cleared* — e.g. a
//! stopped/cancelled turn — there is no per-turn handle left to retry from, so
//! the panel would stay stuck at "계속 처리 중".
//!
//! This store records `(channel_id, panel_msg_id)` durably, independent of the
//! inflight lifecycle, so [`drain`] can retry the delete across sweeps and
//! restarts until it commits or the message is permanently gone (404/403/410).
//!
//! Layout (atomic temp+rename writes, mirroring `queued_placeholders_store`):
//!
//! ```text
//! runtime/discord_status_panel_orphans/<provider>/<token_hash>/<channel_id>.json
//! ```
//!
//! Each file holds panel entries scoped to that channel; legacy raw id arrays
//! still load as stranded entries. `token_hash` scoping keeps one bot's sweeper
//! from trying to delete another bot's messages.
//!
//! Path resolution is split into `*_in_root` helpers so tests inject an explicit
//! temp root instead of mutating the global `AGENTDESK_ROOT_DIR` env var.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};

use crate::services::discord::inflight::{InflightTurnIdentity, InflightTurnState};
use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

/// Serializes the read-modify-write of `enqueue`/`remove` across the watcher
/// tasks and the sweeper task that all touch this store concurrently (codex P2
/// r14). The critical section is purely synchronous file IO (no await), and
/// these operations only run on the rare delete-failure / drain paths, so a
/// single process-wide lock has negligible contention. Per-file `atomic_write`
/// keeps individual writes crash-safe; this lock keeps two concurrent
/// read-modify-write cycles from clobbering each other.
static STORE_WRITE_LOCK: Mutex<()> = Mutex::new(());

const PENDING_BIND_GRACE_DRAIN_CYCLES: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum StatusPanelOrphanKind {
    #[default]
    Stranded,
    PendingBind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StatusPanelOrphanEntry {
    id: u64,
    #[serde(default)]
    kind: StatusPanelOrphanKind,
    #[serde(default)]
    turn_identity: Option<InflightTurnIdentity>,
    #[serde(default)]
    pending_bind_drain_cycles: u8,
}

impl StatusPanelOrphanEntry {
    fn stranded(id: u64) -> Self {
        Self {
            id,
            kind: StatusPanelOrphanKind::Stranded,
            turn_identity: None,
            pending_bind_drain_cycles: 0,
        }
    }

    fn pending_bind(id: u64, turn_identity: Option<InflightTurnIdentity>) -> Self {
        Self {
            id,
            kind: StatusPanelOrphanKind::PendingBind,
            turn_identity,
            pending_bind_drain_cycles: 0,
        }
    }

    fn is_pending_bind(&self) -> bool {
        self.kind == StatusPanelOrphanKind::PendingBind
    }

    fn reclassify_to_stranded(&mut self) {
        self.kind = StatusPanelOrphanKind::Stranded;
        self.turn_identity = None;
        self.pending_bind_drain_cycles = 0;
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StatusPanelOrphanChannelFile {
    Entries(Vec<StatusPanelOrphanEntry>),
    LegacyIds(Vec<u64>),
}

impl StatusPanelOrphanChannelFile {
    fn into_entries(self) -> Vec<StatusPanelOrphanEntry> {
        match self {
            Self::Entries(entries) => entries,
            Self::LegacyIds(ids) => ids
                .into_iter()
                .map(StatusPanelOrphanEntry::stranded)
                .collect(),
        }
    }
}

fn identity_matches_state(identity: &InflightTurnIdentity, state: &InflightTurnState) -> bool {
    identity.user_msg_id == state.user_msg_id
        && identity.started_at == state.started_at
        && identity.tmux_session_name == state.tmux_session_name
        && identity.turn_start_offset == state.turn_start_offset
}

fn provider_dir_in_root(root: &Path, provider: &ProviderKind, token_hash: &str) -> PathBuf {
    root.join(provider.as_str()).join(token_hash)
}

fn channel_file_path_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> PathBuf {
    provider_dir_in_root(root, provider, token_hash).join(format!("{channel_id}.json"))
}

fn load_channel_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> Vec<StatusPanelOrphanEntry> {
    let path = channel_file_path_in_root(root, provider, token_hash, channel_id);
    let Ok(raw) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str::<StatusPanelOrphanChannelFile>(&raw)
        .map(StatusPanelOrphanChannelFile::into_entries)
        .unwrap_or_default()
}

fn save_channel_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    entries: &[StatusPanelOrphanEntry],
) {
    let path = channel_file_path_in_root(root, provider, token_hash, channel_id);
    if entries.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(entries) {
        let _ = runtime_store::atomic_write(&path, &json);
    }
}

fn upsert_entry_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    entry: StatusPanelOrphanEntry,
) {
    if channel_id == 0 || entry.id == 0 {
        return;
    }
    let _guard = STORE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries = load_channel_in_root(root, provider, token_hash, channel_id);
    if let Some(existing) = entries.iter_mut().find(|existing| existing.id == entry.id) {
        match (existing.kind, entry.kind) {
            // A duplicate panel that failed to bind/delete must become an ordinary
            // stranded orphan immediately; otherwise the pending-bind live-panel
            // protection would delay self-heal for a panel no inflight row owns.
            (_, StatusPanelOrphanKind::Stranded) => {
                *existing = StatusPanelOrphanEntry::stranded(entry.id);
            }
            // Never downgrade an already-stranded delete retry back into a
            // pending bind. A stranded entry is an explicit delete intent.
            (StatusPanelOrphanKind::Stranded, StatusPanelOrphanKind::PendingBind) => {}
            (StatusPanelOrphanKind::PendingBind, StatusPanelOrphanKind::PendingBind) => {
                if existing.turn_identity.is_none() {
                    existing.turn_identity = entry.turn_identity;
                }
            }
        }
        save_channel_in_root(root, provider, token_hash, channel_id, &entries);
        return;
    }
    entries.push(entry);
    save_channel_in_root(root, provider, token_hash, channel_id, &entries);
}

fn enqueue_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    upsert_entry_in_root(
        root,
        provider,
        token_hash,
        channel_id,
        StatusPanelOrphanEntry::stranded(panel_msg_id),
    );
}

fn enqueue_pending_bind_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
    turn_identity: Option<InflightTurnIdentity>,
) {
    upsert_entry_in_root(
        root,
        provider,
        token_hash,
        channel_id,
        StatusPanelOrphanEntry::pending_bind(panel_msg_id, turn_identity),
    );
}

fn remove_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    let _guard = STORE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries = load_channel_in_root(root, provider, token_hash, channel_id);
    let before = entries.len();
    entries.retain(|entry| entry.id != panel_msg_id);
    if entries.len() != before {
        save_channel_in_root(root, provider, token_hash, channel_id, &entries);
    }
}

fn remove_pending_bind_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    let _guard = STORE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries = load_channel_in_root(root, provider, token_hash, channel_id);
    let before = entries.len();
    entries.retain(|entry| !(entry.id == panel_msg_id && entry.is_pending_bind()));
    if entries.len() != before {
        save_channel_in_root(root, provider, token_hash, channel_id, &entries);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum PendingBindOwnedRemovalOutcome {
    OwnershipMismatch,
    OwnedRecordRemoved,
    OwnedRecordMissing,
}

fn ensure_pending_bind_protection_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
    identity: &InflightTurnIdentity,
) {
    let _store_guard = STORE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries = load_channel_in_root(root, provider, token_hash, channel_id);
    if entries.iter().any(|entry| entry.id == panel_msg_id) {
        return;
    }
    entries.push(StatusPanelOrphanEntry::pending_bind(
        panel_msg_id,
        Some(identity.clone()),
    ));
    save_channel_in_root(root, provider, token_hash, channel_id, &entries);
}

fn remove_pending_bind_if_owned_in_root(
    root: &Path,
    inflight_root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
    identity: &InflightTurnIdentity,
) -> PendingBindOwnedRemovalOutcome {
    let inflight_path = crate::services::discord::inflight::inflight_state_path(
        inflight_root,
        provider,
        channel_id,
    );
    let Ok(_inflight_guard) =
        crate::services::discord::inflight::lock_inflight_state_path(&inflight_path)
    else {
        ensure_pending_bind_protection_in_root(
            root,
            provider,
            token_hash,
            channel_id,
            panel_msg_id,
            identity,
        );
        return PendingBindOwnedRemovalOutcome::OwnershipMismatch;
    };
    let Some(inflight) = fs::read_to_string(&inflight_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<InflightTurnState>(&raw).ok())
    else {
        ensure_pending_bind_protection_in_root(
            root,
            provider,
            token_hash,
            channel_id,
            panel_msg_id,
            identity,
        );
        return PendingBindOwnedRemovalOutcome::OwnershipMismatch;
    };
    if !identity.matches_state(&inflight) || inflight.status_message_id != Some(panel_msg_id) {
        ensure_pending_bind_protection_in_root(
            root,
            provider,
            token_hash,
            channel_id,
            panel_msg_id,
            identity,
        );
        return PendingBindOwnedRemovalOutcome::OwnershipMismatch;
    }

    let _store_guard = STORE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries = load_channel_in_root(root, provider, token_hash, channel_id);
    let before = entries.len();
    entries.retain(|entry| !(entry.id == panel_msg_id && entry.is_pending_bind()));
    if entries.len() == before {
        return PendingBindOwnedRemovalOutcome::OwnedRecordMissing;
    }
    save_channel_in_root(root, provider, token_hash, channel_id, &entries);
    PendingBindOwnedRemovalOutcome::OwnedRecordRemoved
}

fn is_queued_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) -> bool {
    load_channel_in_root(root, provider, token_hash, channel_id)
        .iter()
        .any(|entry| entry.id == panel_msg_id)
}

#[cfg(test)]
fn load_pending_in_root(root: &Path, provider: &ProviderKind, token_hash: &str) -> Vec<(u64, u64)> {
    load_pending_entries_in_root(root, provider, token_hash)
        .into_iter()
        .map(|(channel_id, entry)| (channel_id, entry.id))
        .collect()
}

fn load_pending_entries_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
) -> Vec<(u64, StatusPanelOrphanEntry)> {
    let dir = provider_dir_in_root(root, provider, token_hash);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(channel_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<u64>().ok())
        else {
            continue;
        };
        for entry in load_channel_in_root(root, provider, token_hash, channel_id) {
            out.push((channel_id, entry));
        }
    }
    out
}

/// Record a panel id for durable delete-retry. Idempotent (set semantics) so a
/// sweeper that re-observes the same orphan every pass does not grow the file.
/// #3351: also accepts watcher relay-placeholder ids (not only v2 panel ids) —
/// the drain semantics (delete or forget) are identical for both.
pub(in crate::services::discord) fn enqueue(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
        return;
    };
    enqueue_in_root(&root, provider, token_hash, channel_id, panel_msg_id);
}

/// Record a just-sent panel id as a pending bind: it is live-protected until the
/// inflight bind either lands, the same turn is still in the bind window, or the
/// record ages past the unclaimed grace and becomes an ordinary stranded delete.
pub(in crate::services::discord) fn enqueue_pending_bind(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
    turn_identity: Option<InflightTurnIdentity>,
) {
    let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
        return;
    };
    enqueue_pending_bind_in_root(
        &root,
        provider,
        token_hash,
        channel_id,
        panel_msg_id,
        turn_identity,
    );
}

fn should_record_separate_status_panel_orphan_for_flags(
    single_message_panel_enabled: bool,
    status_panel_v2_enabled: bool,
) -> bool {
    super::single_message_panel::separate_status_panel_enabled_for_flags(
        single_message_panel_enabled,
        status_panel_v2_enabled,
    )
}

fn enqueue_separate_status_panel_orphan_in_root_for_flags(
    root: &Path,
    single_message_panel_enabled: bool,
    status_panel_v2_enabled: bool,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    if !should_record_separate_status_panel_orphan_for_flags(
        single_message_panel_enabled,
        status_panel_v2_enabled,
    ) {
        return;
    }
    enqueue_in_root(root, provider, token_hash, channel_id, panel_msg_id);
}

/// Record a same-run separate status-panel orphan. Footer-mode turns never own a
/// separate status panel, so they must not grow this store. Transition cleanup
/// for stale flag-off panels uses the raw [`enqueue`] after an attempted sweeper
/// delete, because those are real legacy panel messages that still need retry.
pub(in crate::services::discord) fn enqueue_separate_status_panel_orphan(
    status_panel_v2_enabled: bool,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
        return;
    };
    enqueue_separate_status_panel_orphan_in_root_for_flags(
        &root,
        super::single_message_panel_enabled(),
        status_panel_v2_enabled,
        provider,
        token_hash,
        channel_id,
        panel_msg_id,
    );
}

/// All pending `(channel_id, panel_msg_id)` records for this bot.
#[cfg(test)]
pub(in crate::services::discord) fn load_pending(
    provider: &ProviderKind,
    token_hash: &str,
) -> Vec<(u64, u64)> {
    let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
        return Vec::new();
    };
    load_pending_in_root(&root, provider, token_hash)
}

/// Drop a record once its delete has committed (or the message is permanently
/// gone). No-op when the id is not present.
pub(in crate::services::discord) fn remove(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
        return;
    };
    remove_in_root(&root, provider, token_hash, channel_id, panel_msg_id);
}

/// Drop only pending-bind records for a completed live panel. Stranded delete
/// retries keep their original semantics.
pub(in crate::services::discord) fn remove_pending_bind(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
        return;
    };
    remove_pending_bind_in_root(&root, provider, token_hash, channel_id, panel_msg_id);
}

/// Drop a pending-bind record only while the same durable turn still owns the
/// exact panel. The inflight flock stays held through the orphan-store removal,
/// so a replacement owner cannot land between the ownership check and removal.
pub(in crate::services::discord) fn remove_pending_bind_if_owned(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
    identity: &InflightTurnIdentity,
) -> PendingBindOwnedRemovalOutcome {
    let (Some(root), Some(inflight_root)) = (
        runtime_store::discord_status_panel_orphans_root(),
        runtime_store::discord_inflight_root(),
    ) else {
        return PendingBindOwnedRemovalOutcome::OwnershipMismatch;
    };
    remove_pending_bind_if_owned_in_root(
        &root,
        &inflight_root,
        provider,
        token_hash,
        channel_id,
        panel_msg_id,
        identity,
    )
}

/// Is this panel still queued for deletion? Used by [`drain`] to re-validate a
/// record immediately before deleting it, so a record the completion path
/// removed (the panel became valid) after the drain's snapshot is not deleted.
pub(in crate::services::discord) fn is_queued(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) -> bool {
    let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
        return false;
    };
    is_queued_in_root(&root, provider, token_hash, channel_id, panel_msg_id)
}

fn delete_error_is_permanent(err: &serenity::Error) -> bool {
    matches!(err, serenity::Error::Http(http_err)
        if http_err
            .status_code()
            .is_some_and(|status| matches!(status.as_u16(), 404 | 403 | 410)))
}

/// #3607: emit the durable `relay_delete` observation for the orphan-store drain
/// delete (sweeper-class). Outcome mirrors the convergence branches:
/// `Ok` → committed, permanent `Err` (404/403/410) → already_gone, other
/// `Err` → failed. Panel deletes are non-terminal cleanups. Observation only —
/// the caller's removal / retry logic is unchanged.
fn emit_orphan_drain_delete(
    provider: &ProviderKind,
    channel_id: u64,
    panel_msg_id: u64,
    result: &Result<(), serenity::Error>,
) {
    let permanent = result.as_ref().err().is_some_and(delete_error_is_permanent);
    let outcome = super::placeholder_cleanup::panel_sweep_delete_outcome(result.is_ok(), permanent);
    let detail = result.as_ref().err().map(|err| err.to_string());
    crate::services::observability::emit_relay_delete(
        provider.as_str(),
        channel_id,
        panel_msg_id,
        None,
        None,
        "status_panel_orphan_store_drain",
        super::placeholder_cleanup::PlaceholderCleanupOperation::DeleteNonterminal.as_str(),
        outcome,
        detail.as_deref(),
    );
}

/// #3351: pure drain-defer decision for relay-placeholder records — `true` when
/// the live inflight row still anchors `candidate` as its `current_msg_id`.
fn orphan_drain_placeholder_is_live(current_msg_id: Option<u64>, candidate: u64) -> bool {
    candidate != 0 && current_msg_id == Some(candidate)
}

fn stranded_orphan_drain_should_delete(
    inflight_state: Option<&InflightTurnState>,
    candidate: u64,
) -> bool {
    if candidate == 0 {
        return false;
    }
    let legacy_owns = inflight_state.and_then(|state| state.status_message_id) == Some(candidate);
    let live_placeholder_owns = orphan_drain_placeholder_is_live(
        inflight_state.map(|state| state.current_msg_id),
        candidate,
    );
    !legacy_owns && !live_placeholder_owns
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingBindDrainOutcome {
    Missing,
    RemovedBoundPanel,
    Deferred,
    ReclassifiedToStranded,
    AlreadyStranded,
}

fn pending_bind_same_turn_window(
    entry: &StatusPanelOrphanEntry,
    inflight: Option<&InflightTurnState>,
) -> bool {
    let (Some(identity), Some(inflight)) = (entry.turn_identity.as_ref(), inflight) else {
        return false;
    };
    identity_matches_state(identity, inflight)
}

fn prepare_pending_bind_for_drain_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
    inflight: Option<&InflightTurnState>,
) -> PendingBindDrainOutcome {
    let _guard = STORE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries = load_channel_in_root(root, provider, token_hash, channel_id);
    let Some(index) = entries.iter().position(|entry| entry.id == panel_msg_id) else {
        return PendingBindDrainOutcome::Missing;
    };
    if !entries[index].is_pending_bind() {
        return PendingBindDrainOutcome::AlreadyStranded;
    }

    if inflight.and_then(|state| state.status_message_id) == Some(panel_msg_id) {
        entries.remove(index);
        save_channel_in_root(root, provider, token_hash, channel_id, &entries);
        return PendingBindDrainOutcome::RemovedBoundPanel;
    }

    if pending_bind_same_turn_window(&entries[index], inflight) {
        return PendingBindDrainOutcome::Deferred;
    }

    if entries[index].pending_bind_drain_cycles >= PENDING_BIND_GRACE_DRAIN_CYCLES {
        entries[index].reclassify_to_stranded();
        save_channel_in_root(root, provider, token_hash, channel_id, &entries);
        return PendingBindDrainOutcome::ReclassifiedToStranded;
    }

    entries[index].pending_bind_drain_cycles =
        entries[index].pending_bind_drain_cycles.saturating_add(1);
    save_channel_in_root(root, provider, token_hash, channel_id, &entries);
    PendingBindDrainOutcome::Deferred
}

fn prepare_pending_bind_for_drain(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
    inflight: Option<&InflightTurnState>,
) -> PendingBindDrainOutcome {
    let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
        return PendingBindDrainOutcome::Missing;
    };
    prepare_pending_bind_for_drain_in_root(
        &root,
        provider,
        token_hash,
        channel_id,
        panel_msg_id,
        inflight,
    )
}

/// Retry every pending panel delete once. A committed delete, or a permanent
/// "message gone" (404/403/410), drops the record; a transient failure keeps it
/// for the next pass. Returns the number of records cleared this pass.
pub(in crate::services::discord) async fn drain(
    http: &Arc<serenity::Http>,
    _shared: &Arc<crate::services::discord::SharedData>,
    provider: &ProviderKind,
    token_hash: &str,
) -> usize {
    let pending = {
        let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
            return 0;
        };
        load_pending_entries_in_root(&root, provider, token_hash)
    };
    let mut cleared = 0usize;
    for (channel_id, entry) in pending {
        let panel_msg_id = entry.id;
        // #3003 (codex P2 r26): re-validate against the live store immediately
        // before deleting. Between `load_pending` and here, the completion path may
        // have removed this record (the panel was completed and is now valid);
        // deleting from a stale snapshot would skip a record already cleaned up.
        // NOTE: this narrows but does not by itself close the check→delete gap — the
        // inflight gate below is what closes the TOCTOU against an in-flight
        // completion (see #3003 workflow r27).
        if !is_queued(provider, token_hash, channel_id, panel_msg_id) {
            continue;
        }
        // Pending-bind entries are crash-window protection for a just-sent live
        // panel. They are not delete intents until the bind window is conclusively
        // gone for at least two drain passes.
        let mut inflight_state =
            crate::services::discord::inflight::load_inflight_state(provider, channel_id);
        if entry.is_pending_bind() {
            match prepare_pending_bind_for_drain(
                provider,
                token_hash,
                channel_id,
                panel_msg_id,
                inflight_state.as_ref(),
            ) {
                PendingBindDrainOutcome::Missing => continue,
                PendingBindDrainOutcome::RemovedBoundPanel => {
                    cleared += 1;
                    continue;
                }
                PendingBindDrainOutcome::Deferred => continue,
                PendingBindDrainOutcome::ReclassifiedToStranded
                | PendingBindDrainOutcome::AlreadyStranded => {
                    inflight_state = crate::services::discord::inflight::load_inflight_state(
                        provider, channel_id,
                    );
                }
            }
        }
        if !is_queued(provider, token_hash, channel_id, panel_msg_id) {
            continue;
        }
        // #3003 (workflow r28): defer the delete only while the live inflight row
        // still owns THIS EXACT panel (`status_message_id == panel_msg_id`). In that
        // window the turn's completion/reclaim path may be editing the panel into its
        // final state, and the unlocked `delete_message` round-trip below would race
        // that completion and erase a freshly-finalized valid panel — the residual
        // TOCTOU the r26 `is_queued` recheck only narrows.
        //
        // Keying on turn identity (not bare channel presence, as r27 did) is required:
        // a channel-coarse gate deferred whenever ANY inflight existed, so a newer
        // turn re-occupying the channel — or a stale row pinned alive by a long-lived
        // tmux pane — would defer an OLD turn's orphan forever (the store is its only
        // reclaim path). A different/absent `status_message_id` means the live turn
        // does not own this orphan, so it is safe to delete now.
        if !stranded_orphan_drain_should_delete(inflight_state.as_ref(), panel_msg_id) {
            continue;
        }
        let channel = serenity::ChannelId::new(channel_id);
        let message = serenity::MessageId::new(panel_msg_id);
        let delete_result = channel.delete_message(http, message).await;
        // #3607: durable observability for the sweeper-class retry delete — classify
        // committed / already_gone (permanent 404/403/410) / failed using the SAME
        // `delete_error_is_permanent` match the convergence below uses (emit-only; no
        // behaviour change).
        emit_orphan_drain_delete(provider, channel_id, panel_msg_id, &delete_result);
        match delete_result {
            Ok(_) => {
                remove(provider, token_hash, channel_id, panel_msg_id);
                cleared += 1;
            }
            Err(err) if delete_error_is_permanent(&err) => {
                remove(provider, token_hash, channel_id, panel_msg_id);
                cleared += 1;
            }
            Err(err) => {
                tracing::debug!(
                    "[status_panel_orphan_store] retry delete for {channel_id}/{panel_msg_id} \
                     failed transiently — keeping for next drain: {err}"
                );
            }
        }
    }
    cleared
}

#[cfg(test)]
#[path = "status_panel_orphan_store_tests.rs"]
mod tests;
