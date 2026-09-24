use std::sync::atomic::Ordering;

use poise::serenity_prelude::{ChannelId, MessageId};

use super::turn_finished_signal::reset_turn_finished_signal;
use super::{ChannelMailboxRegistry, ChannelMailboxState};
use crate::services::provider::CancelToken;

/// Outcome of a `RecoveryKickoff`. Only `Activated` installed the candidate;
/// every other outcome left the actor state and finished signal untouched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryKickoffResult {
    /// The slot was empty and now holds the candidate, which the caller must drive.
    Activated,
    /// The live occupant has the same message id and the same present nonce.
    AlreadyActiveSameEpisode,
    /// The live occupant is another episode, or its identity cannot be proven equal.
    OccupiedDifferentEpisode,
    /// The occupant is cancelled but has not released the slot yet.
    OccupiedCancelled,
    /// Refused by a purge tombstone (`state.closed`).
    RefusedClosed,
    /// Recovery admission refused or the actor was unreachable.
    Unavailable,
}

impl RecoveryKickoffResult {
    pub(crate) fn activated_turn(self) -> bool {
        self == Self::Activated
    }

    pub(crate) fn refused_closed(self) -> bool {
        self == Self::RefusedClosed
    }
}

/// A kickoff claims only an empty slot; an occupied one, including its finished
/// signal, is never rebound.
pub(super) fn kickoff_refusal(
    state: &ChannelMailboxState,
    candidate: &CancelToken,
    user_message_id: Option<MessageId>,
) -> Option<RecoveryKickoffResult> {
    let occupant = state.cancel_token.as_deref()?;
    Some(occupied_kickoff_outcome(
        state,
        occupant,
        candidate,
        user_message_id,
    ))
}

/// Classifies a kickoff against an occupied slot. Same episode needs an exact,
/// present nonce, so `None == None` never proves identity (id-0 turns included).
fn occupied_kickoff_outcome(
    state: &ChannelMailboxState,
    occupant: &CancelToken,
    candidate: &CancelToken,
    user_message_id: Option<MessageId>,
) -> RecoveryKickoffResult {
    if occupant.cancelled.load(Ordering::Relaxed) {
        return RecoveryKickoffResult::OccupiedCancelled;
    }
    let same_nonce = candidate
        .turn_nonce()
        .is_some_and(|nonce| state.active_turn_nonce.as_deref() == Some(nonce));
    if same_nonce && state.active_user_message_id == user_message_id {
        RecoveryKickoffResult::AlreadyActiveSameEpisode
    } else {
        RecoveryKickoffResult::OccupiedDifferentEpisode
    }
}

/// Runs only on the empty-slot `Activated` transition, before `recovery_started_at`
/// is observable, so a refusal never clears a live recovery's `recovery_done` latch.
pub(super) fn reset_activation_signals(channel_id: ChannelId) {
    reset_turn_finished_signal(channel_id);
    if let Some(recovery_done) = ChannelMailboxRegistry::global_recovery_done(channel_id) {
        recovery_done.reset();
    }
}
