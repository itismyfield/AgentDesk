//! Verified native input attached to its durable birth episode, without cancellation effects.
use super::*;
use crate::services::provider::{CancelToken, native_stop::NativeUserPin};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct VerifiedNativeUserPin {
    input: NativeUserPin,
    provider: String,
    channel_id: u64,
    request_owner_user_id: u64,
    user_msg_id: u64,
    turn_nonce: String,
    started_at: String,
}

impl VerifiedNativeUserPin {
    fn new(row: &InflightTurnState, input: NativeUserPin) -> Option<Self> {
        let pin = Self {
            input,
            provider: row.provider.clone(),
            channel_id: row.channel_id,
            request_owner_user_id: row.request_owner_user_id,
            user_msg_id: row.user_msg_id,
            turn_nonce: row.turn_nonce.clone().filter(|n| !n.is_empty())?,
            started_at: row.started_at.clone(),
        };
        // A receipt may precede the runtime handoff; adoption rechecks source coordinates.
        pin.matches_owner(row).then_some(pin)
    }
    fn matches_owner(&self, row: &InflightTurnState) -> bool {
        self.provider == row.provider
            && self.provider == "claude"
            && self.channel_id == row.channel_id
            && self.request_owner_user_id == row.request_owner_user_id
            && self.user_msg_id == row.user_msg_id
            && row.turn_nonce.as_deref() == Some(self.turn_nonce.as_str())
            && self.started_at == row.started_at
            && !row.has_native_herdr_input()
    }
    fn matches_source(&self, row: &InflightTurnState) -> bool {
        self.matches_owner(row)
            && row.runtime_kind
                == Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui)
            && row.output_path.as_deref() == Some(self.input.output_path.as_str())
            && row.tmux_session_name.as_deref() == Some(self.input.tmux_session.as_str())
            && row
                .session_id
                .as_deref()
                .is_none_or(|sid| sid == self.input.session_id)
    }
}

/// A stale same-episode snapshot keeps the verified pin; a successor never inherits it.
pub(super) fn carry_forward(
    existing: Option<&InflightTurnState>,
    outgoing: &mut InflightTurnState,
) {
    if outgoing
        .native_stop_user_pin
        .as_ref()
        .is_some_and(|pin| !pin.matches_owner(outgoing))
    {
        outgoing.native_stop_user_pin = None;
    }
    if let Some(pin) = existing
        .and_then(|row| {
            row.native_stop_user_pin
                .as_ref()
                .filter(|pin| pin.matches_owner(row))
        })
        .filter(|pin| pin.matches_owner(outgoing))
    {
        outgoing.native_stop_user_pin = Some(pin.clone());
    }
}

pub(in crate::services::discord) fn bind_native_stop_pin(
    row: &InflightTurnState,
    token: &Arc<CancelToken>,
) {
    if row.turn_nonce.as_deref().filter(|n| !n.is_empty()) != token.turn_nonce()
        || row.provider != "claude"
        || row.has_native_herdr_input()
    {
        return;
    }
    if let Some(pin) = row
        .native_stop_user_pin
        .as_ref()
        .filter(|pin| pin.matches_source(row))
    {
        token
            .native_stop
            .receipt_sink()
            .restore_candidate(pin.input.clone());
    }
    let episode = InflightEpisodePin::from_state(row);
    let channel = row.channel_id;
    token
        .native_stop
        .receipt_sink()
        .on_pin(Arc::new(move |input| {
            if input.spawn_nonce.is_none() {
                return false;
            }
            let Ok(mut locked) = episode_guard::lock_inflight_birth_episode(
                &ProviderKind::Claude,
                channel,
                &episode,
            ) else {
                return false;
            };
            let mut updated = locked.state().clone();
            let Some(pin) = VerifiedNativeUserPin::new(&updated, input.clone()) else {
                return false;
            };
            if let Some(existing) = &updated.native_stop_user_pin {
                return *existing == pin;
            }
            updated.native_stop_user_pin = Some(pin);
            locked.persist_verified_under_guard(&updated, "native_input_pin")
                == GuardedSaveOutcome::Saved
        }));
}

/// Recovery keeps the proof across an app generation as a candidate the stop rechecks.
pub(in crate::services::discord) fn restore_native_stop_pin(
    row: &InflightTurnState,
    token: &Arc<CancelToken>,
) {
    let restored = row
        .native_stop_user_pin
        .as_ref()
        .filter(|pin| pin.matches_source(row));
    if restored.is_some_and(|pin| token.turn_nonce() == Some(pin.turn_nonce.as_str())) {
        bind_native_stop_pin(row, token);
    }
}
