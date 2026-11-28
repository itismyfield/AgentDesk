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
    Arc::get_mut(&mut shared).unwrap().token_hash = "invalid\0path".to_string();
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
