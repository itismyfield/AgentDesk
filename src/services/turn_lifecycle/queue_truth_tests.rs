//! Cancel queue-truth tests that read the pending queue through ROOT health and DL shared data.
use crate::domain::tmux_cleanup::TmuxCleanupPolicy;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;

#[tokio::test]
async fn queue_truth_lost_actor_is_not_a_measured_empty_queue() {
    use crate::services::turn_orchestrator::{ChannelMailboxRegistry, QueuePersistenceContext};
    let temp = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let registry = crate::services::discord::health::HealthRegistry::new();
    registry.register("claude".into(), shared.clone()).await;
    let (mailboxes, token) = shared.queue_fixture_parts();
    for (i, drop_reply) in [false, true].into_iter().enumerate() {
        let channel = ChannelId::new(6038740 + i as u64);
        let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
            provider: Some(ProviderKind::Claude),
            channel_id: Some(channel),
            tmux_name: String::new(),
        };
        let handle = mailboxes.handle(channel);
        let item = ChannelMailboxRegistry::queued_for_test(42);
        handle
            .replace_queue(
                vec![item],
                QueuePersistenceContext::new(&ProviderKind::Claude, token, None),
            )
            .await;
        let pre =
            crate::services::turn_lifecycle::pending_queue_pre_snapshot(Some(&registry), &target)
                .await
                .unwrap();
        assert_eq!(pre.queue_depth, 1);
        if drop_reply {
            mailboxes.insert_reply_dropping_for_test(channel);
        } else {
            mailboxes.insert_unreachable_for_test(channel);
        }
        let post =
            crate::services::turn_lifecycle::pending_queue_post_snapshot(Some(&registry), &target)
                .await;
        mailboxes.remove_fixture_for_test(channel);
        assert!(post.is_none());
        assert_eq!(
            crate::services::turn_lifecycle::compute_queue_preserved(
                TmuxCleanupPolicy::PreserveSession,
                Some(&pre),
                post.as_ref()
            ),
            None
        );
        assert!(
            crate::services::turn_lifecycle::dropped_queue_message_ids(Some(&pre), post.as_ref())
                .is_empty()
        );
    }
}

#[test]
fn compute_queue_preserved_detects_disk_and_memory_loss() {
    use crate::services::discord::health::PendingQueueSnapshot;

    let pre = PendingQueueSnapshot {
        queue_depth: 1,
        disk_present: Some(true),
        disk_path: None,
        message_ids: vec![9_001],
    };
    let post_loss = PendingQueueSnapshot {
        queue_depth: 0,
        disk_present: Some(false),
        disk_path: None,
        message_ids: Vec::new(),
    };
    let lost_memory = PendingQueueSnapshot {
        queue_depth: 0,
        ..pre.clone()
    };
    let policy = TmuxCleanupPolicy::PreserveSession;
    assert_eq!(
        crate::services::turn_lifecycle::compute_queue_preserved(
            policy,
            Some(&pre),
            Some(&post_loss)
        ),
        Some(false)
    );
    assert_eq!(
        crate::services::turn_lifecycle::compute_queue_preserved(policy, Some(&pre), Some(&pre)),
        Some(true)
    );
    assert_eq!(
        crate::services::turn_lifecycle::compute_queue_preserved(
            policy,
            Some(&pre),
            Some(&lost_memory)
        ),
        Some(false)
    );
    let lost_disk = PendingQueueSnapshot {
        queue_depth: 1,
        ..post_loss.clone()
    };
    assert_eq!(
        crate::services::turn_lifecycle::compute_queue_preserved(
            policy,
            Some(&pre),
            Some(&lost_disk)
        ),
        Some(false)
    );
    let empty = PendingQueueSnapshot {
        disk_present: Some(false),
        ..Default::default()
    };
    assert_eq!(
        crate::services::turn_lifecycle::compute_queue_preserved(
            policy,
            Some(&empty),
            Some(&empty)
        ),
        Some(true)
    );
    for (pre, post) in [(None, None), (Some(&pre), None), (None, Some(&pre))] {
        assert_eq!(
            crate::services::turn_lifecycle::compute_queue_preserved(policy, pre, post),
            None
        );
    }
}

