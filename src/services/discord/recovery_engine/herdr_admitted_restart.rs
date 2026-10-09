//! Restart settles a prior process's admitted Herdr terminal by its persisted kind, acks it on
//! the row its delivery changed, then clears only that episode; a row it cannot settle stays.

use super::*;
use crate::services::agent_protocol::NativeTerminalKind;

/// What restart shows for an admitted Aborted: the provider stopped this turn, so its transcript
/// result is never relayed as an answer.
pub(in crate::services::discord) const ADMITTED_ABORT_NOTICE: &str = "⏹ 중단됨";

/// The persisted admitted kind of a prior process's Herdr turn, read as restart's terminal.
pub(super) struct AdmittedRestartTerminal {
    provider: ProviderKind,
    channel: ChannelId,
    turn_nonce: String,
    kind: NativeTerminalKind,
    delivered: bool,
}

impl AdmittedRestartTerminal {
    /// Only under settlement, for a nonce-bearing Herdr row a prior process admitted; any other row
    /// keeps the existing recovery path.
    fn from_row(provider: &ProviderKind, state: &inflight::InflightTurnState) -> Option<Self> {
        if !crate::services::provider::cancel_token_claude_interrupt::herdr_stop_settlement_available()
        {
            return None;
        }
        let kind = state.tui_terminal_kind?;
        let turn_nonce = state.turn_nonce.clone().filter(|nonce| !nonce.is_empty())?;
        let current = super::runtime_store::process_generation();
        if inflight::row_is_current_generation(state, current) {
            return None;
        }
        let name = recovery_tmux_session_name(provider, state)?;
        if !crate::services::discord::turn_bridge::herdr_marked(&name) {
            return None;
        }
        Some(Self {
            provider: provider.clone(),
            channel: inflight::opt_channel_id(state.channel_id)?,
            turn_nonce,
            kind,
            delivered: state.terminal_delivery_committed,
        })
    }

    fn kind(&self) -> NativeTerminalKind {
        if mutant("restart_admitted_kind_reclassified") {
            return NativeTerminalKind::Completed;
        }
        self.kind
    }

    /// The text this kind shows. A completion shows only the body its row stored for this turn; a
    /// transcript read to EOF can hold a successor's answer, so none is read here.
    fn delivery(
        &self,
        provider: &ProviderKind,
        state: &inflight::InflightTurnState,
    ) -> Option<String> {
        if self.kind() != NativeTerminalKind::Completed {
            return Some(ADMITTED_ABORT_NOTICE.to_string());
        }
        let stored = if mutant("restart_admitted_body_eof") {
            state
                .output_path
                .as_deref()
                .map(|path| extract_response_from_output(path, state.last_offset))
                .unwrap_or_default()
        } else {
            state.full_response.clone()
        };
        (!stored.trim().is_empty())
            .then(|| super::formatting::format_for_discord_with_provider(&stored, provider))
    }

