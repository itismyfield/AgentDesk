//! Checks and reporting only the queued-head owner needs: whether the pane it resolved is still
//! the one it would type into, and how its finished attempt reads to an operator.
use std::sync::Arc;

use super::{
    HumanInputRequest, InjectAttempt, Target, binding_key, busy_turn, channel_name,
    live_tui_session, observe, row_key,
};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::SharedData;

/// The head was left Unknown by an earlier attempt; nothing types it again.
pub(super) const HEAD_UNKNOWN: &str = "queued_head_unknown";

pub(super) async fn queued_target_matches(
    shared: &SharedData,
    request: &HumanInputRequest,
    target: &Target,
    before_effect: bool,
) -> bool {
    let named = channel_name(shared, request.channel_id).await;
    let (snapshot, row) = observe(shared, request).await;
    let claim = match (&snapshot.cancel_token, &target.claim) {
        (None, None) => true,
        (Some(token), Some((expected, kind, message))) => {
            Arc::ptr_eq(token, expected)
                && snapshot.active_turn_kind == *kind
                && snapshot.active_user_message_id == *message
        }
        _ => false,
    };
    claim
        && row.as_ref().map(row_key) == target.row
        && binding_key(&target.session) == target.binding
        && live_tui_session(
            shared,
            request,
            RuntimeHandoffKind::ClaudeTui,
            row.as_ref(),
            named,
        ) == Some((target.session.clone(), target.transcript.clone()))
        && (!before_effect || busy_turn(RuntimeHandoffKind::ClaudeTui, &target.transcript).is_ok())
}

impl InjectAttempt {
    /// `(outcome, reason, head_state)` of a finished queued-head attempt: `terminal` and `queued`
    /// only where the actor committed that state, otherwise `unknown` or `unavailable`. A NotSent
    /// the actor committed comes back as `HandedBack`; a bare one never reached its claim.
    pub(crate) fn queued_report(&self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Injected { .. } => ("injected", "", "terminal"),
            Self::NotSent(veto) => ("not_sent", veto, "unavailable"),
            Self::HandbackFailed(HEAD_UNKNOWN) => ("blocked", HEAD_UNKNOWN, "unknown"),
            Self::HandbackFailed(detail) => ("blocked", detail, "unavailable"),
            Self::Unconfirmed { detail, .. } => ("unconfirmed", detail, "unknown"),
            Self::HandedBack { veto, .. } => ("not_sent", veto, "queued"),
            Self::OwnerFailed { .. } => ("unconfirmed", "owner_failed", "unavailable"),
        }
    }
}
