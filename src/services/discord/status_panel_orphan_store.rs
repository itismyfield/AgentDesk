//! Durable per-panel retry store for status-panel deletes and pending binds.
//!
//! New writers never read-modify-write the legacy per-channel aggregate file.
//! Every `(channel, panel)` has its own atomic file, so concurrent processes and
//! rolling old/new dcserver writers cannot overwrite another new-writer entry.
//! A tombstone suppresses a removed legacy aggregate entry without rewriting the
//! legacy file. Malformed canonical files fail closed and are never overwritten.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};

use crate::services::discord::inflight::{InflightTurnIdentity, InflightTurnState};
use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum PendingBindOwnedRemovalOutcome {
    Removed,
    NotOwned,
    Deferred,
    DurabilityFailure(String),
}

fn ensure_pending_bind_protection_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
    identity: &InflightTurnIdentity,
) -> Result<(), String> {
    enqueue_pending_bind_in_root(
        root,
        provider,
        token_hash,
        channel_id,
        panel_msg_id,
        Some(identity.clone()),
    )
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
    let _inflight_guard =
        match crate::services::discord::inflight::lock_inflight_state_path(&inflight_path) {
            Ok(guard) => guard,
            Err(error) => return PendingBindOwnedRemovalOutcome::DurabilityFailure(error),
        };
    let inflight = match fs::read_to_string(&inflight_path) {
        Ok(raw) => match serde_json::from_str::<InflightTurnState>(&raw) {
            Ok(state) => state,
            Err(error) => {
                return PendingBindOwnedRemovalOutcome::DurabilityFailure(error.to_string());
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return match ensure_pending_bind_protection_in_root(
                root,
                provider,
                token_hash,
                channel_id,
                panel_msg_id,
                identity,
            ) {
                Ok(()) => PendingBindOwnedRemovalOutcome::NotOwned,
                Err(error) => PendingBindOwnedRemovalOutcome::DurabilityFailure(error),
            };
        }
        Err(error) => return PendingBindOwnedRemovalOutcome::DurabilityFailure(error.to_string()),
    };
    if !identity.matches_state(&inflight) || inflight.status_message_id != Some(panel_msg_id) {
        return match ensure_pending_bind_protection_in_root(
            root,
            provider,
            token_hash,
            channel_id,
            panel_msg_id,
            identity,
        ) {
            Ok(()) => PendingBindOwnedRemovalOutcome::NotOwned,
            Err(error) => PendingBindOwnedRemovalOutcome::DurabilityFailure(error),
        };
    }
    match remove_pending_bind_in_root_checked(root, provider, token_hash, channel_id, panel_msg_id)
    {
        Ok(()) => PendingBindOwnedRemovalOutcome::Removed,
        Err(error) => PendingBindOwnedRemovalOutcome::DurabilityFailure(error),
    }
}

fn is_queued_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) -> bool {
    load_channel_result_in_root(root, provider, token_hash, channel_id)
        .map(|entries| entries.iter().any(|entry| entry.id == panel_msg_id))
        .unwrap_or(true)
}

fn discover_channels_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
) -> Result<HashSet<u64>, String> {
    let dir = provider_dir_in_root(root, provider, token_hash);
    let files = match fs::read_dir(dir) {
        Ok(files) => files,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut channels = HashSet::new();
    for file in files {
        let file = file.map_err(|error| error.to_string())?;
        let path = file.path();
        let raw = if path.is_dir() {
            path.file_name().and_then(|value| value.to_str())
        } else if path.extension().and_then(|value| value.to_str()) == Some("json") {
            path.file_stem().and_then(|value| value.to_str())
        } else {
            None
        };
        if let Some(channel) = raw.and_then(|value| value.parse::<u64>().ok()) {
            channels.insert(channel);
        }
    }
    Ok(channels)
}

fn load_pending_entries_result_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
) -> Result<Vec<(u64, StatusPanelOrphanEntry)>, String> {
    let mut out = Vec::new();
    for channel_id in discover_channels_in_root(root, provider, token_hash)? {
        for entry in load_channel_result_in_root(root, provider, token_hash, channel_id)? {
            out.push((channel_id, entry));
        }
    }
    Ok(out)
}

fn load_pending_entries_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
) -> Vec<(u64, StatusPanelOrphanEntry)> {
    load_pending_entries_result_in_root(root, provider, token_hash).unwrap_or_default()
}

#[cfg(test)]
fn load_pending_in_root(root: &Path, provider: &ProviderKind, token_hash: &str) -> Vec<(u64, u64)> {
    load_pending_entries_in_root(root, provider, token_hash)
        .into_iter()
        .map(|(channel_id, entry)| (channel_id, entry.id))
        .collect()
}

