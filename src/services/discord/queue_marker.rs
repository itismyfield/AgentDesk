use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::{SharedData, queue_reactions, turn_view_reconciler};
use crate::services::turn_orchestrator::{QueueExitEvent, SourceMessageQueuedGeneration};

fn effective_queued_generation(shared: &SharedData, generation: u64) -> u64 {
    if generation == 0 {
        shared.restart.current_generation
    } else {
        generation
    }
}

pub(in crate::services::discord) async fn note_added_current(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
    source: &'static str,
) {
    turn_view_reconciler::note_intake_queue_marker_added_current(
        shared, http, channel_id, message_id, emoji, source,
    )
    .await;
}

pub(in crate::services::discord) async fn note_removed_current(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
    source: &'static str,
) {
    turn_view_reconciler::note_intake_queue_marker_removed_current(
        shared, http, channel_id, message_id, emoji, source,
    )
    .await;
}

pub(in crate::services::discord) async fn note_exit_feedback_added(
    shared: &SharedData,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
) {
    let target = turn_view_reconciler::TurnViewTarget::intake_user_message(channel_id, message_id);
    shared
        .turn_view_reconciler
        .note_untracked_reaction_added(
            shared,
            target,
            turn_view_reconciler::TurnViewIdentity::IntakeHttp(http.clone()),
            emoji,
            "queue_exit_feedback_reaction",
        )
        .await;
}

async fn note_removed_via_shared_generation(
    shared: &SharedData,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
    generation: u64,
    source: &'static str,
) {
    let generation = effective_queued_generation(shared, generation);
    let target = turn_view_reconciler::TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner =
        turn_view_reconciler::turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_queue_marker_removed(
            shared,
            target,
            owner,
            turn_view_reconciler::TurnViewIdentity::IntakeShared,
            emoji,
            source,
        )
        .await;
}

pub(in crate::services::discord) async fn note_removed_queued_generation(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
    generation: u64,
    source: &'static str,
) {
    let generation = effective_queued_generation(shared, generation);
    turn_view_reconciler::note_intake_queue_marker_removed(
        shared, http, channel_id, message_id, generation, emoji, source,
    )
    .await;
}

pub(in crate::services::discord) async fn drain_queue_exit_markers(
    shared: &SharedData,
    _http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    queue_exit_events: &[&QueueExitEvent],
) {
    for event in queue_exit_events {
        let source_generations = event.intervention.source_message_queued_generations();
        let is_standalone = source_generations.len() <= 1;
        for source_generation in &source_generations {
            for emoji in queue_reactions::drain_reactions_for_queue_exit(is_standalone) {
                note_removed_via_shared_generation(
                    shared,
                    channel_id,
                    source_generation.message_id,
                    *emoji,
                    source_generation.queued_generation,
                    "queue_exit_feedback",
                )
                .await;
            }
        }
    }
}

pub(in crate::services::discord) async fn drain_dispatched_queue_markers(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    head_message_id: MessageId,
    source_message_generations: &[SourceMessageQueuedGeneration],
) {
    for source_generation in source_message_generations {
        for emoji in queue_reactions::QUEUE_PENDING_REACTION_EMOJIS {
            if emoji == queue_reactions::QUEUE_STANDALONE_PENDING_REACTION
                && source_generation.message_id == head_message_id
            {
                continue;
            }
            note_removed_queued_generation(
                shared,
                http,
                channel_id,
                source_generation.message_id,
                emoji,
                source_generation.queued_generation,
                "dispatch_queued_turn_marker_clear",
            )
            .await;
        }
    }
}

