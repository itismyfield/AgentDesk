//! Create-time refusal when another turn's durable inflight row holds the channel. Each turn
//! mints a fresh nonce, so that row would abort this turn's bridge at entry and drop the prompt.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::HeadlessTurnStartError;
use crate::services::discord::SharedData;
use crate::services::discord::inflight::{CreateNewInflightError, InflightTurnState};
use crate::services::discord::live_bridge::defer_unstarted_turn;
use crate::services::provider::{CancelToken, ProviderKind};

/// Logs the create outcome; true when another turn's durable row holds the channel.
fn held_by_another_turn(
    created: Result<(), CreateNewInflightError>,
    provider: &ProviderKind,
    state: &InflightTurnState,
) -> bool {
    if !matches!(created, Err(CreateNewInflightError::AlreadyExists)) {
        super::intake_turn::inflight_create_log::log_create_new_inflight_outcome(
            created, provider, state,
        );
        return false;
    }
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = state.channel_id,
        user_msg_id = state.user_msg_id,
        "turn start refused: another turn's durable inflight row holds the channel"
    );
    true
}

/// Headless start: unwinds this turn's claim and answers Conflict, so the caller keeps the input.
pub(super) async fn admit_headless(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    cancel_token: &Arc<CancelToken>,
    created: Result<(), CreateNewInflightError>,
) -> Result<(), HeadlessTurnStartError> {
    if !held_by_another_turn(created, provider, state) {
        return Ok(());
    }
    let channel = ChannelId::new(state.channel_id);
    crate::services::discord::mailbox_finish::unwind_unstarted_turn(shared, channel, cancel_token)
        .await;
    Err(HeadlessTurnStartError::Conflict(format!(
        "another turn's durable inflight row holds channel {}",
        state.channel_id
    )))
}

