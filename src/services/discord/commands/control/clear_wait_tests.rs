//! A /clear that meets a pane effect holding the session transition waits the usual window, then
//! goes ahead with a warning and discards the live queued head, never refusing.

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use super::clear_channel_session_state_with_session_key;
use super::clear_persist_failure_tests::{queue_len, seed_session};
use crate::services::discord::{SharedData, make_shared_data_for_tests};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, HeadOutcome, HeadSettle, Intervention, InterventionMode, QueueIdentity,
    QueuePersistenceContext, QueuedHeadClaim, SourceMessageQueuedGeneration,
};

fn person(id: u64, author: u64) -> Intervention {
    let source = SourceMessageQueuedGeneration::user_instruction(MessageId::new(id), 5);
    Intervention {
        author_id: UserId::new(author),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 5,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: vec![source],
        source_text_segments: Vec::new(),
        text: format!("queued {id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: true,
        pending_uploads: Vec::new(),
    }
}

fn context(shared: &SharedData, provider: &ProviderKind) -> QueuePersistenceContext {
    QueuePersistenceContext::new(provider, &shared.token_hash, None)
}

#[test]
fn a_clear_overtakes_a_held_pane_effect_and_discards_the_live_head() {
    let root = tempfile::tempdir().expect("scratch runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let provider = ProviderKind::Gemini;
    let channel_id = ChannelId::new(6_995_101);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let shared = make_shared_data_for_tests();
        seed_session(&shared, channel_id).await;
        let mailbox = shared.mailbox(channel_id);
        let head = person(6_995_102, 7);
        assert!(
            mailbox
                .enqueue(head.clone(), context(&shared, &provider))
                .await
                .enqueued
        );
        let claim = mailbox
            .claim_queued_head(QueueIdentity::of(&head), context(&shared, &provider))
            .await;
        let QueuedHeadClaim::Reserved(ticket) = claim else {
            panic!("the head is reserved");
        };
        let marked = mailbox
            .settle_queued_head(
                ticket,
                HeadOutcome::MayHaveSent,
                context(&shared, &provider),
            )
            .await;
        let HeadSettle::Committed(Some(ticket)) = marked else {
            panic!("the head may have been sent");
        };
        let tail = person(6_995_103, 8);
        assert!(
            mailbox
                .enqueue(tail, context(&shared, &provider))
                .await
                .enqueued
        );
        // The queued injector holds the transition from its claim through its settle.
        let effect = shared
            .session_transition_lock(channel_id)
            .lock_owned()
            .await;

        let http = Arc::new(serenity::Http::new(""));
        let report = clear_channel_session_state_with_session_key(
            &http, &shared, &provider, channel_id, "/clear", None,
        )
        .await
        .expect("the clear goes ahead past the held effect");
        assert!(report.transition_busy, "{report:?}");
        let warning = report.warning().expect("the overtaken effect is warned");
        assert!(warning.contains("기다리지 않고"), "{warning}");
        drop(effect);

        assert_eq!(queue_len(&shared, channel_id).await, 0);
        let late = mailbox
            .settle_queued_head(ticket, HeadOutcome::Injected, context(&shared, &provider))
            .await;
        assert!(matches!(late, HeadSettle::Stale));
        assert_eq!(queue_len(&shared, channel_id).await, 0);
        let restart = ChannelMailboxRegistry::default().handle(channel_id);
        let hydrated = restart
            .hydrate_pending_queue_from_disk(context(&shared, &provider))
            .await;
        assert_eq!(hydrated.persistence_error, None);
        assert!(restart.snapshot().await.intervention_queue.is_empty());
    });
}
