//! Explicit status-panel ownership transition authority (#4891).
//!
//! `discord_inflight`, singleton, and orphan files cannot be committed as one
//! filesystem transaction. Every multi-file ownership change therefore writes an
//! intent first. The intent is the recovery authority until the reducer reaches a
//! terminal state and removes it. Replay is idempotent.
//!
//! Lock order for synchronous transitions is journal → inflight → singleton →
//! orphan. Transport code never holds these locks across Discord HTTP awaits.
//!
//! Crash boundaries:
//! - before intent write: no transition exists;
//! - after intent, before inflight/singleton/orphan write: replay resumes the same
//!   exact turn-identity/generation guarded transition;
//! - after ownership write, before intent removal: replay observes the committed
//!   state and removes only the stale intent;
//! - any read/write ambiguity: `DeferDurability`; never destructive ownership.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::inflight::InflightTurnIdentity;
use super::runtime_store;
use super::status_panel_orphan_store::{self, PendingBindOwnedRemovalOutcome};
use super::status_panel_singleton_store;
use crate::services::provider::ProviderKind;

static TRANSITION_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum StatusPanelTransitionState {
    CandidateSent,
    PendingBindDurable,
    OwnershipCommitted,
    RetirementDurable,
    Unreconciled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum StatusPanelTransitionEvent {
    CandidateObserved,
    SingletonCommitted,
    PendingBindCleanup,
    CandidateNotOwned,
    Retry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum StatusPanelTransitionAction {
    KeepCurrent { generation: Option<u64> },
    AdoptFallback { generation: u64 },
    RetireCandidate,
    DeferDurability { error: String },
    RecoverUnreconciled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct StatusPanelTransitionIntent {
    pub provider: String,
    pub token_hash: String,
    pub channel_id: u64,
    pub candidate_panel_id: u64,
    pub prior_panel_id: Option<u64>,
    pub generation: u64,
    pub identity: Option<InflightTurnIdentity>,
    pub state: StatusPanelTransitionState,
}

fn root() -> Result<PathBuf, String> {
    runtime_store::discord_status_panel_transitions_root()
        .ok_or_else(|| "status panel transition root unavailable".to_string())
}

fn path_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
) -> PathBuf {
    root.join(provider.as_str())
        .join(token_hash)
        .join(channel_id.to_string())
        .join(format!("{candidate_panel_id}.json"))
}

fn load_in_root(path: &Path) -> Result<Option<StatusPanelTransitionIntent>, String> {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map(Some)
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn save_in_root(path: &Path, intent: &StatusPanelTransitionIntent) -> Result<(), String> {
    let json = serde_json::to_string_pretty(intent).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(path, &json)
}

fn remove_in_root(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

pub(in crate::services::discord) fn record_candidate_intent(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
    prior_panel_id: Option<u64>,
    generation: u64,
    identity: Option<InflightTurnIdentity>,
) -> Result<(), String> {
    let _guard = TRANSITION_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let root = root()?;
    let path = path_in_root(&root, provider, token_hash, channel_id, candidate_panel_id);
    let intent = StatusPanelTransitionIntent {
        provider: provider.as_str().to_string(),
        token_hash: token_hash.to_string(),
        channel_id,
        candidate_panel_id,
        prior_panel_id,
        generation,
        identity,
        state: StatusPanelTransitionState::CandidateSent,
    };
    save_in_root(&path, &intent)
}

pub(in crate::services::discord) fn begin_candidate(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
    prior_panel_id: Option<u64>,
    generation: u64,
    identity: Option<InflightTurnIdentity>,
) -> StatusPanelTransitionAction {
    if let Err(error) = record_candidate_intent(
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
        prior_panel_id,
        generation,
        identity.clone(),
    ) {
        return StatusPanelTransitionAction::DeferDurability { error };
    }
    match status_panel_orphan_store::enqueue_pending_bind(
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
        identity,
    ) {
        Ok(()) => {
            match mark_pending_bind_durable(provider, token_hash, channel_id, candidate_panel_id) {
                Ok(()) => StatusPanelTransitionAction::KeepCurrent { generation: None },
                Err(error) => StatusPanelTransitionAction::DeferDurability { error },
            }
        }
        Err(_) => {
            let _ = update_state(
                provider,
                token_hash,
                channel_id,
                candidate_panel_id,
                StatusPanelTransitionState::Unreconciled,
            );
            StatusPanelTransitionAction::RecoverUnreconciled
        }
    }
}

pub(in crate::services::discord) fn mark_pending_bind_durable(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
) -> Result<(), String> {
    update_state(
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
        StatusPanelTransitionState::PendingBindDurable,
    )
}

pub(in crate::services::discord) fn commit_bound_candidate(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
    identity: &InflightTurnIdentity,
    expected_generation: u64,
    desired_generation: Option<u64>,
) -> StatusPanelTransitionAction {
    let binding = match status_panel_singleton_store::bind_if_owned_guarded(
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
        desired_generation,
        Some(identity),
        Some(expected_generation),
    ) {
        status_panel_singleton_store::GuardedSingletonBindOutcome::Committed(binding) => binding,
        status_panel_singleton_store::GuardedSingletonBindOutcome::NotOwned => {
            return StatusPanelTransitionAction::RetireCandidate;
        }
        status_panel_singleton_store::GuardedSingletonBindOutcome::DurabilityFailure(error) => {
            return StatusPanelTransitionAction::DeferDurability { error };
        }
    };
    resolve_after_singleton_commit(
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
        identity,
        binding.generation,
    )
}

pub(in crate::services::discord) fn resolve_after_singleton_commit(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
    identity: &InflightTurnIdentity,
    generation: u64,
) -> StatusPanelTransitionAction {
    let _guard = TRANSITION_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let root = match root() {
        Ok(root) => root,
        Err(error) => return StatusPanelTransitionAction::DeferDurability { error },
    };
    let path = path_in_root(&root, provider, token_hash, channel_id, candidate_panel_id);
    let mut intent = match load_in_root(&path) {
        Ok(Some(intent)) if intent.candidate_panel_id == candidate_panel_id => intent,
        Ok(_) => {
            return StatusPanelTransitionAction::DeferDurability {
                error: "status panel transition intent missing".to_string(),
            };
        }
        Err(error) => return StatusPanelTransitionAction::DeferDurability { error },
    };
    intent.state = StatusPanelTransitionState::OwnershipCommitted;
    if let Err(error) = save_in_root(&path, &intent) {
        return StatusPanelTransitionAction::DeferDurability { error };
    }

    match status_panel_orphan_store::remove_pending_bind_if_owned(
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
        identity,
    ) {
        PendingBindOwnedRemovalOutcome::Removed => {
            if let Err(error) = remove_in_root(&path) {
                return StatusPanelTransitionAction::DeferDurability { error };
            }
            StatusPanelTransitionAction::KeepCurrent {
                generation: Some(generation),
            }
        }
        PendingBindOwnedRemovalOutcome::NotOwned => StatusPanelTransitionAction::RetireCandidate,
        PendingBindOwnedRemovalOutcome::Deferred => {
            StatusPanelTransitionAction::RecoverUnreconciled
        }
        PendingBindOwnedRemovalOutcome::DurabilityFailure(error) => {
            StatusPanelTransitionAction::DeferDurability { error }
        }
    }
}

pub(in crate::services::discord) fn mark_retirement_durable(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
) -> Result<(), String> {
    update_state(
        provider,
        token_hash,
        channel_id,
        candidate_panel_id,
        StatusPanelTransitionState::RetirementDurable,
    )
}

fn update_state(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    candidate_panel_id: u64,
    state: StatusPanelTransitionState,
) -> Result<(), String> {
    let _guard = TRANSITION_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let root = root()?;
    let path = path_in_root(&root, provider, token_hash, channel_id, candidate_panel_id);
    let mut intent =
        load_in_root(&path)?.ok_or_else(|| "status panel transition intent missing".to_string())?;
    if intent.candidate_panel_id != candidate_panel_id {
        return Err("status panel transition candidate changed".to_string());
    }
    intent.state = state;
    save_in_root(&path, &intent)
}

pub(in crate::services::discord) async fn recover_unreconciled_with_delete<D, DeleteFuture>(
    provider: &ProviderKind,
    token_hash: &str,
    mut delete_candidate: D,
) -> usize
where
    D: FnMut(u64, u64) -> DeleteFuture,
    DeleteFuture: std::future::Future<Output = bool>,
{
    let intents = match load_unreconciled(provider, token_hash) {
        Ok(intents) => intents,
        Err(_) => return 0,
    };
    let mut resolved = 0;
    for intent in intents {
        let action = match intent.state {
            StatusPanelTransitionState::OwnershipCommitted => intent
                .identity
                .as_ref()
                .map(|identity| {
                    resolve_after_singleton_commit(
                        provider,
                        token_hash,
                        intent.channel_id,
                        intent.candidate_panel_id,
                        identity,
                        intent.generation,
                    )
                })
                .unwrap_or(StatusPanelTransitionAction::RecoverUnreconciled),
            StatusPanelTransitionState::RetirementDurable => {
                StatusPanelTransitionAction::RetireCandidate
            }
            StatusPanelTransitionState::CandidateSent
            | StatusPanelTransitionState::PendingBindDurable
            | StatusPanelTransitionState::Unreconciled => {
                StatusPanelTransitionAction::RecoverUnreconciled
            }
        };
        match action {
            StatusPanelTransitionAction::KeepCurrent { .. } => resolved += 1,
            StatusPanelTransitionAction::RetireCandidate
            | StatusPanelTransitionAction::RecoverUnreconciled => {
                if delete_candidate(intent.channel_id, intent.candidate_panel_id).await {
                    if let Ok(root) = root() {
                        let path = path_in_root(
                            &root,
                            provider,
                            token_hash,
                            intent.channel_id,
                            intent.candidate_panel_id,
                        );
                        if remove_in_root(&path).is_ok() {
                            resolved += 1;
                        }
                    }
                }
            }
            StatusPanelTransitionAction::AdoptFallback { .. }
            | StatusPanelTransitionAction::DeferDurability { .. } => {}
        }
    }
    resolved
}

pub(in crate::services::discord) fn load_unreconciled(
    provider: &ProviderKind,
    token_hash: &str,
) -> Result<Vec<StatusPanelTransitionIntent>, String> {
    let root = root()?.join(provider.as_str()).join(token_hash);
    let channels = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut intents = HashMap::new();
    for channel in channels.flatten() {
        let candidates = match fs::read_dir(channel.path()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        for candidate in candidates.flatten() {
            if let Some(intent) = load_in_root(&candidate.path())? {
                intents.insert((intent.channel_id, intent.candidate_panel_id), intent);
            }
        }
    }
    Ok(intents.into_values().collect())
}

pub(in crate::services::discord) fn reducer(
    state: StatusPanelTransitionState,
    event: StatusPanelTransitionEvent,
    exact_owner: bool,
    durability_ok: bool,
    generation: u64,
) -> StatusPanelTransitionAction {
    if !durability_ok {
        return StatusPanelTransitionAction::DeferDurability {
            error: "status panel transition durability unavailable".to_string(),
        };
    }
    match (state, event, exact_owner) {
        (
            StatusPanelTransitionState::CandidateSent,
            StatusPanelTransitionEvent::CandidateObserved,
            true,
        ) => StatusPanelTransitionAction::AdoptFallback { generation },
        (
            StatusPanelTransitionState::OwnershipCommitted,
            StatusPanelTransitionEvent::PendingBindCleanup,
            true,
        ) => StatusPanelTransitionAction::KeepCurrent {
            generation: Some(generation),
        },
        (_, StatusPanelTransitionEvent::CandidateNotOwned, false) => {
            StatusPanelTransitionAction::RetireCandidate
        }
        (StatusPanelTransitionState::Unreconciled, StatusPanelTransitionEvent::Retry, _) => {
            StatusPanelTransitionAction::RecoverUnreconciled
        }
        _ => StatusPanelTransitionAction::RecoverUnreconciled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reducer_never_turns_durability_uncertainty_into_retirement_4891() {
        assert!(matches!(
            reducer(
                StatusPanelTransitionState::OwnershipCommitted,
                StatusPanelTransitionEvent::PendingBindCleanup,
                true,
                false,
                7,
            ),
            StatusPanelTransitionAction::DeferDurability { .. }
        ));
    }

    #[test]
    fn intent_replay_survives_every_write_boundary_4891() {
        let root = tempfile::tempdir().expect("transition root");
        let provider = ProviderKind::Claude;
        let path = path_in_root(root.path(), &provider, "tok", 42, 99);
        let intent = StatusPanelTransitionIntent {
            provider: provider.as_str().to_string(),
            token_hash: "tok".to_string(),
            channel_id: 42,
            candidate_panel_id: 99,
            prior_panel_id: Some(98),
            generation: 7,
            identity: None,
            state: StatusPanelTransitionState::CandidateSent,
        };
        save_in_root(&path, &intent).expect("intent first");
        assert_eq!(load_in_root(&path).expect("replay"), Some(intent));
        remove_in_root(&path).expect("terminal remove");
        assert_eq!(load_in_root(&path).expect("removed"), None);
    }
}