    /// Delivers by the admitted kind, commits the delivery on the row it changed, then clears
    /// exactly that row. Every refused or unconfirmed step keeps the row for the next boot.
    pub(super) async fn settle(
        self,
        http: &Arc<serenity::Http>,
        shared: &Arc<SharedData>,
        state: &inflight::InflightTurnState,
    ) -> AdmittedRestart {
        let (provider, channel_id) = (&self.provider, self.channel);
        if state.restart_mode.is_some() && !mutant("restart_admitted_restart_mode_settled") {
            return kept(state, "planned restart owns the row");
        }
        // O owns this destination's body whether or not a delivery was recorded; neither a send
        // nor a cleanup is this path's to make there.
        let undelivered_only = mutant("restart_admitted_o_guard_undelivered_only");
        if (!undelivered_only || !self.delivered) && o_owns(state) {
            return kept(state, "O owns the destination body");
        }
        // A prior process's turn has no actor here; one present now is another turn's.
        if !mutant("restart_admitted_mailbox_channel_scoped")
            && super::mailbox_snapshot(shared, channel_id)
                .await
                .cancel_token
                .is_some()
        {
            return kept(state, "a live actor owns the channel");
        }
        let mut row = state.clone();
        if !self.delivered && !mutant("restart_admitted_row_dropped") {
            let Some(text) = self.delivery(provider, &row) else {
                return kept(state, "a completion with no stored body");
            };
            let mut delivery =
                relay_captured_recovery_terminal_notice(http, shared, provider, &row, &text).await;
            // O's body marker is not this terminal's delivery.
            if o_owns(&row) {
                delivery.outcome = RecoveryRelayOutcome::TransientFailure;
            }
            if mutant("restart_admitted_anchor_dropped")
                && let Some(pending) = delivery.pending_anchor.take()
            {
                let _ = pending.bind_after_actor_check(shared, &mut row.clone());
            }
            let commit = CapturedReadyDeliveryCommit {
                shared: shared.clone(),
                state: row,
                actor: None,
                delivery,
            };
            let mailbox = shared.mailbox(channel_id);
            let Some(committed) = mailbox.commit_captured_ready_delivery(commit).await else {
                return kept(state, "the delivery ack was not committed");
            };
            row = committed.state;
            #[cfg(test)]
            test_hooks::stop_after_ack(&row);
        }
        #[cfg(test)]
        test_hooks::before_cleanup().await;
        if mutant("restart_admitted_mailbox_channel_scoped") {
            let source = "recovery_herdr_admitted_terminal";
            finish_recovered_turn_mailbox(shared, provider, channel_id, source).await;
        } else if super::mailbox_snapshot(shared, channel_id)
            .await
            .cancel_token
            .is_some()
        {
            return kept(state, "an actor arrived before cleanup");
        }
        let current = super::runtime_store::process_generation();
        let wildcard = mutant("restart_admitted_clear_wildcard_nonce")
            || mutant("restart_admitted_row_dropped")
            || mutant("restart_admitted_mailbox_channel_scoped");
        let cleared = if wildcard {
            inflight::clear_inflight_state_if_matches_identity_turn_nonce(
                provider,
                row.channel_id,
                &inflight::InflightTurnIdentity::from_state(&row),
                Some(self.turn_nonce.as_str()),
            )
        } else {
            inflight::clear_admitted_restart_terminal(provider, &row, &self.turn_nonce, current)
        };
        if cleared == inflight::GuardedClearOutcome::Cleared {
            AdmittedRestart::Settled
        } else {
            kept(state, "the fresh row is not this delivered episode")
        }
    }
}

/// How restart left an admitted row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AdmittedRestart {
    /// Delivered durably and its exact row cleared.
    Settled,
    /// Kept for the next boot; never counted as settled.
    Retained,
}

fn kept(state: &inflight::InflightTurnState, why: &'static str) -> AdmittedRestart {
    tracing::warn!(
        channel_id = state.channel_id,
        why,
        "recovery kept an admitted Herdr terminal"
    );
    AdmittedRestart::Retained
}

/// Whether O owns this row's destination body; an unreadable answer counts as owned.
fn o_owns(state: &inflight::InflightTurnState) -> bool {
    if mutant("restart_admitted_o_marker_acked") {
        return false;
    }
    crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
        state.channel_id,
        state.runtime_kind,
    )
    .unwrap_or(true)
}

fn mutant(name: &str) -> bool {
    #[cfg(test)]
    {
        crate::services::provider::cancel_token_claude_interrupt::herdr_interrupt_mutant(name)
    }
    #[cfg(not(test))]
    {
        let _ = name;
        false
    }
}

/// The restart terminal of `state`, if it is a prior process's admitted Herdr turn.
pub(super) fn admitted(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
) -> Option<AdmittedRestartTerminal> {
    AdmittedRestartTerminal::from_row(provider, state)
}

#[cfg(test)]
pub(in crate::services::discord) mod test_hooks {
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;

    type Hook = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

    thread_local! {
        static BEFORE_CLEANUP: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Runs once, after any delivery ack and before the mailbox check and cleanup.
    pub(in crate::services::discord) fn set_before_cleanup(hook: Hook) {
        BEFORE_CLEANUP.with(|slot| *slot.borrow_mut() = Some(hook));
    }

    pub(super) async fn before_cleanup() {
        if let Some(hook) = BEFORE_CLEANUP.with(|slot| slot.borrow_mut().take()) {
            hook().await;
        }
    }

    /// The crash rig's stop: the process ends with the ack durable and the row not yet cleared.
    pub(in crate::services::discord) const STOP_AFTER_ACK: &str = "ADK_RESTART_KIND_STOP_AFTER_ACK";

    pub(super) fn stop_after_ack(row: &crate::services::discord::inflight::InflightTurnState) {
        if std::env::var_os(STOP_AFTER_ACK).is_some() {
            println!(
                "RESTART_KIND_ACK_STOP channel={} pid={}",
                row.channel_id,
                std::process::id()
            );
            std::process::exit(0);
        }
    }
}

#[cfg(test)]
#[path = "herdr_admitted_restart_tests.rs"]
mod tests;
