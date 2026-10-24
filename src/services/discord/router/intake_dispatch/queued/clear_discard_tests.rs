use super::*;
use crate::services::turn_orchestrator::{InterventionMode, QueuePersistenceContext};
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[tokio::test]
async fn a_clear_superseded_restore_owner_consumes_protected_instead_of_retrying_forever() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new(""));
    let deps = super::super::super::message_handler::IntakeDeps {
        http: &http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &shared,
        token: "clear-restore-test",
    };
    let channel = ChannelId::new(7_186_301);
    let provider = ProviderKind::Claude;
    let persistence = QueuePersistenceContext::new(&provider, &shared.token_hash, None);
    let message_id = MessageId::new(7_186_302);
    let original = Intervention {
        author_id: UserId::new(7_186_303),
        author_is_bot: false,
        message_id,
        queued_generation: 1,
        source_message_ids: vec![message_id],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "discarded waiting input".into(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    };
    let mailbox = shared.mailbox(channel);
    assert!(
        mailbox
            .enqueue(original.clone(), persistence.clone())
            .await
            .enqueued
    );
    let taken = mailbox.take_next_soft(persistence.clone()).await;
    let dispatch_lease = taken
        .dispatch_lease
        .expect("the real dequeue leases the selected input");
    assert_eq!(taken.intervention.unwrap().message_id, message_id);
    let cleared = mailbox
        .discard_waiting_or_refused(persistence.clone())
        .await
        .unwrap();
    assert!(cleared.discarded_message_ids.contains(&message_id));
    let witness = mailbox
        .restore_dequeued_head(original.clone(), persistence, dispatch_lease.clone())
        .await;
    assert_eq!(
        witness.refusal_reason,
        Some(crate::services::turn_orchestrator::EnqueueRefusalReason::SupersededByClear)
    );
    let owner = QueuedRestoreOwner {
        original,
        provider,
        channel_id: channel,
        dispatch_lease,
    };
    let result = tokio::time::timeout(Duration::from_secs(1), owner.restore(&deps)).await;
    assert!(
        matches!(
            result,
            Ok(super::super::super::IntakeExecution::ConsumedProtected)
        ),
        "a witnessed clear-superseded restore must terminate protected: {result:?}"
    );
    assert!(mailbox.snapshot().await.intervention_queue.is_empty());
}