pub(in crate::services::discord) async fn start_and_drain_kickoff_markers(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    head_message_id: MessageId,
    source_message_generations: &[SourceMessageQueuedGeneration],
) {
    turn_view_reconciler::note_intake_turn_started_current(
        shared,
        http,
        channel_id,
        head_message_id,
        "idle_kickoff_queued_turn_started",
    )
    .await;
    for source_generation in source_message_generations {
        for emoji in queue_reactions::QUEUE_PENDING_REACTION_EMOJIS {
            if emoji == queue_reactions::QUEUE_STANDALONE_PENDING_REACTION
                && source_generation.message_id == head_message_id
            {
                continue;
            }
            note_removed_queued_generation(
                shared,
                http,
                channel_id,
                source_generation.message_id,
                emoji,
                source_generation.queued_generation,
                "idle_kickoff_queue_marker_clear",
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use poise::serenity_prelude as serenity;
    use serenity::{ChannelId, MessageId};

    use super::*;

    fn persisted_path(channel_id: ChannelId, message_id: MessageId) -> std::path::PathBuf {
        super::super::runtime_store::discord_turn_view_reconciler_root()
            .expect("turn view reconciler root")
            .join("intake_user_message")
            .join(format!("{}-{}.json", channel_id.get(), message_id.get()))
    }

    fn remove_persisted(channel_id: ChannelId, message_id: MessageId) {
        let path = persisted_path(channel_id, message_id);
        if let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            panic!(
                "failed to clear persisted test state {}: {error}",
                path.display()
            );
        }
    }

    struct ScopedRuntimeRoot {
        _lock: std::sync::MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }

    impl Drop for ScopedRuntimeRoot {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                    None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
                }
            }
        }
    }

    #[must_use]
    fn scoped_runtime_root() -> ScopedRuntimeRoot {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        let temp = tempfile::tempdir().expect("create temp runtime dir for queue marker test");
        unsafe {
            std::env::set_var(
                "AGENTDESK_ROOT_DIR",
                temp.path().to_str().expect("temp path must be valid utf-8"),
            );
        }
        ScopedRuntimeRoot {
            _lock: lock,
            _temp: temp,
            prev,
        }
    }

    #[tokio::test]
    async fn merged_and_reconcile_markers_add_and_remove_through_reconciler() {
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = Arc::new(serenity::Http::new("Bot test-token"));
        let channel_id = ChannelId::new(100_000_000_000_151);
        let cases = [
            (
                MessageId::new(100_000_000_000_152),
                queue_reactions::QUEUE_MERGED_PENDING_REACTION,
                "test_merged_marker",
            ),
            (
                MessageId::new(100_000_000_000_153),
                queue_reactions::QUEUE_RECONCILE_PENDING_REACTION,
                "test_reconcile_marker",
            ),
        ];

        for (message_id, emoji, source) in cases {
            remove_persisted(channel_id, message_id);

            note_added_current(&shared, &http, channel_id, message_id, emoji, source).await;
            assert!(persisted_path(channel_id, message_id).exists());
            assert!(
                shared.turn_view_reconciler.ops().iter().any(|op| {
                    op.target.channel_id == channel_id
                        && op.target.message_id == message_id
                        && op.add
                        && op.emoji == emoji
                }),
                "{emoji} queue marker add must be reconciler-owned"
            );

            note_removed_current(
                &shared,
                &http,
                channel_id,
                message_id,
                emoji,
                "test_marker_remove",
            )
            .await;
            assert!(!persisted_path(channel_id, message_id).exists());
            assert!(
                shared.turn_view_reconciler.ops().iter().any(|op| {
                    op.target.channel_id == channel_id
                        && op.target.message_id == message_id
                        && !op.add
                        && op.emoji == emoji
                }),
                "{emoji} queue marker removal must be reconciler-owned"
            );
        }
    }

    #[tokio::test]
    async fn queue_exit_feedback_reactions_use_reconciler_untracked_path() {
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = Arc::new(serenity::Http::new("Bot test-token"));
        let channel_id = ChannelId::new(100_000_000_000_161);
        let cases = [
            (MessageId::new(100_000_000_000_162), '🚫'),
            (MessageId::new(100_000_000_000_163), '⌛'),
            (MessageId::new(100_000_000_000_164), '⏏'),
        ];

        for (message_id, emoji) in cases {
            remove_persisted(channel_id, message_id);
            note_exit_feedback_added(&shared, &http, channel_id, message_id, emoji).await;

            assert!(
                shared.turn_view_reconciler.ops().iter().any(|op| {
                    op.target.channel_id == channel_id
                        && op.target.message_id == message_id
                        && op.add
                        && op.emoji == emoji
                }),
                "{emoji} queue-exit feedback must be emitted by the reconciler"
            );
            assert!(
                !persisted_path(channel_id, message_id).exists(),
                "queue-exit feedback is untracked and must not create durable queue state"
            );
        }
    }

    #[tokio::test]
    async fn dispatch_drain_mailbox_marker_discards_persisted_queued_state() {
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = Arc::new(serenity::Http::new("Bot test-token"));
        let channel_id = ChannelId::new(100_000_000_000_171);
        let source_a = MessageId::new(100_000_000_000_172);
        remove_persisted(channel_id, source_a);

        turn_view_reconciler::note_intake_queue_marker_added(
            &shared,
            &http,
            channel_id,
            source_a,
            53,
            queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
            "test_seed_queued_source_a",
        )
        .await;
        assert!(persisted_path(channel_id, source_a).exists());

        note_removed_queued_generation(
            &shared,
            &http,
            channel_id,
            source_a,
            queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
            53,
            "test_dispatch_queued_turn_marker_clear",
        )
        .await;

        let ops = shared.turn_view_reconciler.ops();
        assert!(
            ops.iter().any(|op| op.target.channel_id == channel_id
                && op.target.message_id == source_a
                && !op.add
                && op.emoji == '📬'),
            "dispatch drain must remove the visible mailbox marker through the reconciler"
        );
        assert!(
            !persisted_path(channel_id, source_a).exists(),
            "dispatch drain must discard the persisted queued state atomically"
        );
    }

    #[tokio::test]
    async fn regression_4049_stale_dispatch_drain_generation_preserves_newer_queued_state() {
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = Arc::new(serenity::Http::new("Bot test-token"));
        let channel_id = ChannelId::new(100_000_000_000_181);
        let source = MessageId::new(100_000_000_000_182);
        let stale_head = MessageId::new(100_000_000_000_183);
        remove_persisted(channel_id, source);

        turn_view_reconciler::note_intake_queue_marker_added(
            &shared,
            &http,
            channel_id,
            source,
            54,
            queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
            "test_seed_fresh_queued_source",
        )
        .await;
        assert!(persisted_path(channel_id, source).exists());
        let ops_after_queue = shared.turn_view_reconciler.ops().len();

        drain_dispatched_queue_markers(
            &shared,
            &http,
            channel_id,
            stale_head,
            &[SourceMessageQueuedGeneration::new(source, 53)],
        )
        .await;

        assert_eq!(
            shared.turn_view_reconciler.ops().len(),
            ops_after_queue,
            "stale dispatch drain must not touch a newer queued marker"
        );
        assert!(
            persisted_path(channel_id, source).exists(),
            "stale dispatch drain must preserve newer persisted queued state"
        );
    }

    #[tokio::test]
    async fn regression_4049_dispatch_drain_skips_anchor_mailbox_for_start_swap() {
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = Arc::new(serenity::Http::new("Bot test-token"));
        let channel_id = ChannelId::new(100_000_000_000_191);
        let head = MessageId::new(100_000_000_000_192);
        remove_persisted(channel_id, head);

        turn_view_reconciler::note_intake_queue_marker_added(
            &shared,
            &http,
            channel_id,
            head,
            61,
            queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
            "test_seed_anchor_queued",
        )
        .await;
        let ops_after_queue = shared.turn_view_reconciler.ops();
        assert_eq!(ops_after_queue.len(), 1);
        assert!(ops_after_queue[0].add && ops_after_queue[0].emoji == '📬');
        assert!(persisted_path(channel_id, head).exists());

        drain_dispatched_queue_markers(
            &shared,
            &http,
            channel_id,
            head,
            &[SourceMessageQueuedGeneration::new(head, 61)],
        )
        .await;

        assert_eq!(
            shared.turn_view_reconciler.ops().len(),
            ops_after_queue.len(),
            "dispatch drain must leave the anchor mailbox for the turn-start swap"
        );
        assert!(
            persisted_path(channel_id, head).exists(),
            "anchor queued state must survive until turn-start"
        );

        turn_view_reconciler::note_intake_turn_started(
            &shared,
            &http,
            channel_id,
            head,
            61,
            "test_dispatch_anchor_start",
        )
        .await;
        let ops = shared.turn_view_reconciler.ops();
        assert_eq!(ops.len(), 3);
        assert!(!ops[1].add && ops[1].emoji == '📬');
        assert!(ops[2].add && ops[2].emoji == '⏳');
    }

    #[tokio::test]
    async fn regression_4049_merged_non_anchor_cancel_cleans_persisted_state() {
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = Arc::new(serenity::Http::new("Bot test-token"));
        let channel_id = ChannelId::new(100_000_000_000_201);
        let source_a = MessageId::new(100_000_000_000_202);
        let head_b = MessageId::new(100_000_000_000_203);
        remove_persisted(channel_id, source_a);
        remove_persisted(channel_id, head_b);

        turn_view_reconciler::note_intake_queue_marker_added(
            &shared,
            &http,
            channel_id,
            source_a,
            62,
            queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
            "test_seed_merged_source_a",
        )
        .await;
        assert!(persisted_path(channel_id, source_a).exists());

        drain_dispatched_queue_markers(
            &shared,
            &http,
            channel_id,
            head_b,
            &[
                SourceMessageQueuedGeneration::new(source_a, 62),
                SourceMessageQueuedGeneration::new(head_b, 62),
            ],
        )
        .await;

        let ops = shared.turn_view_reconciler.ops();
        assert!(
            ops.iter()
                .any(|op| op.target.message_id == source_a && !op.add && op.emoji == '📬'),
            "merged non-anchor source must receive owner-aware queued cancel"
        );
        assert!(
            !persisted_path(channel_id, source_a).exists(),
            "merged non-anchor source cancel must remove persisted queued state"
        );
    }

    #[tokio::test]
    async fn regression_4049_cross_generation_merged_source_exit_clears_each_owner() {
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = Arc::new(serenity::Http::new("Bot test-token"));
        let channel_id = ChannelId::new(100_000_000_000_211);
        let source_a = MessageId::new(100_000_000_000_212);
        let source_b = MessageId::new(100_000_000_000_213);
        remove_persisted(channel_id, source_a);
        remove_persisted(channel_id, source_b);

        turn_view_reconciler::note_intake_queue_marker_added(
            &shared,
            &http,
            channel_id,
            source_a,
            71,
            queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
            "test_seed_cross_generation_source_a",
        )
        .await;
        turn_view_reconciler::note_intake_queue_marker_added(
            &shared,
            &http,
            channel_id,
            source_b,
            72,
            queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
            "test_seed_cross_generation_source_b",
        )
        .await;
        assert!(persisted_path(channel_id, source_a).exists());
        assert!(persisted_path(channel_id, source_b).exists());

        let intervention = crate::services::turn_orchestrator::Intervention {
            author_id: serenity::UserId::new(71),
            author_is_bot: false,
            message_id: source_b,
            queued_generation: 72,
            source_message_ids: vec![source_a, source_b],
            source_message_queued_generations: vec![
                SourceMessageQueuedGeneration::new(source_a, 71),
                SourceMessageQueuedGeneration::new(source_b, 72),
            ],
            source_text_segments: Vec::new(),
            text: "merged cross-generation".to_string(),
            mode: crate::services::turn_orchestrator::InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: true,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };
        let event = crate::services::turn_orchestrator::QueueExitEvent {
            intervention,
            kind: crate::services::turn_orchestrator::QueueExitKind::Cancelled,
        };
        drain_queue_exit_markers(&shared, &http, channel_id, &[&event]).await;

        let ops = shared.turn_view_reconciler.ops();
        assert!(
            ops.iter()
                .any(|op| op.target.message_id == source_a && !op.add && op.emoji == '📬'),
            "source A must be cleared with generation 71"
        );
        assert!(
            ops.iter()
                .any(|op| op.target.message_id == source_b && !op.add && op.emoji == '📬'),
            "source B must be cleared with generation 72"
        );
        assert!(
            !persisted_path(channel_id, source_a).exists(),
            "source A persisted queued state must be gone"
        );
        assert!(
            !persisted_path(channel_id, source_b).exists(),
            "source B persisted queued state must be gone"
        );
    }
}
