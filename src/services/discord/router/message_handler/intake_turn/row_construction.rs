use super::*;
use crate::services::discord::inflight::{self, CreateNewInflightError};
use crate::services::discord::queue_io::arm_slow_idle_queue_backstop_if_queue_nonempty as arm;
use crate::services::discord::{mailbox_finish, mailbox_requeue_intervention_front};
use std::sync::atomic::Ordering::Relaxed;

pub(super) enum RefusedStart {
    Requeued(MailboxEnqueueOutcome),
    LeaseLost,
}

/// Counter and actor side of giving back a claimed slot before submit.
pub(super) fn abandon_claimed_start(shared: &SharedData, channel: ChannelId, actor: &CancelToken) {
    crate::services::discord::saturating_decrement_global_active(shared);
    shared.turn_start_times.remove(&channel);
    actor.cancelled.store(true, Relaxed);
}

/// On a foreign row the construction owner releases its own lease and requeues the message
/// for the slow backstop; an `Internal` store failure still starts, without a durable row.
pub(super) async fn construct_or_refuse(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    actor: &Arc<CancelToken>,
    request_owner: serenity::UserId,
    state: InflightTurnState,
) -> Result<InflightTurnState, RefusedStart> {
    let result = inflight::save_inflight_state_create_new(&state);
    let channel = ChannelId::new(state.channel_id);
    let head = MessageId::new(state.user_msg_id);
    if !matches!(result, Err(CreateNewInflightError::AlreadyExists)) {
        inflight_create_log::log_create_new_inflight_outcome(result, provider, &state);
        inflight_create_log::record_turn_start_origin(provider, channel, &state).await;
        return Ok(state);
    }
    let released = mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion(
        shared, provider, channel, head, state.turn_nonce.clone(), std::time::Instant::now(), Some(actor.clone()),
    ).await;
    let refusal = if released.removed_token.is_some() {
        abandon_claimed_start(shared, channel, actor);
        let mut item = super::super::super::response_format::build_race_requeued_intervention(
            request_owner,
            head,
            &state.user_text,
            state.followup_preserve_on_cancel,
            state.followup_reply_context.clone(),
            state.followup_has_reply_boundary,
            state.followup_merge_consecutive,
            state.followup_pending_uploads.clone(),
            state.followup_voice_announcement.clone(),
        );
        let ids = state.source_message_ids.iter().copied().map(MessageId::new);
        let missing_head = (!state.source_message_ids.contains(&head.get())).then_some(head);
        item.source_message_ids = ids.chain(missing_head).collect();
        let outcome = mailbox_requeue_intervention_front(shared, provider, channel, item).await;
        arm(shared, provider, channel, "intake_refused_start").await;
        RefusedStart::Requeued(outcome)
    } else {
        RefusedStart::LeaseLost
    };
    let occupant = inflight::load_inflight_state_read_only(provider, state.channel_id)
        .map(|row| (row.user_msg_id, row.turn_nonce));
    tracing::warn!(
        channel_id = state.channel_id, user_msg_id = state.user_msg_id, nonce = ?state.turn_nonce, ?occupant,
        requeued = matches!(refusal, RefusedStart::Requeued(_)),
        "intake refused start: inflight row occupied; own lease released and message requeued, or lease already lost and message dropped with notice"
    );
    Err(refusal)
}

impl RefusedStart {
    /// A lost lease drops the message with its own notice and does not revive
    /// the session retry context that the winning /clear took.
    pub(super) async fn abandon<'a, S: std::future::Future<Output = ()>>(
        self,
        finalize_ctx: impl FnOnce(Option<&'static str>) -> busy_retry::FinalizeEnqueueContext<'a>,
        awaiting_user: impl FnOnce() -> S,
    ) -> Result<(), Error> {
        let (outcome, ctx) = match self {
            RefusedStart::Requeued(outcome) => (outcome, finalize_ctx(None)),
            RefusedStart::LeaseLost => {
                let notice = super::super::tui_followup::INTAKE_START_REFUSED_NOTICE;
                let mut ctx = finalize_ctx(Some(notice));
                ctx.session_retry_context = None;
                (MailboxEnqueueOutcome::default(), ctx)
            }
        };
        busy_retry::finalize_enqueue(ctx, &outcome).await;
        awaiting_user().await;
        Ok(())
    }
}