/// #5176: the cancel response must name the user instructions it destroyed.
/// Depth is not enough — a swapped queue keeps the same depth.
#[test]
fn dropped_queue_message_ids_names_the_lost_user_instructions() {
    use crate::services::discord::health::PendingQueueSnapshot;

    fn snapshot(message_ids: Vec<u64>) -> PendingQueueSnapshot {
        PendingQueueSnapshot {
            queue_depth: message_ids.len(),
            disk_present: Some(!message_ids.is_empty()),
            disk_path: None,
            message_ids,
        }
    }

    // The #5176 incident shape: queued_before=1, queued_remaining=0.
    assert_eq!(
        crate::services::turn_lifecycle::dropped_queue_message_ids(
            Some(&snapshot(vec![9_001])),
            Some(&snapshot(Vec::new())),
        ),
        vec![9_001]
    );

    // Same depth, different message: still a lost user instruction, and the
    // reason a depth comparison alone cannot audit the lossless contract.
    assert_eq!(
        crate::services::turn_lifecycle::dropped_queue_message_ids(
            Some(&snapshot(vec![9_001])),
            Some(&snapshot(vec![9_002])),
        ),
        vec![9_001]
    );

    // Preservation, and growth, report nothing.
    assert!(
        crate::services::turn_lifecycle::dropped_queue_message_ids(
            Some(&snapshot(vec![9_001])),
            Some(&snapshot(vec![9_001, 9_002])),
        )
        .is_empty()
    );

    // An unobservable side is not evidence of a drop.
    assert!(
        crate::services::turn_lifecycle::dropped_queue_message_ids(
            Some(&snapshot(vec![9_001])),
            None
        )
        .is_empty()
    );
    assert!(
        crate::services::turn_lifecycle::dropped_queue_message_ids::<PendingQueueSnapshot>(
            None, None
        )
        .is_empty()
    );
}

pub(crate) mod policy_observability {
    use std::sync::Arc;

    use poise::serenity_prelude::ChannelId;

    use crate::services::turn_lifecycle::{TurnLifecycleTarget, stop_turn_preserving_queue};

    #[tokio::test]
    async fn cancel_observability_emits_unknown_noop_direct_fallback() {
        let _ = crate::services::observability::events::test_capture::capture_async(
            cancel_observability_emits_unknown_noop_direct_fallback_scenario(|| {}),
        )
        .await;
    }

    pub(crate) async fn cancel_observability_emits_unknown_noop_direct_fallback_scenario(
        before_observe: impl FnOnce(),
    ) {
        let _guard = crate::services::observability::lock_env_then_runtime();
        crate::services::observability::reset_for_tests();
        crate::services::observability::init_observability(None);

        let target = TurnLifecycleTarget {
            provider: Some(crate::services::provider::ProviderKind::Codex),
            channel_id: None,
            tmux_name: format!(
                "AgentDesk-missing-noop-cancel-observability-{}",
                std::process::id()
            ),
        };
        let registry = None::<&crate::services::discord::health::HealthRegistry>;
        let result =
            stop_turn_preserving_queue(registry, &target, "queue-api cancel_turn (preserve)").await;
        assert_eq!(result.lifecycle_path, "direct-fallback");
        assert!(!result.tmux_killed);
        assert!(!result.inflight_cleared);
        assert_eq!(result.queue_depth, None);
        assert!(!result.termination_recorded);

        before_observe();
        assert_noop_cancel_event();
    }

    pub(crate) fn assert_noop_cancel_event() {
        let event = crate::services::observability::events::test_capture::one("turn_cancelled");
        assert_eq!(event.channel_id, None);
        assert_eq!(event.provider.as_deref(), Some("codex"));
        assert_eq!(event.payload["reason"], "queue-api cancel_turn (preserve)");
        assert_eq!(event.payload["surface"], "queue_cancel_preserve");
        assert_eq!(event.payload["lifecyclePath"], "direct-fallback");
        assert_eq!(event.payload["emittedNoOp"], true);
        assert!(event.payload["dispatch_id"].is_null());
        assert!(event.payload["session_key"].is_null());
        assert!(event.payload["turn_id"].is_null());
    }

    #[tokio::test]
    async fn queue_truth_disk_stat_error_is_unmeasured() {
        let temp = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        // An embedded NUL makes metadata lookup fail on every platform.
        Arc::get_mut(&mut shared)
            .unwrap()
            .set_token_hash_for_test("invalid\0path");
        let registry = crate::services::discord::health::HealthRegistry::new();
        registry.register("claude".into(), shared.clone()).await;
        let (mailboxes, token) = shared.queue_fixture_parts();
        let channel = ChannelId::new(6038742);
        let disk_path = crate::services::discord::runtime_store::discord_pending_queue_root()
            .unwrap()
            .join("claude")
            .join(token)
            .join(format!("{}.json", channel.get()));
        assert_eq!(
            disk_path.try_exists().unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        let observed = crate::services::discord::health::snapshot_pending_queue_state(
            &registry, "claude", channel,
        )
        .await
        .unwrap();
        mailboxes.remove_fixture_for_test(channel);
        assert_eq!(observed.disk_present, None);
    }
}
