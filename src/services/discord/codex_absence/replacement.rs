//! A Codex turn's planned pane replacement is not its terminal pane death.

use super::super::inflight::{InflightTurnIdentity, load_inflight_state_read_only_result};
use crate::services::provider::{CancelToken, ProviderKind, cancel_requested};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};

type Key = (u64, String);
type State = Mutex<Phase>;
static STATES: LazyLock<Mutex<HashMap<Key, Weak<State>>>> = LazyLock::new(Default::default);

#[derive(Default)]
struct Phase {
    generation: u64,
    active: bool,
    completed: bool,
    cleanup: bool,
    turn: Option<Turn>,
}

struct Turn {
    identity: InflightTurnIdentity,
    owner: u64,
    nonce: String,
    token: Weak<CancelToken>,
}

impl Turn {
    fn current(&self, key: &Key) -> bool {
        self.token.upgrade().is_some_and(|token| {
            !cancel_requested(Some(&token))
                && token.turn_nonce() == Some(self.nonce.as_str())
                && load_inflight_state_read_only_result(&ProviderKind::Codex, key.0)
                    .ok()
                    .flatten()
                    .is_some_and(|row| {
                        self.identity.matches_state(&row)
                            && row.request_owner_user_id == self.owner
                            && row.turn_nonce.as_deref() == Some(self.nonce.as_str())
                    })
        })
    }
}

fn state(key: &Key) -> Arc<State> {
    let mut states = STATES.lock().unwrap_or_else(|e| e.into_inner());
    states.retain(|_, value| value.strong_count() != 0);
    if let Some(state) = states.get(key).and_then(Weak::upgrade) {
        return state;
    }
    let state = Arc::new(State::default());
    states.insert(key.clone(), Arc::downgrade(&state));
    state
}

/// Owns only the kill/create interval; every early return releases it.
pub(crate) struct Replacement {
    state: Arc<State>,
    completed: bool,
}

impl Replacement {
    pub(crate) fn begin(
        channel: Option<u64>,
        tmux: &str,
        token: Option<&Arc<CancelToken>>,
    ) -> Result<Option<Self>, String> {
        let (Some(channel), Some(token)) = (channel, token) else {
            return Ok(None);
        };
        #[cfg(test)]
        if crate::services::claude_tui::source_verify::n2b_mutant("c-replacement-off") {
            return Ok(None);
        }
        let key = (channel, tmux.to_owned());
        let state = state(&key);
        let mut phase = state.lock().unwrap_or_else(|e| e.into_inner());
        if phase.active || phase.cleanup {
            return Err(
                "Codex pane replacement conflicts with an existing lifecycle action".into(),
            );
        }
        let row = load_inflight_state_read_only_result(&ProviderKind::Codex, channel)?
            .ok_or("Codex pane replacement lost its turn row")?;
        let nonce = token
            .turn_nonce()
            .ok_or("Codex pane replacement has no turn nonce")?;
        if cancel_requested(Some(token))
            || row.provider != "codex"
            || row.channel_id != channel
            || row.tmux_session_name.as_deref() != Some(tmux)
            || row.user_msg_id == 0
            || row.request_owner_user_id == 0
            || row.turn_nonce.as_deref() != Some(nonce)
        {
            return Err("Codex pane replacement no longer owns its turn".into());
        }
        phase.turn = Some(Turn {
            identity: InflightTurnIdentity::from_state(&row),
            owner: row.request_owner_user_id,
            nonce: nonce.to_owned(),
            token: Arc::downgrade(token),
        });
        phase.generation = phase.generation.wrapping_add(1);
        phase.active = true;
        phase.completed = false;
        drop(phase);
        Ok(Some(Self {
            state,
            completed: false,
        }))
    }

    /// The successor has completed spawn validation; its later death is real.
    pub(crate) fn complete(mut self) {
        self.completed = true;
    }
}

impl Drop for Replacement {
    fn drop(&mut self) {
        let mut phase = self.state.lock().unwrap_or_else(|e| e.into_inner());
        phase.active = false;
        phase.completed = self.completed;
        phase.generation = phase.generation.wrapping_add(1);
    }
}

/// Keeps the generation alive across a probe, including a whole replacement during its await.
pub(crate) struct Probe {
    key: Key,
    state: Arc<State>,
    generation: u64,
}

impl Probe {
    pub(crate) fn new(channel: u64, tmux: &str) -> Self {
        let key = (channel, tmux.to_owned());
        let state = state(&key);
        let generation = state.lock().unwrap_or_else(|e| e.into_inner()).generation;
        Self {
            key,
            state,
            generation,
        }
    }

    fn protected(&self, phase: &Phase) -> bool {
        let completed_gap = phase.completed && phase.generation != self.generation;
        #[cfg(test)]
        let completed_gap = completed_gap
            && !crate::services::claude_tui::source_verify::n2b_mutant("c-probe-generation-off");
        (phase.active || completed_gap)
            && phase
                .turn
                .as_ref()
                .is_some_and(|turn| turn.current(&self.key))
    }

    pub(crate) fn defer_death(&self) -> bool {
        self.protected(&self.state.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Admission and replacement begin share a lock; no mutex is held across async cleanup.
    pub(crate) fn cleanup(&self) -> Option<Cleanup> {
        let mut phase = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let protected = self.protected(&phase);
        #[cfg(test)]
        let protected =
            protected && !crate::services::claude_tui::source_verify::n2b_mutant("c-cleanup-open");
        if protected || phase.cleanup {
            return None;
        }
        phase.cleanup = true;
        Some(Cleanup {
            _held: Arc::new(CleanupState(Arc::clone(&self.state))),
        })
    }
}

#[derive(Clone)]
pub(crate) struct Cleanup {
    _held: Arc<CleanupState>,
}

struct CleanupState(Arc<State>);

impl Drop for CleanupState {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).cleanup = false;
    }
}

#[cfg(test)]
#[path = "replacement_tests.rs"]
mod tests;
