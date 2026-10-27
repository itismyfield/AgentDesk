//! Restart settles a prior process's admitted Herdr terminal by its persisted kind, acks it on
//! the row its delivery changed (O keeps a body it owns), then clears only that episode.

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
    /// exactly that row; O's body is left to O. A refused or unconfirmed step keeps the row.
    pub(super) async fn settle(
        self,
        http: &Arc<serenity::Http>,
        shared: &Arc<SharedData>,
        state: &inflight::InflightTurnState,
    ) -> AdmittedRestart {
        match self.settle_checked(http, shared, state).await {
            AdmittedRestartReport::Settled => AdmittedRestart::Settled,
            AdmittedRestartReport::Retained(reason) => {
                debug_assert!(!reason.is_empty());
                AdmittedRestart::Retained
            }
        }
    }

    pub(super) async fn settle_checked(
        self,
        http: &Arc<serenity::Http>,
        shared: &Arc<SharedData>,
        state: &inflight::InflightTurnState,
    ) -> AdmittedRestartReport {
        let (provider, channel_id) = (&self.provider, self.channel);
        // A restart that kept the prior process's actor would leave this settlement unmade.
        if mutant("restart_in_process") {
            return kept(state, "a live actor owns the channel");
        }
        // A hot swap may still overlap the prior process; a drain restart left the row to settle.
        let hot_swap = matches!(
            state.restart_mode,
            Some(crate::services::discord::InflightRestartMode::HotSwapHandoff)
        );
        if hot_swap && !mutant("restart_admitted_restart_mode_settled") {
            return kept(state, "a hot-swap handoff owns the row");
        }
        // Where O owns the body its frontier keeps that body; an unreadable answer keeps the row.
        let Some(o_body) = o_owns(state) else {
            return kept(state, "O ownership is unreadable");
        };
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
        // A drain restart left the row to this process: take that authority from the locked row
        // only while it is still this episode, and carry on with the row that write stored.
        if row.restart_mode.is_some() {
            let (snapshot, provider) = (row.clone(), provider.clone());
            let current = super::runtime_store::process_generation();
            let take = move || snapshot.take_drain_restart(&provider, current);
            let Ok(Some(taken)) = tokio::task::spawn_blocking(take).await else {
                return kept(state, "the drain restart's row was not taken");
            };
            row = taken;
        }
        if o_body {
            super::super::outbound::completed_turn_ledger::o_pending::capture(provider, &row);
            // Nothing is sent for O's body; an Aborted still shows its stop as a reply.
            let reply = crate::services::discord::turn_bridge::before_start_stop::send_stop_reply;
            if self.kind() == NativeTerminalKind::Aborted
                && !self.delivered
                && !matches!(reply(http, shared, &row).await, Ok(Some(_)))
            {
                return kept(state, "the stop reply was not shown");
            }
        } else if !self.delivered && !mutant("restart_admitted_row_dropped") {
            let Some(text) = self.delivery(provider, &row) else {
                return kept(state, "a completion with no stored body");
            };
            let mut delivery =
                relay_captured_recovery_terminal_notice(http, shared, provider, &row, &text).await;
            // O's body marker is not this terminal's delivery.
            if o_owns(&row) != Some(false) {
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
            let nonce = &self.turn_nonce;
            inflight::clear_admitted_restart_terminal(provider, &row, nonce, current, o_body)
        };
        if cleared == inflight::GuardedClearOutcome::Cleared {
            AdmittedRestartReport::Settled
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

/// Settlement evidence, never an installation acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AdmittedRestartReport {
    Settled,
    Retained(&'static str),
}

fn kept(state: &inflight::InflightTurnState, why: &'static str) -> AdmittedRestartReport {
    tracing::warn!(
        channel_id = state.channel_id,
        why,
        "recovery kept an admitted Herdr terminal"
    );
    AdmittedRestartReport::Retained(why)
}

/// Whether O owns this row's destination body; `None` while that cannot be read.
fn o_owns(state: &inflight::InflightTurnState) -> Option<bool> {
    if mutant("restart_admitted_o_marker_acked") {
        return Some(false);
    }
    crate::services::discord::turn_bridge::before_start_stop::o_owns_body(state)
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

    type Kind = Option<crate::services::agent_protocol::NativeTerminalKind>;
    type PersistStop = Box<dyn FnOnce(Kind) + Send>;
    static AFTER_ADMITTED_PERSIST: std::sync::Mutex<Vec<(u64, PersistStop)>> =
        std::sync::Mutex::new(Vec::new());

    /// The crash rig's stop right after an admission made `channel`'s terminal kind durable.
    pub(in crate::services::discord) fn stop_after_admitted_persist(
        channel: u64,
        stop: PersistStop,
    ) {
        let mut stops = AFTER_ADMITTED_PERSIST
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        stops.push((channel, stop));
    }

    pub(in crate::services::discord) fn after_admitted_persist(channel: u64, kind: Kind) {
        let stop = {
            let mut stops = AFTER_ADMITTED_PERSIST
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let at = stops.iter().position(|(owner, _)| *owner == channel);
            at.map(|at| stops.remove(at).1)
        };
        if let Some(stop) = stop {
            stop(kind);
        }
    }

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
