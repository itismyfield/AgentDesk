//! Restart settles a prior process's before-start receipt: one stop notice on the turn's message,
//! ack, then clear of that row. An unreadable or foreign receipt keeps the row off other recovery.

use super::herdr_admitted_restart::ADMITTED_ABORT_NOTICE;
use super::*;
use crate::services::discord::gateway::{DiscordGateway, TurnGateway};

/// A row that carries a policy receipt; a row without one never gets here.
pub(super) enum BeforeStartRestart {
    /// A readable receipt of a prior process's turn: notice if pending, ack, clear.
    Settle(ProviderKind, ChannelId),
    /// Kept as is for a later boot.
    Held(&'static str),
}

/// How restart left a receipt row; only `Settled` means its row is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BeforeStartRestartReport {
    Settled,
    Retained(&'static str),
}

/// `None` keeps the existing recovery path: settlement is off or the row has no receipt.
pub(super) fn decode(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
) -> Option<BeforeStartRestart> {
    if !crate::services::provider::cancel_token_claude_interrupt::herdr_stop_settlement_available()
    {
        return None;
    }
    let receipt = state.herdr_before_start_stop.as_ref()?;
    let current = shared.restart.current_generation;
    let admitted = receipt.admitted_generation();
    let held = if !receipt.supported() {
        "a receipt this binary cannot read"
    } else if state.tui_terminal_kind.is_some() {
        "a native terminal was admitted on the same row"
    } else if state.turn_nonce.as_deref() != Some(receipt.turn_nonce()) {
        "the receipt names another turn"
    } else if admitted == 0 {
        "a receipt without its generation"
    } else if admitted >= current {
        "this process's own close owns the receipt"
    } else if inflight::row_is_current_generation(state, current) {
        "the row was born in this process"
    } else {
        return Some(match inflight::opt_channel_id(state.channel_id) {
            Some(channel) => BeforeStartRestart::Settle(provider.clone(), channel),
            None => BeforeStartRestart::Held("the row has no channel"),
        });
    };
    Some(BeforeStartRestart::Held(held))
}

impl BeforeStartRestart {
    /// Every refused or unconfirmed step keeps the row for the next boot.
    pub(super) async fn settle(
        self,
        http: &Arc<serenity::Http>,
        shared: &Arc<SharedData>,
        state: &inflight::InflightTurnState,
    ) -> BeforeStartRestartReport {
        let (provider, channel) = match self {
            Self::Held(why) => return kept(state, why),
            Self::Settle(provider, channel) => (provider, channel),
        };
        if state.restart_mode.is_some() {
            return kept(state, "planned restart owns the row");
        }
        if o_owns(state) {
            return kept(state, "O owns the destination body");
        }
        // A prior process's turn has no actor here; one present now is another turn's.
        if super::mailbox_snapshot(shared, channel)
            .await
            .cancel_token
            .is_some()
        {
            return kept(state, "a live actor owns the channel");
        }
        let mut row = state.clone();
        if row
            .herdr_before_start_stop
            .as_ref()
            .is_some_and(|receipt| receipt.notice_pending())
        {
            if row.current_msg_id == 0 {
                return kept(state, "no message to show the stop on");
            }
            let gateway = DiscordGateway::new(http.clone(), shared.clone(), provider, None);
            let anchor = MessageId::new(row.current_msg_id);
            let sent = gateway
                .replace_message_with_outcome(channel, anchor, ADMITTED_ABORT_NOTICE)
                .await;
            let Some(acked) = row.ack_before_start_notice(&sent).await else {
                return kept(state, "the notice was not shown and acked");
            };
            row = acked;
        }
        #[cfg(test)]
        test_hooks::before_cleanup().await;
        if super::mailbox_snapshot(shared, channel)
            .await
            .cancel_token
            .is_some()
        {
            return kept(state, "an actor arrived before cleanup");
        }
        let current = shared.restart.current_generation;
        let cleared =
            tokio::task::spawn_blocking(move || row.clear_before_start_receipt(current)).await;
        if matches!(cleared, Ok(inflight::GuardedClearOutcome::Cleared)) {
            BeforeStartRestartReport::Settled
        } else {
            kept(state, "the fresh row is not this delivered receipt")
        }
    }
}

fn kept(state: &inflight::InflightTurnState, why: &'static str) -> BeforeStartRestartReport {
    tracing::warn!(
        channel_id = state.channel_id,
        why,
        "recovery kept a before-start policy receipt"
    );
    BeforeStartRestartReport::Retained(why)
}

/// Whether O owns this row's destination body; an unreadable answer counts as owned.
fn o_owns(state: &inflight::InflightTurnState) -> bool {
    crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
        state.channel_id,
        state.runtime_kind,
    )
    .unwrap_or(true)
}

#[cfg(test)]
mod test_hooks {
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;

    type Hook = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

    thread_local! {
        static BEFORE_CLEANUP: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Runs once, after any notice ack and before the mailbox check and cleanup.
    pub(super) fn set_before_cleanup(hook: Hook) {
        BEFORE_CLEANUP.with(|slot| *slot.borrow_mut() = Some(hook));
    }

    pub(super) async fn before_cleanup() {
        if let Some(hook) = BEFORE_CLEANUP.with(|slot| slot.borrow_mut().take()) {
            hook().await;
        }
    }
}

#[cfg(all(test, unix))]
#[path = "herdr_before_start_restart_tests.rs"]
mod tests;
