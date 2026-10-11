//! In-process policy close of a sealed before-start proof. A durable receipt and restart
//! recovery are not present yet, so a crash between admission and finalize stays held.
use crate::services::discord::gateway::TurnGateway;
use crate::services::discord::inflight::{InflightTurnIdentity, InflightTurnState};
#[cfg(unix)]
use crate::services::discord::recovery_engine::herdr_admitted_restart::ADMITTED_ABORT_NOTICE;
use crate::services::discord::turn_finalizer::{
    FinalizeContext, FinalizeOutcome, SyntheticClaimSnapshot, TerminalEvent, TurnFinalizer, TurnKey,
};
use crate::services::discord::{self, SharedData};
use crate::services::provider::CancelToken;
use crate::services::provider::herdr_before_start::{BeforeStartProof, ExitDecision};
use poise::serenity_prelude::{ChannelId, MessageId};
use std::sync::Arc;

// The restart module is Unix only; elsewhere the same text keeps the bridge building, though
// settlement there is never available, so no policy close reaches it.
#[cfg(not(unix))]
const ADMITTED_ABORT_NOTICE: &str = "⏹ 중단됨";

/// The bridge exit; true keeps the turn held. A held row is never sealed or closed by policy.
pub(super) async fn exit_held(
    shared: &Arc<SharedData>,
    gateway: &dyn TurnGateway,
    actor: &Arc<CancelToken>,
    row: &InflightTurnState,
    current_msg_id: MessageId,
    (cancelled, terminal_admitted, held): (bool, bool, bool),
) -> bool {
    if held {
        return true;
    }
    let exit = super::stream_loop::exit_reconcile::herdr_exit_decision(
        actor,
        cancelled,
        terminal_admitted,
    );
    if let ExitDecision::PolicyClose(proof) = &exit {
        run_policy_close(shared, gateway, actor, row, current_msg_id, proof).await;
    }
    exit != ExitDecision::Normal
}

/// Settles one sealed policy close; on false the turn stays held as before.
async fn run_policy_close(
    shared: &Arc<SharedData>,
    gateway: &dyn TurnGateway,
    actor: &Arc<CancelToken>,
    row: &InflightTurnState,
    current_msg_id: MessageId,
    proof: &BeforeStartProof,
) -> bool {
    let channel = ChannelId::new(row.channel_id);
    let notice = async {
        gateway
            .replace_message_with_outcome(channel, current_msg_id, ADMITTED_ABORT_NOTICE)
            .await
            .map(|_| ())
    };
    let settled = consume(shared, &shared.turn_finalizer, actor, row, proof, notice).await;
    tracing::info!(
        event = "herdr_turn_policy_close",
        channel_id = row.channel_id,
        logical = proof.owner.logical_key,
        turn_nonce = proof.turn_nonce,
        runtime_generation = proof.generation,
        settled,
        "Herdr turn closed before its input started"
    );
    settled
}

async fn consume(
    shared: &Arc<SharedData>,
    fin: &TurnFinalizer,
    actor: &Arc<CancelToken>,
    row: &InflightTurnState,
    proof: &BeforeStartProof,
    notice: impl std::future::Future<Output = Result<(), String>>,
) -> bool {
    let Some(state) = actor.herdr_interrupt_state() else {
        return false;
    };
    let expected = InflightTurnIdentity::from_state(row);
    if proof.owner != state.owner
        || proof.owner.discord_token_hash != shared.token_hash
        || proof.owner.provider != row.provider
        || proof.owner.channel_id != row.channel_id.to_string()
        || row.tmux_session_name.as_deref() != Some(&proof.owner.logical_key)
        || row.turn_nonce.as_deref() != Some(&proof.turn_nonce)
        || actor.turn_nonce() != Some(&proof.turn_nonce)
        || actor.claude_interrupt_generation() != proof.generation
        || state.closed_probe() != Some(true)
        || !discord::inflight::load_inflight_state_read_only(&shared.provider, row.channel_id)
            .is_some_and(|current| {
                expected.matches_state(&current) && current.turn_nonce == row.turn_nonce
            })
    {
        return false;
    }
    let channel = ChannelId::new(row.channel_id);
    let Some(mailbox) = shared.mailbox_peek(channel) else {
        return false;
    };
    if !mailbox
        .snapshot()
        .await
        .cancel_token
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, actor))
    {
        return false;
    }
    if notice.await.is_err() {
        return false;
    }
    let key = TurnKey::new(
        channel,
        row.effective_finalizer_turn_id(),
        shared.restart.current_generation,
    )
    .with_episode_nonce(row.turn_nonce.as_deref());
    let mut snapshot = SyntheticClaimSnapshot::from_row(row);
    snapshot.recovery_actor = Some(Arc::downgrade(actor));
    let context = FinalizeContext {
        clear_inflight: true,
        ..FinalizeContext::bridge()
    };
    matches!(
        fin.submit_terminal_with_claim_snapshot(
            key,
            shared.provider.clone(),
            TerminalEvent::Cancel,
            context,
            Some(snapshot),
            shared.clone()
        )
        .await,
        FinalizeOutcome::Finalized { .. }
    )
}

#[cfg(test)]
#[cfg(unix)]
#[path = "before_start_stop_tests.rs"]
pub(in crate::services::discord::turn_bridge) mod tests;
