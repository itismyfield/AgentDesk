//! Restart settles a prior process's before-start receipt: one stop notice (a reply where O owns
//! the body), ack, then clear. An unreadable or foreign receipt keeps the row off other recovery.

use super::herdr_admitted_restart::ADMITTED_ABORT_NOTICE;
use super::*;
use crate::services::discord::gateway::{DiscordGateway, TurnGateway};

/// Receipt rows stay outside ordinary replay recovery, even when settlement retains them.
pub(super) async fn settle_receipt(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
) -> bool {
    if inflight::receipt_mutant("all-early")
        || (state.herdr_before_start_stop.is_none()
            && !inflight::before_start_receipt_present(provider, state.channel_id))
    {
        return false;
    }
    #[cfg(test)]
    test_hooks::note_early_hold();
    if let Some(receipt) = decode(shared, provider, state) {
        receipt.settle(http, shared, state).await;
    }
    true
}

pub(super) fn herdr_turn_held(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
) -> bool {
    #[cfg(test)]
    if crate::services::provider::cancel_token_claude_interrupt::herdr_interrupt_mutant(
        "restart_none_as_complete",
    ) {
        return false;
    }
    use crate::services::provider::cancel_token_claude_interrupt as interrupt;
    let nonce = state.turn_nonce.as_deref().unwrap_or_default();
    interrupt::herdr_stop_settlement_available()
        && state.tui_terminal_kind.is_none()
        // Only a turn this process indexed is held; a prior process's row takes ordinary recovery.
        && recovery_tmux_session_name(provider, state).is_some_and(|name| {
            interrupt::herdr_turn_indexed(&name, nonce)
                && crate::services::discord::turn_bridge::herdr_marked(&name)
        })
}

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

/// Only a receipt-free row takes the ordinary recovery path.
pub(super) fn decode(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
) -> Option<BeforeStartRestart> {
    let receipt = state.herdr_before_start_stop.as_ref()?;
    if !crate::services::provider::cancel_token_claude_interrupt::herdr_stop_settlement_available()
    {
        if inflight::receipt_mutant("off-decode") {
            return None;
        }
        return Some(BeforeStartRestart::Held("policy settlement is unavailable"));
    }
    let current = shared.restart.current_generation;
    let admitted = receipt.admitted_generation();
    let held = if !receipt.supported() {
        "a receipt this binary cannot read"
    } else if state.rebind_origin {
        "a rebind origin owns the same row"
    } else if state.replay_rerun_blocked() {
        "replay authority owns the same row"
    } else if state.tui_terminal_kind.is_some() {
        "a native terminal was admitted on the same row"
    } else if state.turn_nonce.as_deref() != Some(receipt.turn_nonce()) {
        "the receipt names another turn"
    } else if admitted == 0 {
        "a receipt without its generation"
    } else if admitted >= current && !inflight::receipt_mutant("same-generation") {
        "this process's own close owns the receipt"
    } else if inflight::row_is_current_generation(state, current)
        && !inflight::receipt_mutant("same-generation")
    {
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
        if state.restart_mode.is_some() && !inflight::receipt_mutant("mode-bypass") {
            return kept(state, "planned restart owns the row");
        }
        let Some(o_body) =
            crate::services::discord::turn_bridge::before_start_stop::o_owns_body(state)
        else {
            return kept(state, "O ownership is unreadable");
        };
        // A prior process's turn has no actor here; one present now is another turn's.
        if super::mailbox_snapshot(shared, channel)
            .await
            .cancel_token
            .is_some()
            && !inflight::receipt_mutant("actor-present")
        {
            return kept(state, "a live actor owns the channel");
        }
        let mut row = state.clone();
        // A pending or delivered receipt without its message is never settled.
        if row.current_msg_id == 0 {
            return kept(state, "no message to show the stop on");
        }
        if row
            .herdr_before_start_stop
            .as_ref()
            .is_some_and(|receipt| receipt.notice_pending())
            || inflight::receipt_mutant("delivered-resend")
        {
            // Where O owns the body the stop is a reply to the turn's message, never an edit of it.
            let acked = if o_body {
                let notice =
                    crate::services::discord::turn_bridge::before_start_stop::reply_and_ack_stop;
                notice(http, shared, &row).await
            } else {
                let gateway = DiscordGateway::new(http.clone(), shared.clone(), provider, None);
                let anchor = MessageId::new(row.current_msg_id);
                let sent = gateway
                    .replace_message_with_outcome(channel, anchor, ADMITTED_ABORT_NOTICE)
                    .await;
                row.ack_before_start_notice(&sent).await
            };
            let Some(acked) = acked else {
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
            && !inflight::receipt_mutant("actor-present")
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

#[cfg(test)]
pub(super) mod test_hooks {
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::pin::Pin;

    type Hook = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

    thread_local! {
        static BEFORE_CLEANUP: RefCell<Option<Hook>> = const { RefCell::new(None) };
        static EARLY_HOLDS: Cell<usize> = const { Cell::new(0) };
    }

    /// Counts receipt rows the scan's first check kept off every later path.
    pub(super) fn note_early_hold() {
        EARLY_HOLDS.set(EARLY_HOLDS.get() + 1);
    }

    pub(in crate::services::discord::recovery_engine) fn early_holds() -> usize {
        EARLY_HOLDS.replace(0)
    }

    /// Runs once, after any notice ack and before the mailbox check and cleanup.
    pub(in crate::services::discord::recovery_engine) fn set_before_cleanup(hook: Hook) {
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
