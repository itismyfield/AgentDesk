//! Settles a sealed before-start stop: persist a receipt, retry its notice and ACK, then submit
//! one policy close. Unconfirmed work stays held for restart.
use crate::services::discord::SharedData;
use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::discord::gateway::TurnGateway;
use crate::services::discord::inflight::InflightTurnState;
#[cfg(unix)]
use crate::services::discord::recovery_engine::herdr_admitted_restart::ADMITTED_ABORT_NOTICE;
use crate::services::discord::turn_finalizer::{
    PolicyCloseIntent, PolicyCloseOutcome, SyntheticClaimSnapshot, TurnFinalizer, TurnKey,
};
use crate::services::provider::CancelToken;
use crate::services::provider::herdr_before_start::{BeforeStartProof, ExitDecision};
use crate::services::turn_orchestrator::ChannelMailboxHandle;
use crate::services::turn_orchestrator::policy_close::{PolicyCloseRequest, PolicyRetain};
use poise::serenity_prelude::{self as serenity, ChannelId, MessageId};
use std::sync::Arc;
use std::time::Duration;

// Brief transport outages can recover while this channel keeps its existing active slot.
const NOTICE_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

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
    let mut attempts = 0;
    let mut last_failure = None;
    let mut previous_sent = None;
    let settled = if let Some(mailbox) = shared.mailbox_peek(channel) {
        loop {
            attempts += 1;
            let outcome = consume(
                shared,
                (&shared.turn_finalizer, &mailbox),
                actor,
                row,
                (proof, current_msg_id),
                &mut previous_sent,
                || {
                    gateway.replace_message_with_outcome(
                        channel,
                        current_msg_id,
                        ADMITTED_ABORT_NOTICE,
                    )
                },
            )
            .await;
            match outcome {
                CloseAttempt::Settled => break true,
                CloseAttempt::Held => break false,
                CloseAttempt::Retry(stage) => last_failure = Some(stage),
            }
            let Some(delay) = NOTICE_RETRY_DELAYS.get(attempts - 1) else {
                break false;
            };
            #[cfg(test)]
            test_hooks::boundary("before_notice_retry", row);
            tokio::time::sleep(*delay).await;
        }
    } else {
        false
    };
    tracing::info!(
        event = "herdr_turn_policy_close",
        channel_id = row.channel_id,
        logical = proof.owner.logical_key,
        turn_nonce = proof.turn_nonce,
        runtime_generation = proof.generation,
        attempts,
        last_failure = last_failure.unwrap_or("none"),
        settled,
        "Herdr turn closed before its input started"
    );
    settled
}

