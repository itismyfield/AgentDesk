//! A forced cancel purge drains the channel queue without a live session key.
use std::sync::Arc;
use std::time::Instant;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use crate::services::discord::health::HealthRegistry;
use crate::services::provider::ProviderKind;
use crate::services::queue::force_purge_channel_mailbox;
use crate::services::turn_lifecycle::TurnLifecycleTarget;
use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, EnqueueRefusalReason, Intervention, InterventionMode,
    QueuePersistenceContext, save_channel_queue,
};

struct EnvReset(Option<std::ffi::OsString>);

impl Drop for EnvReset {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

fn make_intervention(message_id: u64, text: &str) -> Intervention {
    Intervention {
        author_id: UserId::new(1),
        author_is_bot: false,
        message_id: MessageId::new(message_id),
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: vec![MessageId::new(message_id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: text.to_string(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

#[test]
fn force_purge_channel_mailbox_drains_queue_without_session_key() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
    let temp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(9270601);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let persistence = QueuePersistenceContext::new(&provider, "force-purge-test", None);
        handle
            .replace_queue(
                vec![make_intervention(10, "stale queued prompt")],
                persistence,
            )
            .await;

        let target = TurnLifecycleTarget {
            provider: Some(provider),
            channel_id: Some(channel_id),
            tmux_name: String::new(),
        };

        let purged = force_purge_channel_mailbox(None::<&Arc<HealthRegistry>>, &target, None).await;

        assert_eq!(purged.map(|c| c.drained), Some(1));
        assert!(handle.snapshot().await.intervention_queue.is_empty());
    });
}

/// #4535 mutation guard: a restart replay can hydrate durable work and then
/// dedup without reporting a fresh enqueue. Handle creation must already
/// have published the actor so force purge reaches the live hydrated queue.
#[test]
fn force_purge_reaches_queue_hydrated_by_dedup_replay() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
    let temp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(9_270_602);
        let persistence = QueuePersistenceContext::new(&provider, "", None);
        let durable = make_intervention(11, "hydrated stale prompt");
        save_channel_queue(
            &provider,
            &persistence.token_hash,
            channel_id,
            std::slice::from_ref(&durable),
            None,
        )
        .expect("seed durable pending queue");

        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel_id);
        let replay = handle.enqueue(durable, persistence.clone()).await;
        assert!(!replay.enqueued);
        assert!(!replay.merged);
        assert_eq!(
            replay.refusal_reason,
            Some(EnqueueRefusalReason::SourceIdAlreadyQueued)
        );
        assert_eq!(handle.snapshot().await.intervention_queue.len(), 1);
        assert!(
            ChannelMailboxRegistry::global_handle(channel_id).is_some(),
            "hydrated actor must remain reachable through the force-purge registry"
        );

        let target = TurnLifecycleTarget {
            provider: Some(provider),
            channel_id: Some(channel_id),
            tmux_name: String::new(),
        };
        let purged = force_purge_channel_mailbox(None::<&Arc<HealthRegistry>>, &target, None).await;

        assert_eq!(purged.map(|c| c.drained), Some(1));
        assert!(handle.snapshot().await.intervention_queue.is_empty());
        let later_dispatch = handle.take_next_soft(persistence).await;
        assert!(
            later_dispatch.intervention.is_none(),
            "force-purged hydrated message must not dispatch later"
        );
    });
}
