//! A Herdr user stop on the channel mailbox: the first stop records its intent on the current
//! token and never cancels it, so only the provider's terminal record ends the turn.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::{CancelActiveTurnResult, ChannelMailboxHandle, ChannelMailboxMsg, StopCancel};
use crate::services::provider::CancelToken;
use crate::services::provider::cancel_token_claude_interrupt::herdr_stop_settlement_available;

impl ChannelMailboxHandle {
    /// Records a Herdr user-stop intent on `expected_token` while it is the channel's turn, never
    /// cancelling it; `already_stopping` reports an earlier intent or an already cancelled token.
    pub(crate) async fn admit_herdr_user_stop_if_current(
        &self,
        expected_token: Arc<CancelToken>,
        reason: String,
    ) -> CancelActiveTurnResult {
        self.request(
            |reply| ChannelMailboxMsg::CancelActiveTurnIfCurrentWithReason {
                expected_token,
                reason,
                herdr_user_stop: true,
                reply,
            },
        )
        .await
        .unwrap_or(CancelActiveTurnResult {
            token: None,
            already_stopping: false,
        })
    }

    /// A user stop's guarded cancel, decided once under the token's Herdr slot and returned as
    /// decided, so a turn whose Herdr state came first keeps its token for the intent path.
    pub(crate) async fn cancel_active_turn_if_current_unless_herdr(
        &self,
        expected_token: Arc<CancelToken>,
        reason: String,
    ) -> StopCancel {
        self.request(
            |reply| ChannelMailboxMsg::CancelActiveTurnIfCurrentUnlessHerdr {
                expected_token,
                reason,
                reply,
            },
        )
        .await
        .unwrap_or(StopCancel::NotCurrent)
    }
}

/// Records the first Herdr user-stop intent on the current token and never cancels it, so only
/// the provider's terminal record ends the turn; without settlement or Herdr state, nothing.
pub(super) fn herdr_user_stop_intent(
    token: Option<Arc<CancelToken>>,
    reason: &str,
) -> CancelActiveTurnResult {
    let herdr = token
        .filter(|_| herdr_stop_settlement_available())
        .and_then(|token| Some((token.herdr_interrupt_state()?, token)));
    let Some((intent, token)) = herdr else {
        return CancelActiveTurnResult {
            token: None,
            already_stopping: false,
        };
    };
    let already_stopping =
        token.cancelled.load(Ordering::Acquire) || intent.user_stop.swap(true, Ordering::AcqRel);
    if !already_stopping {
        tracing::info!(reason, "herdr user stop intent recorded");
    }
    CancelActiveTurnResult {
        token: Some(token),
        already_stopping,
    }
}