enum CloseAttempt {
    Settled,
    Held,
    Retry(&'static str),
}

async fn consume<F>(
    shared: &Arc<SharedData>,
    (fin, mailbox): (&TurnFinalizer, &ChannelMailboxHandle),
    actor: &Arc<CancelToken>,
    row: &InflightTurnState,
    (proof, anchor): (&BeforeStartProof, MessageId),
    previous_sent: &mut Option<ReplaceLongMessageOutcome>,
    notice: impl FnOnce() -> F,
) -> CloseAttempt
where
    F: std::future::Future<Output = Result<ReplaceLongMessageOutcome, String>>,
{
    let Some(state) = actor.herdr_interrupt_state() else {
        return CloseAttempt::Held;
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
        return CloseAttempt::Held;
    }
    if !mailbox_owns(mailbox, actor).await {
        return CloseAttempt::Held;
    }
    #[cfg(test)]
    test_hooks::boundary("before_receipt", row);
    // The receipt is durable before anything is shown or released; a refusal keeps the hold.
    let generation = shared.restart.current_generation;
    let Some(admitted) = row
        .admit_before_start_receipt(&proof.turn_nonce, proof.basis, generation)
        .await
    else {
        return CloseAttempt::Held;
    };
    #[cfg(test)]
    test_hooks::boundary("after_receipt", &admitted);
    // The stop shows on the durable row's message, as a reply where O owns that body; an
    // unreadable ownership shows nothing.
    let o_body = o_owns_body(&admitted);
    if admitted.current_msg_id != anchor.get() || o_body.is_none() {
        return CloseAttempt::Held;
    }
    if !mailbox_owns(mailbox, actor).await {
        return CloseAttempt::Held;
    }
    // A re-entry whose notice is already durably delivered submits without showing it again.
    let acked = if admitted
        .herdr_before_start_stop
        .as_ref()
        .is_some_and(|receipt| receipt.notice_delivered())
    {
        admitted.clone()
    } else if o_body == Some(true) {
        let http = shared.serenity_http_or_token_fallback();
        let acked = match http {
            Some(http) => reply_and_ack_stop(&http, shared, &admitted).await,
            None => None,
        };
        let Some(acked) = acked else {
            return CloseAttempt::Retry("notice");
        };
        acked
    } else {
        // A successful send remains evidence for this anchor's ACK, never a substitute for it.
        let sent = match previous_sent.as_ref() {
            Some(sent) => Ok(sent.clone()),
            None => notice().await,
        };
        if let Ok(
            sent @ (ReplaceLongMessageOutcome::EditedOriginal
            | ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { .. }),
        ) = &sent
        {
            *previous_sent = Some(sent.clone());
        }
        let Some(acked) = admitted.ack_before_start_notice(&sent).await else {
            return CloseAttempt::Retry(if previous_sent.is_some() {
                "ack"
            } else {
                "notice"
            });
        };
        acked
    };
    #[cfg(test)]
    test_hooks::boundary("after_notice_ack", &admitted);
    let Some(intent) = policy_intent(shared, actor, row, proof, mailbox.clone(), acked) else {
        return CloseAttempt::Held;
    };
    #[cfg(test)]
    test_hooks::boundary("before_submit", &admitted);
    #[cfg(test)]
    test_hooks::submitting();
    let outcome = fin.submit_policy_close(intent, shared.clone()).await;
    #[cfg(test)]
    test_hooks::boundary("after_submit", &admitted);
    let reason: &dyn std::fmt::Debug = match &outcome {
        PolicyCloseOutcome::Settled | PolicyCloseOutcome::DuplicateConfirmed => {
            return CloseAttempt::Settled;
        }
        PolicyCloseOutcome::Superseded => return CloseAttempt::Held,
        PolicyCloseOutcome::Retained(PolicyRetain::Clear(clear)) => clear,
        PolicyCloseOutcome::Retained(retain) => retain,
        PolicyCloseOutcome::Unconfirmed(stage) => stage,
    };
    // The turn stays held with its acked receipt for restart; the reason names the stopping step.
    tracing::warn!(
        channel_id = row.channel_id,
        ?reason,
        "before-start policy close left its turn held"
    );
    CloseAttempt::Held
}

/// Whether O owns `row`'s destination body; `None` while that cannot be read.
pub(in crate::services::discord) fn o_owns_body(row: &InflightTurnState) -> Option<bool> {
    crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
        row.channel_id,
        row.runtime_kind,
    )
    .ok()
}

/// The stop notice as a reply to `row`'s message, for a body O owns. The turn nonce names the
/// send, so a resend after a lost ack is deduplicated rather than shown twice.
pub(in crate::services::discord) async fn send_stop_reply(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    row: &InflightTurnState,
) -> Result<Option<MessageId>, String> {
    if row.current_msg_id == 0 {
        return Err("no message to reply to".into());
    }
    let nonce = row.turn_nonce.as_deref().unwrap_or_default();
    crate::services::discord::outbound::serenity_reference::send_referenced_lifecycle_notice(
        http.clone(),
        shared.clone(),
        ChannelId::new(row.channel_id),
        MessageId::new(row.current_msg_id),
        ADMITTED_ABORT_NOTICE,
        format!("herdr-stop:{nonce}"),
        "herdr_stop_notice".into(),
    )
    .await
}