pub(in crate::services::discord) fn enqueue(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
        return;
    };
    if let Err(error) = enqueue_in_root(&root, provider, token_hash, channel_id, panel_msg_id) {
        tracing::warn!(channel_id, panel_msg_id, error = %error, "failed to persist status-panel orphan");
    }
}

pub(in crate::services::discord) fn enqueue_pending_bind(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
    turn_identity: Option<InflightTurnIdentity>,
) -> Result<(), String> {
    let root = runtime_store::discord_status_panel_orphans_root()
        .ok_or_else(|| "AgentDesk runtime root unavailable".to_string())?;
    enqueue_pending_bind_in_root(
        &root,
        provider,
        token_hash,
        channel_id,
        panel_msg_id,
        turn_identity,
    )
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
    if should_record_separate_status_panel_orphan_for_flags(
        single_message_panel_enabled,
        status_panel_v2_enabled,
    ) {
        let _ = enqueue_in_root(root, provider, token_hash, channel_id, panel_msg_id);
    }
}

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

pub(in crate::services::discord) fn remove_checked(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) -> Result<(), String> {
    let root = runtime_store::discord_status_panel_orphans_root()
        .ok_or_else(|| "AgentDesk runtime root unavailable".to_string())?;
    remove_in_root_checked(&root, provider, token_hash, channel_id, panel_msg_id)
}

pub(in crate::services::discord) fn remove(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    let _ = remove_checked(provider, token_hash, channel_id, panel_msg_id);
}

pub(in crate::services::discord) fn remove_pending_bind_checked(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) -> Result<(), String> {
    let root = runtime_store::discord_status_panel_orphans_root()
        .ok_or_else(|| "AgentDesk runtime root unavailable".to_string())?;
    remove_pending_bind_in_root_checked(&root, provider, token_hash, channel_id, panel_msg_id)
}

pub(in crate::services::discord) fn remove_pending_bind(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) {
    let _ = remove_pending_bind_checked(provider, token_hash, channel_id, panel_msg_id);
}

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
        return PendingBindOwnedRemovalOutcome::DurabilityFailure(
            "status panel transition roots unavailable".to_string(),
        );
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

pub(in crate::services::discord) fn is_queued(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_msg_id: u64,
) -> bool {
    let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
        return true;
    };
    is_queued_in_root(&root, provider, token_hash, channel_id, panel_msg_id)
}

fn delete_status_is_permanent(status: u16) -> bool {
    matches!(status, 404 | 403 | 410)
}

pub(in crate::services::discord) fn delete_error_is_permanent(err: &serenity::Error) -> bool {
    matches!(err, serenity::Error::Http(http_err)
        if http_err
            .status_code()
            .is_some_and(|status| delete_status_is_permanent(status.as_u16())))
}

