//! Lock-held exact episode guard for multi-step recovery mutations.

use super::*;

/// Exact, immutable provider-episode identity used by automatic recovery.
/// Includes every row axis consumed by the guarded handoff; mutable progress
/// fields remain outside the identity and are read from the lock-held state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct InflightEpisodePin {
    provider: String,
    channel_id: u64,
    channel_name: Option<String>,
    request_owner_user_id: u64,
    user_msg_id: u64,
    current_msg_id: u64,
    finalizer_turn_id: u64,
    started_at: String,
    tmux_session_name: Option<String>,
    session_id: Option<String>,
    output_path: Option<String>,
    input_fifo_path: Option<String>,
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    relay_owner_kind: RelayOwnerKind,
    turn_start_offset: Option<u64>,
    born_generation: u64,
    turn_nonce: Option<String>,
    terminal_delivery_committed: bool,
}

impl InflightEpisodePin {
    pub(in crate::services::discord) fn from_state(state: &InflightTurnState) -> Self {
        Self {
            provider: state.provider.clone(),
            channel_id: state.channel_id,
            channel_name: state.channel_name.clone(),
            request_owner_user_id: state.request_owner_user_id,
            user_msg_id: state.user_msg_id,
            current_msg_id: state.current_msg_id,
            finalizer_turn_id: state.finalizer_turn_id,
            started_at: state.started_at.clone(),
            tmux_session_name: state.tmux_session_name.clone(),
            session_id: state.session_id.clone(),
            output_path: state.output_path.clone(),
            input_fifo_path: state.input_fifo_path.clone(),
            runtime_kind: state.runtime_kind,
            relay_owner_kind: state.effective_relay_owner_kind(),
            turn_start_offset: state.turn_start_offset,
            born_generation: state.born_generation,
            turn_nonce: state.turn_nonce.clone(),
            terminal_delivery_committed: state.terminal_delivery_committed,
        }
    }

    pub(in crate::services::discord) fn matches_state(&self, state: &InflightTurnState) -> bool {
        *self == Self::from_state(state)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum InflightEpisodeLockError {
    Missing,
    Mismatch,
    Io,
}

/// Keeps the canonical inflight flock held while a synchronous watcher claim
/// and spawn handoff is performed for the exact adopted episode.
pub(in crate::services::discord) struct LockedInflightEpisode {
    _lock: super::store::InflightStateFileLock,
    root: std::path::PathBuf,
    path: std::path::PathBuf,
    state: InflightTurnState,
}

impl LockedInflightEpisode {
    pub(in crate::services::discord) fn state(&self) -> &InflightTurnState {
        &self.state
    }

    pub(in crate::services::discord::inflight) fn new(
        lock: super::store::InflightStateFileLock,
        root: std::path::PathBuf,
        path: std::path::PathBuf,
        state: InflightTurnState,
    ) -> Self {
        Self {
            _lock: lock,
            root,
            path,
            state,
        }
    }

    pub(in crate::services::discord) fn mark_readopted_under_guard(
        &mut self,
    ) -> GuardedSaveOutcome {
        if self.state.rebind_origin {
            return GuardedSaveOutcome::IdentityMismatch;
        }
        if self.state.readopted_from_inflight {
            return GuardedSaveOutcome::Saved;
        }
        self.state.readopted_from_inflight = true;
        persist_under_lock(
            &self.root,
            &self.path,
            &self.state,
            "src/services/discord/inflight/episode_guard.rs:mark_readopted_under_guard",
        )
        .map_or(GuardedSaveOutcome::IoError, |()| GuardedSaveOutcome::Saved)
    }
}

/// Atomically adopt one exact episode and retain the same canonical flock.
/// No reader or replacement writer can observe the adoption without ordering
/// after the full watcher handoff protected by the returned guard.
pub(in crate::services::discord) fn adopt_and_lock_inflight_episode(
    state: &InflightTurnState,
    expected_identity: &InflightTurnIdentity,
    expected_episode: &InflightEpisodePin,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset_for_rebase: Option<u64>,
) -> Result<LockedInflightEpisode, GuardedSaveOutcome> {
    let Some(root) = inflight_runtime_root() else {
        return Err(GuardedSaveOutcome::IoError);
    };
    let (lock, state) = super::save_store::identity_gate::lock_and_save_existing_inflight_rebind_adoption_impl_in_root(
        &root,
        state,
        expected_identity,
        Some(expected_episode),
        expected_turn_start_offset,
        expected_last_offset_for_rebase,
    )?;
    let provider = state.provider_kind().ok_or(GuardedSaveOutcome::IoError)?;
    let path = inflight_state_path(&root, &provider, state.channel_id);
    Ok(LockedInflightEpisode::new(lock, root, path, state))
}

pub(in crate::services::discord) fn lock_inflight_episode(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightEpisodePin,
) -> Result<LockedInflightEpisode, InflightEpisodeLockError> {
    let root = inflight_runtime_root().ok_or(InflightEpisodeLockError::Missing)?;
    let path = inflight_state_path(&root, provider, channel_id);
    let lock = lock_inflight_state_path(&path).map_err(|_| InflightEpisodeLockError::Io)?;
    let state = load_inflight_state_unlocked(&path).ok_or(InflightEpisodeLockError::Missing)?;
    if !expected.matches_state(&state) {
        return Err(InflightEpisodeLockError::Mismatch);
    }
    Ok(LockedInflightEpisode {
        _lock: lock,
        root,
        path,
        state,
    })
}