/// Shows the stop as a reply and acks `row`'s receipt only on the message id that send returned.
pub(in crate::services::discord) async fn reply_and_ack_stop(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    row: &InflightTurnState,
) -> Option<InflightTurnState> {
    let sent = send_stop_reply(http, shared, row).await;
    row.ack_before_start_reply(&sent).await
}

async fn mailbox_owns(mailbox: &ChannelMailboxHandle, actor: &Arc<CancelToken>) -> bool {
    mailbox
        .snapshot()
        .await
        .cancel_token
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, actor))
}

#[cfg(all(test, unix))]
async fn consume_once(
    shared: &Arc<SharedData>,
    fin: &TurnFinalizer,
    actor: &Arc<CancelToken>,
    row: &InflightTurnState,
    proof: (&BeforeStartProof, MessageId),
    notice: impl std::future::Future<Output = Result<ReplaceLongMessageOutcome, String>>,
) -> bool {
    let Some(mailbox) = shared.mailbox_peek(ChannelId::new(row.channel_id)) else {
        return false;
    };
    matches!(
        consume(
            shared,
            (fin, &mailbox),
            actor,
            row,
            proof,
            &mut None,
            || notice
        )
        .await,
        CloseAttempt::Settled
    )
}

/// The typed close of the row the notice ACK returned. The captured actor's own step clears its
/// receipt and only then releases the turn, so a successor never starts over it.
fn policy_intent(
    shared: &Arc<SharedData>,
    actor: &Arc<CancelToken>,
    row: &InflightTurnState,
    proof: &BeforeStartProof,
    mailbox: ChannelMailboxHandle,
    acked: InflightTurnState,
) -> Option<PolicyCloseIntent> {
    let generation = shared.restart.current_generation;
    let channel = ChannelId::new(row.channel_id);
    let key = TurnKey::new(channel, row.effective_finalizer_turn_id(), generation)
        .with_episode_nonce(row.turn_nonce.as_deref());
    let user_message_id = (key.user_msg_id != 0).then(|| MessageId::new(key.user_msg_id))?;
    let request = PolicyCloseRequest {
        actor: Arc::downgrade(actor),
        user_message_id,
        turn_nonce: proof.turn_nonce.clone(),
        interrupt_generation: proof.generation,
        observed_before: std::time::Instant::now(),
        receipt: acked.into_policy_receipt(generation),
        active: shared.restart.global_active.clone(),
        persistence: crate::services::discord::queue_persistence_context(
            shared,
            &shared.provider,
            channel,
        ),
        release_reconcile_allowed: false,
        ticket: None,
        trace: Default::default(),
    };
    // Tests bind the request's own probe and settlement switch to it here.
    #[cfg(test)]
    let request = crate::services::turn_orchestrator::policy_close::test_hooks::bind(request);
    Some(PolicyCloseIntent {
        key,
        provider: shared.provider.clone(),
        mailbox,
        request,
        snapshot: SyntheticClaimSnapshot::from_row(row),
    })
}

#[cfg(test)]
#[cfg(unix)]
#[path = "before_start_stop_tests.rs"]
pub(in crate::services::discord::turn_bridge) mod tests;

/// Test seams at each step of `consume`: a per-thread observer, a policy-submit count, and a
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

    /// Policy submissions `consume` made on this thread.
    #[cfg(unix)]
    pub(in crate::services::discord) fn submitted() -> usize {
        SUBMITTED.with(Cell::get)
    }

    /// Counts one call to the finalizer's policy submit API, right before it is made.
    pub(super) fn submitting() {
        SUBMITTED.with(|count| count.set(count.get() + 1));
    }

    pub(super) fn boundary(step: &'static str, row: &InflightTurnState) {
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
