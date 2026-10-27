//! Checks and reporting only the queued-head owner needs: whether the pane it resolved is still
//! the one it would type into, and how its finished attempt reads to an operator.
use std::sync::Arc;

use super::queued_provider::QueuedProvider;
use super::{
    HumanInputRequest, InjectAttempt, Target, binding_key, busy_turn, channel_name, herdr,
    live_tui_session, observe, row_key, turn_mode_tmux,
};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::SharedData;

/// The head was left Unknown by an earlier attempt; nothing types it again.
pub(super) const HEAD_UNKNOWN: &str = "queued_head_unknown";
/// The Unknown head was consumed on transcript proof alone; nothing was typed for it.
pub(super) const RECONCILED: &str = "queued_head_reconciled";
/// The paste could not be named in its transcript before it started, so it was not typed.
pub(super) const ATTEMPT_UNNAMED: &str = "queued_paste_unnamed";

pub(super) async fn queued_target_matches(
    shared: &SharedData,
    request: &HumanInputRequest,
    target: &Target,
    before_effect: bool,
) -> bool {
    let Some(provider) = QueuedProvider::select(&request.provider, request.channel_id.get()) else {
        return false;
    };
    let runtime = provider.runtime();
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
        && live_tui_session(shared, request, runtime, row.as_ref(), named)
            == Some((target.session.clone(), target.transcript.clone()))
        && (!before_effect
            || (runtime == RuntimeHandoffKind::ClaudeTui
                && target.herdr.is_none()
                && turn_mode_tmux(request.channel_id.get()))
            || busy_turn(runtime, &target.transcript).is_ok())
        && same_herdr_execution(shared, request, target).await
}

/// A Herdr target is still the channel's Bound execution: the same endpoint, pane and nonce.
async fn same_herdr_execution(
    shared: &SharedData,
    request: &HumanInputRequest,
    target: &Target,
) -> bool {
    let Some(pane) = &target.herdr else {
        return true;
    };
    let channel = request.channel_id.get();
    let now = herdr::bound_pane(shared, &request.provider, channel, &target.session).await;
    now.as_ref() == Some(pane)
}

impl InjectAttempt {
    /// `(outcome, reason, head_state)` of a finished attempt: `terminal`, or `queued` for a
    /// handed-back NotSent, only where the actor committed it; otherwise unknown or unavailable.
    pub(crate) fn queued_report(&self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Injected { .. } => ("injected", "", "terminal"),
            Self::NotSent(RECONCILED) => ("reconciled", RECONCILED, "terminal"),
            Self::NotSent(veto) => ("not_sent", veto, "unavailable"),
            Self::HandbackFailed(HEAD_UNKNOWN) => ("blocked", HEAD_UNKNOWN, "unknown"),
            Self::HandbackFailed(detail) => ("blocked", detail, "unavailable"),
            Self::Unconfirmed { detail, .. } => ("unconfirmed", detail, "unknown"),
            Self::HandedBack { veto, .. } => ("not_sent", veto, "queued"),
            Self::OwnerFailed { .. } => ("unconfirmed", "owner_failed", "unavailable"),
        }
    }
}
