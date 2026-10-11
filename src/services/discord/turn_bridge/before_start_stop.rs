//! In-process policy close of a sealed before-start proof: a durable receipt first, then the stop
//! notice and its ack, then finalize. A crash after the receipt leaves restart to finish the close.
use crate::services::discord::SharedData;
use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::discord::gateway::TurnGateway;
use crate::services::discord::inflight::InflightTurnState;
#[cfg(unix)]
use crate::services::discord::recovery_engine::herdr_admitted_restart::ADMITTED_ABORT_NOTICE;
use crate::services::discord::turn_finalizer::{
    FinalizeContext, FinalizeOutcome, SyntheticClaimSnapshot, TerminalEvent, TurnFinalizer, TurnKey,
};
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
    let notice =
        gateway.replace_message_with_outcome(channel, current_msg_id, ADMITTED_ABORT_NOTICE);
    let fin = &shared.turn_finalizer;
    let settled = consume(shared, fin, actor, row, (proof, current_msg_id), notice).await;
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
    (proof, anchor): (&BeforeStartProof, MessageId),
    notice: impl std::future::Future<Output = Result<ReplaceLongMessageOutcome, String>>,
) -> bool {
    let Some(state) = actor.herdr_interrupt_state() else {
        return false;
    };
    if proof.owner != state.owner
        || proof.owner.discord_token_hash != shared.token_hash
        || proof.owner.provider != row.provider
        || proof.owner.channel_id != row.channel_id.to_string()
        || row.tmux_session_name.as_deref() != Some(&proof.owner.logical_key)
        || row.turn_nonce.as_deref() != Some(&proof.turn_nonce)
        || actor.turn_nonce() != Some(&proof.turn_nonce)
        || actor.claude_interrupt_generation() != proof.generation
        || state.closed_probe() != Some(true)
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
    #[cfg(test)]
    test_hooks::boundary("before_receipt", row);
    // The receipt is durable before anything is shown or released; a refusal keeps the hold.
    let generation = shared.restart.current_generation;
    let Some(admitted) = row
        .admit_before_start_receipt(&proof.turn_nonce, proof.basis, generation)
        .await
    else {
        return false;
    };
    #[cfg(test)]
    test_hooks::boundary("after_receipt", &admitted);
    // The stop shows on the durable row's message and only where O does not own the body.
    if admitted.current_msg_id != anchor.get()
        || crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
            admitted.channel_id,
            admitted.runtime_kind,
        )
        .unwrap_or(true)
    {
        return false;
    }
    let sent = notice.await;
    if admitted.ack_before_start_notice(&sent).await.is_none() {
        return false;
    }
    #[cfg(test)]
    test_hooks::boundary("after_notice_ack", &admitted);
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
    #[cfg(test)]
    test_hooks::boundary("before_finalize", &admitted);
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

/// Test seams at each step of `consume`: a per-thread observer, a terminal-submit count, and a
/// child-process stop named by `STOP_AT`.
#[cfg(test)]
pub(in crate::services::discord) mod test_hooks {
    use crate::services::discord::inflight::InflightTurnState;
    use std::cell::{Cell, RefCell};

    type Observer = Box<dyn FnMut(&'static str, &InflightTurnState)>;

    thread_local! {
        static OBSERVER: RefCell<Option<Observer>> = const { RefCell::new(None) };
        static SUBMITTED: Cell<usize> = const { Cell::new(0) };
    }

    pub(in crate::services::discord) const STOP_AT: &str = "ADK_COLDSTOP_B_STOP_AT";

    #[cfg(unix)]
    pub(in crate::services::discord) fn observe(observer: Option<Observer>) {
        OBSERVER.with(|slot| *slot.borrow_mut() = observer);
    }

    /// Terminal submissions `consume` made on this thread.
    #[cfg(unix)]
    pub(in crate::services::discord) fn submitted() -> usize {
        SUBMITTED.with(Cell::get)
    }

    pub(super) fn boundary(step: &'static str, row: &InflightTurnState) {
        if step == "before_finalize" {
            SUBMITTED.with(|count| count.set(count.get() + 1));
        }
        if std::env::var(STOP_AT).ok().as_deref() == Some(step) {
            println!(
                "COLDSTOP_B_STOP {step} channel={} pid={}",
                row.channel_id,
                std::process::id()
            );
            std::process::exit(0);
        }
        OBSERVER.with(|slot| {
            if let Some(observer) = slot.borrow_mut().as_mut() {
                observer(step, row);
            }
        });
    }
}