fn emit_orphan_drain_delete(
    provider: &ProviderKind,
    channel_id: u64,
    panel_msg_id: u64,
    result: &Result<(), serenity::Error>,
) {
    let permanent = result.as_ref().err().is_some_and(delete_error_is_permanent);
    let outcome = super::placeholder_cleanup::panel_sweep_delete_outcome(result.is_ok(), permanent);
    let detail = result.as_ref().err().map(|error| error.to_string());
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

fn orphan_drain_placeholder_is_live(current_msg_id: Option<u64>, candidate: u64) -> bool {
    candidate != 0 && current_msg_id == Some(candidate)
}

fn stranded_orphan_drain_should_delete(
    inflight_state: Option<&InflightTurnState>,
    singleton: &crate::services::discord::status_panel_singleton_store::StatusPanelSingletonLoadOutcome,
    candidate: u64,
) -> bool {
    use crate::services::discord::status_panel_singleton_store::StatusPanelSingletonLoadOutcome;
    if candidate == 0
        || matches!(
            singleton,
            StatusPanelSingletonLoadOutcome::DurabilityFailure(_)
        )
    {
        return false;
    }
    let singleton_owns = matches!(
        singleton,
        StatusPanelSingletonLoadOutcome::Present(binding)
            if binding.panel_message_id == candidate
    );
    let legacy_owns = inflight_state.and_then(|state| state.status_message_id) == Some(candidate);
    let placeholder_owns = orphan_drain_placeholder_is_live(
        inflight_state.map(|state| state.current_msg_id),
        candidate,
    );
    !singleton_owns && !legacy_owns && !placeholder_owns
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingBindDrainOutcome {
    Missing,
    Deferred,
    ReclassifiedToStranded,
    AlreadyStranded,
}

mod persistence;
use persistence::*;

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
    persistence::with_channel_lock(root, provider, token_hash, channel_id, || {
        let entries =
            match load_channel_result_locked_in_root(root, provider, token_hash, channel_id) {
                Ok(entries) => entries,
                Err(_) => return PendingBindDrainOutcome::Deferred,
            };
        let Some(mut entry) = entries.into_iter().find(|entry| entry.id == panel_msg_id) else {
            return PendingBindDrainOutcome::Missing;
        };
        if !entry.is_pending_bind() {
            return PendingBindDrainOutcome::AlreadyStranded;
        }
        if inflight.and_then(|state| state.status_message_id) == Some(panel_msg_id)
            || pending_bind_same_turn_window(&entry, inflight)
        {
            return PendingBindDrainOutcome::Deferred;
        }
        if entry.pending_bind_drain_cycles >= PENDING_BIND_GRACE_DRAIN_CYCLES {
            entry.reclassify_to_stranded();
            return match save_entry_in_root(root, provider, token_hash, channel_id, &entry) {
                Ok(()) => PendingBindDrainOutcome::ReclassifiedToStranded,
                Err(_) => PendingBindDrainOutcome::Deferred,
            };
        }
        entry.pending_bind_drain_cycles = entry.pending_bind_drain_cycles.saturating_add(1);
        let _ = save_entry_in_root(root, provider, token_hash, channel_id, &entry);
        PendingBindDrainOutcome::Deferred
    })
    .unwrap_or(PendingBindDrainOutcome::Deferred)
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

pub(in crate::services::discord) async fn drain(
    http: &Arc<serenity::Http>,
    shared: &Arc<crate::services::discord::SharedData>,
    provider: &ProviderKind,
    token_hash: &str,
) -> usize {
    drain_with_delete(
        shared,
        provider,
        token_hash,
        |channel, message| async move { channel.delete_message(http, message).await },
    )
    .await
}

async fn drain_with_delete<D, DeleteFuture>(
    _shared: &Arc<crate::services::discord::SharedData>,
    provider: &ProviderKind,
    token_hash: &str,
    mut delete_message: D,
) -> usize
where
    D: FnMut(serenity::ChannelId, serenity::MessageId) -> DeleteFuture,
    DeleteFuture: std::future::Future<Output = Result<(), serenity::Error>>,
{
    let pending = {
        let Some(root) = runtime_store::discord_status_panel_orphans_root() else {
            return 0;
        };
        match load_pending_entries_result_in_root(&root, provider, token_hash) {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(error = %error, "status-panel orphan store failed closed");
                return 0;
            }
        }
    };
    let mut cleared = 0;
    for (channel_id, entry) in pending {
        let panel_msg_id = entry.id;
        if !is_queued(provider, token_hash, channel_id, panel_msg_id) {
            continue;
        }
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
                PendingBindDrainOutcome::Missing | PendingBindDrainOutcome::Deferred => continue,
                PendingBindDrainOutcome::ReclassifiedToStranded
                | PendingBindDrainOutcome::AlreadyStranded => {
                    inflight_state = crate::services::discord::inflight::load_inflight_state(
                        provider, channel_id,
                    );
                }
            }
        }
        let singleton = crate::services::discord::status_panel_singleton_store::load_typed(
            provider, token_hash, channel_id,
        );
        if !stranded_orphan_drain_should_delete(inflight_state.as_ref(), &singleton, panel_msg_id) {
            continue;
        }
        let result = delete_message(
            serenity::ChannelId::new(channel_id),
            serenity::MessageId::new(panel_msg_id),
        )
        .await;
        emit_orphan_drain_delete(provider, channel_id, panel_msg_id, &result);
        let retirement = match &result {
            Ok(()) => Some(
                crate::services::discord::status_panel_transition::StatusPanelRetirementOutcome::Removed,
            ),
            Err(error) if delete_error_is_permanent(error) => Some(
                crate::services::discord::status_panel_transition::StatusPanelRetirementOutcome::PermanentAbsent,
            ),
            Err(error) => {
                tracing::debug!(
                    channel_id,
                    panel_msg_id,
                    error = %error,
                    "status-panel orphan delete remains pending"
                );
                None
            }
        };
        if let Some(retirement) = retirement
            && crate::services::discord::status_panel_transition::finalize_retirement(
                provider,
                token_hash,
                channel_id,
                panel_msg_id,
                retirement,
            )
            .unwrap_or(false)
        {
            cleared += 1;
        }
    }
    cleared
}

#[cfg(test)]
#[path = "status_panel_orphan_store_tests.rs"]
mod tests;
