//! Restart-path helpers (issue #1074 landing zone; first occupant: #3293).
//!
//! Hosts the side-effecting epilogue for the boot-time recovery branches in
//! `recovery_engine::restore_inflight_turns` whose terminal relay to Discord
//! did NOT deliver. The pure decision matrix lives in [`super::shared`]; this
//! module executes the chosen [`RowDisposition`] with the no-silent-delete
//! guarantees from the #3293 design (handoff + audit + structured WARN on
//! every force-clear, identity-guarded counter persistence on preserve).

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::super::recovery_engine::{finish_recovered_turn_mailbox, save_missing_session_handoff};
use super::super::{SharedData, inflight};
use super::shared::{
    RecoveryRelayOutcome, RowDisposition, disposition_reason_code, unrecoverable_relay_disposition,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxRegistry;

/// #3293 (c): finish the recovered turn's mailbox ONLY when a registry entry
/// already exists. `finish_recovered_turn_mailbox` routes through the turn
/// finalizer, whose channel-scoped resolution mints a mailbox actor on first
/// touch — on a force-clear of a row for a non-existent (bogus) channel that
/// would re-pollute the registry with a permanent entry. Peek, never create.
pub(in crate::services::discord) async fn finish_recovered_turn_mailbox_if_registered(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    stop_source: &'static str,
) {
    let registered = shared.mailbox_peek(channel_id).is_some()
        || ChannelMailboxRegistry::global_handle(channel_id).is_some();
    if !registered {
        tracing::debug!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            stop_source,
            "recovery force-clear: no mailbox registry entry — skipping finish to avoid creating one"
        );
        return;
    }
    finish_recovered_turn_mailbox(shared, provider, channel_id, stop_source).await;
}

/// #3293: shared epilogue for the five recovery notice branches. Computes the
/// [`RowDisposition`] from the relay `outcome` and executes it:
///
/// * `FinishAndClear` (delivered) — the branch's historical epilogue:
///   `finish_recovered_turn_mailbox(finish_stop_source)` + clear.
/// * everything else — [`apply_undeliverable_relay_disposition`].
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn dispose_recovery_relay_outcome(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    outcome: RecoveryRelayOutcome,
    tmux_alive: bool,
    finish_stop_source: &'static str,
    branch: &'static str,
    best_response: &str,
    handoff_already_saved: bool,
) {
    match unrecoverable_relay_disposition(
        outcome,
        state.recovery_relay_attempts,
        inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
        tmux_alive,
    ) {
        RowDisposition::FinishAndClear => {
            finish_recovered_turn_mailbox(
                shared,
                provider,
                ChannelId::new(state.channel_id),
                finish_stop_source,
            )
            .await;
            inflight::clear_inflight_state(provider, state.channel_id);
        }
        disposition => {
            apply_undeliverable_relay_disposition(
                shared,
                provider,
                state,
                disposition,
                branch,
                tmux_alive,
                best_response,
                handoff_already_saved,
            )
            .await;
        }
    }
}

/// Execute a non-`Delivered` [`RowDisposition`] for a recovery branch.
///
/// * `ClearPermanent` / `ClearBudgetExhausted` — termination audit (when the
///   row carries a `session_key`), missing-session handoff (unless the branch
///   already saved one), a structured WARN that ALWAYS fires, then finish the
///   mailbox (existing entries only) and clear the inflight row.
/// * `PreserveAndCount` — persist `recovery_relay_attempts + 1` through the
///   identity-guarded save (never clobbers a newer turn / restart marker) and
///   WARN with the attempt budget so the loop is observable.
/// * `FinishAndClear` is the caller's delivered epilogue — a no-op here.
#[allow(clippy::too_many_arguments)]
async fn apply_undeliverable_relay_disposition(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    disposition: RowDisposition,
    branch: &'static str,
    tmux_alive: bool,
    best_response: &str,
    handoff_already_saved: bool,
) {
    match disposition {
        RowDisposition::FinishAndClear => {}
        RowDisposition::ClearPermanent | RowDisposition::ClearBudgetExhausted => {
            let reason_code = disposition_reason_code(disposition)
                .expect("clearing dispositions always carry a reason code");
            if let Some(ref session_key) = state.session_key {
                crate::services::termination_audit::record_termination_with_handles(
                    None::<&crate::db::Db>,
                    shared.pg_pool.as_ref(),
                    session_key,
                    state.dispatch_id.as_deref(),
                    "recovery",
                    reason_code,
                    Some("recovery terminal relay unrecoverable; inflight force-cleared"),
                    None,
                    Some(state.last_offset),
                    Some(tmux_alive),
                );
            }
            if !handoff_already_saved {
                save_missing_session_handoff(provider, state, best_response);
            }
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                user_msg_id = state.user_msg_id,
                branch,
                reason_code,
                attempts = state.recovery_relay_attempts,
                budget = inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
                "recovery relay unrecoverable — force-clearing inflight row"
            );
            finish_recovered_turn_mailbox_if_registered(
                shared,
                provider,
                ChannelId::new(state.channel_id),
                reason_code,
            )
            .await;
            inflight::clear_inflight_state(provider, state.channel_id);
        }
        RowDisposition::PreserveAndCount => {
            let mut updated = state.clone();
            updated.recovery_relay_attempts = state.recovery_relay_attempts.saturating_add(1);
            let identity = inflight::InflightTurnIdentity::from_state(state);
            let save_outcome = inflight::save_inflight_state_if_matches_identity(
                &updated,
                &identity,
                state.turn_start_offset,
            );
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                branch,
                attempts = updated.recovery_relay_attempts,
                budget = inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET,
                counter_persisted = matches!(save_outcome, inflight::GuardedSaveOutcome::Saved),
                "recovery relay failed — preserving inflight for retry on next restart"
            );
        }
    }
}