/// Intake turn: on a foreign row, releases the claim and front-requeues the prompt behind the
/// slow backstop, so it starts once after the row clears. `Ok(false)` stops before any spawn.
pub(super) async fn admit(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    cancel_token: &Arc<CancelToken>,
    created: Result<(), CreateNewInflightError>,
) -> Result<bool, String> {
    if !held_by_another_turn(created, provider, state) {
        return Ok(true);
    }
    let reason = "foreign_row_start_deferred";
    if defer_unstarted_turn(shared, provider, state, cancel_token, true, reason).await {
        return Ok(false);
    }
    Err(format!(
        "another turn's durable inflight row holds channel {}; retry enqueue refused",
        state.channel_id
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::{
        idle_queue_take_next_soft_if_ready, inflight, mailbox_enqueue_intervention,
        mailbox_snapshot, mailbox_try_start_turn,
    };
    use crate::services::turn_orchestrator::{
        Intervention, InterventionMode, SourceMessageQueuedGeneration,
    };
    use poise::serenity_prelude::{MessageId, UserId};
    use std::sync::atomic::Ordering;

    const PROMPT: &str = "iMessage: are you there?";

    /// The kickoff's dequeue, then what intake does from its claim to the row create verdict.
    async fn promote_and_admit(
        shared: &Arc<SharedData>,
        channel: ChannelId,
        message: MessageId,
        sources: &[MessageId],
        retry: MessageId,
        text: &str,
    ) -> (Arc<CancelToken>, Result<bool, String>) {
        let provider = shared.provider.clone();
        let taken = idle_queue_take_next_soft_if_ready(shared, &provider, channel).await;
        let (item, _, _lease) = taken.into_intervention().expect("queued head");
        assert_eq!(item.message_id, message);
        let token = Arc::new(CancelToken::new());
        assert!(
            mailbox_try_start_turn(shared, channel, token.clone(), UserId::new(7), message).await
        );
        let mut state = InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            7,
            message.get(),
            0,
            text.to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        state.turn_nonce = token.turn_nonce().map(str::to_owned);
        state.source_message_ids = sources.iter().map(|id| id.get()).collect();
        state.busy_followup_retry_user_msg_id = retry.get();
        state.set_followup_requeue_context(None, false, false, Vec::new(), None, true);
        let created = inflight::save_inflight_state_create_new(&state);
        let verdict = admit(shared, &provider, &state, &token, created).await;
        (token, verdict)
    }

    /// The intervention deliver queues, or a merged head carrying every absorbed id.
    fn queued(message: MessageId, sources: &[MessageId]) -> Intervention {
        let generation = crate::services::discord::runtime_store::process_generation();
        Intervention {
            author_id: UserId::new(7),
            author_is_bot: false,
            message_id: message,
            queued_generation: generation,
            source_message_ids: sources.to_vec(),
            source_message_queued_generations: sources
                .iter()
                .map(|id| SourceMessageQueuedGeneration::user_instruction(*id, generation))
                .collect(),
            source_text_segments: Vec::new(),
            text: PROMPT.to_string(),
            mode: InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    fn queued_ids(queue: &[Intervention]) -> Vec<u64> {
        queue.iter().map(|item| item.message_id.get()).collect()
    }

    #[tokio::test]
    async fn an_external_row_requeues_the_promoted_input_and_starts_it_once_after_clear() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = shared.provider.clone();
        let channel = ChannelId::new(6_245_301);
        let message = MessageId::new(6_245_302);
        crate::services::discord::health::seed_external_turn_row_for_tests(
            &provider,
            channel.get(),
        );
        // deliver answered `queued external_turn_active` with this intervention.
        let enqueued =
            mailbox_enqueue_intervention(&shared, &provider, channel, queued(message, &[message]))
                .await;
        assert!(enqueued.enqueued);

        // The kickoff promotes it while the external row is still on disk.
        let (token, verdict) =
            promote_and_admit(&shared, channel, message, &[message], message, PROMPT).await;
        let snapshot = mailbox_snapshot(&shared, channel).await;
        let row = inflight::load_inflight_state_read_only(&provider, channel.get());
        assert_eq!(
            (
                verdict,
                snapshot.cancel_token.is_none(),
                token.cancelled.load(Ordering::Relaxed),
                queued_ids(&snapshot.intervention_queue),
                snapshot
                    .intervention_queue
                    .first()
                    .map(|item| item.text.clone()),
                row.map(|row| (row.user_msg_id, row.turn_source)),
            ),
            (
                Ok(false),
                true,
                true,
                vec![message.get()],
                Some(PROMPT.to_string()),
                Some((0, inflight::TurnSource::ExternalInput)),
            ),
            "no spawn, claim released, prompt requeued once, external row kept"
        );

        // After the external row clears, the same prompt starts exactly once.
        inflight::clear_inflight_state(&provider, channel.get());
        let (_token, verdict) =
            promote_and_admit(&shared, channel, message, &[message], message, PROMPT).await;
        let snapshot = mailbox_snapshot(&shared, channel).await;
        assert_eq!(
            (verdict, queued_ids(&snapshot.intervention_queue)),
            (Ok(true), Vec::new())
        );
    }

    #[tokio::test]
    async fn a_merged_head_keeps_every_source_id_through_the_requeue_and_settles_them_all() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let ids = [6_245_401, 6_245_402, 6_245_403];
        let [a, b, c] = ids.map(MessageId::new);
        let mut missing = Vec::new();
        // Without a busy retry binding the retry id is the primary; with one it is the oldest id.
        for (variant, retry) in [(0, c), (1, a)] {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let provider = shared.provider.clone();
            let channel = ChannelId::new(6_245_410 + variant);
            crate::services::discord::health::seed_external_turn_row_for_tests(
                &provider,
                channel.get(),
            );
            let merged = queued(c, &[a, b, c]);
            assert!(
                mailbox_enqueue_intervention(&shared, &provider, channel, merged)
                    .await
                    .enqueued
            );
            let (_token, verdict) =
                promote_and_admit(&shared, channel, c, &[a, b, c], retry, PROMPT).await;
            let snapshot = mailbox_snapshot(&shared, channel).await;
            let known =
                crate::services::discord::recovery_known_ids::recovery_known_message_ids(&snapshot);
            assert_eq!(verdict, Ok(false));
            let unknown: Vec<u64> = ids.into_iter().filter(|id| !known.contains(id)).collect();

            // The retry claims the merged head again and its delivery settles A, B and C.
            inflight::clear_inflight_state(&provider, channel.get());
            let (token, verdict) =
                promote_and_admit(&shared, channel, c, &[a, b, c], retry, PROMPT).await;
            assert_eq!(verdict, Ok(true));
            crate::services::discord::outbound::completed_turn_ledger::append_completed_episode(
                &provider,
                channel.get(),
                c.get(),
                token.turn_nonce(),
            );
            let settled = crate::services::discord::outbound::completed_turn_ledger::read_ledger(
                &provider,
                channel.get(),
            )
            .map(|ledger| ledger.settled_ids())
            .unwrap_or_default();
            let unsettled: Vec<u64> = ids.into_iter().filter(|id| !settled.contains(id)).collect();
            missing.push((variant, unknown, unsettled));
        }
        // Neither catch-up's known set nor the settled ledger may lose an absorbed id.
        assert_eq!(missing, [(0, vec![], vec![]), (1, vec![], vec![])]);
    }

    #[tokio::test]
    async fn a_requeued_merged_head_keeps_each_body_on_its_own_id_through_a_partial_strip() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let [a, b, c] = [6_245_501, 6_245_502, 6_245_503].map(MessageId::new);
        // Without explicit segments each line belongs to the id at the same position.
        let bodies = "body A\nbody B\nbody C";
        let mut left = Vec::new();
        for (variant, retry) in [(0, c), (1, a)] {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let provider = shared.provider.clone();
            let channel = ChannelId::new(6_245_510 + variant);
            crate::services::discord::health::seed_external_turn_row_for_tests(
                &provider,
                channel.get(),
            );
            let mut merged = queued(c, &[a, b, c]);
            merged.text = bodies.to_string();
            assert!(
                mailbox_enqueue_intervention(&shared, &provider, channel, merged)
                    .await
                    .enqueued
            );
            let (_token, verdict) =
                promote_and_admit(&shared, channel, c, &[a, b, c], retry, bodies).await;
            assert_eq!(verdict, Ok(false));

            // A settles after the requeue, so the next dequeue strips A from the merged head.
            std::thread::sleep(std::time::Duration::from_millis(5));
            crate::services::discord::outbound::completed_turn_ledger::append_completed_episode(
                &provider,
                channel.get(),
                a.get(),
                None,
            );
            let taken = idle_queue_take_next_soft_if_ready(&shared, &provider, channel).await;
            let (item, _, _lease) = taken.into_intervention().expect("stripped head");
            let segments: Vec<(u64, String)> = item
                .source_text_segments()
                .into_iter()
                .map(|segment| (segment.message_id.get(), segment.text))
                .collect();
            left.push((variant, segments, item.text));
        }
        let kept = vec![
            (b.get(), "body B".to_string()),
            (c.get(), "body C".to_string()),
        ];
        let text = "body B\nbody C".to_string();
        assert_eq!(left, [(0, kept.clone(), text.clone()), (1, kept, text)]);
    }

    #[test]
    fn intake_judges_its_row_create_before_any_provider_or_bridge_spawn() {
        let src = include_str!("intake_turn.rs");
        let create = "let row = super::super::super::inflight::save_inflight_state_create_new(&inflight_state);";
        let admit = "if !super::foreign_row::admit(shared, &provider, &inflight_state, &cancel_token, row).await? {\n        return Ok(());\n    }";
        // The provider spawn follows the teardown verdict; the bridge spawn comes after it.
        // Split so the bridge entry-site census does not count this file as a caller.
        let bridge = ["\n    spawn_turn_", "bridge("].concat();
        let anchors = [
            create,
            admit,
            "for_turn(",
            "spawn_blocking(move ||",
            &bridge,
        ];
        let mut from = src.find(create).unwrap_or(src.len());
        for anchor in anchors {
            let at = src[from..].find(anchor).map(|at| from + at);
            assert!(at.is_some(), "{anchor} missing after the previous anchor");
            from = at.unwrap_or(from);
        }
        assert!(
            src.find(create)
                .zip(src.find(admit))
                .is_some_and(|(c, a)| c + create.len() + 5 == a)
        );
        assert_eq!(src.matches("save_inflight_state_create_new(").count(), 2);
    }
}
