use std::sync::atomic::Ordering;

use poise::serenity_prelude::MessageId;

use super::ChannelMailboxState;
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

/// Classifies a kickoff against an occupied slot. Same episode needs an exact,
/// present nonce, so `None == None` never proves identity (id-0 turns included).
pub(super) fn occupied_kickoff_outcome(
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
