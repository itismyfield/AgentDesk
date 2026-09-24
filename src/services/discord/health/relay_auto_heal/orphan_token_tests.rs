use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::super::HealthRegistry;
use crate::config::TestEnvVarGuard;
use crate::services::discord::SharedData;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};

const PAST_ADMISSION_GRACE: Duration = Duration::from_secs(31);

fn queued(message_id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(message_id),
        queued_generation: crate::services::discord::runtime_store::load_generation(),
        source_message_ids: vec![MessageId::new(message_id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("queued behind orphan token {message_id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

/// A rowless, watcherless mailbox anchor aged past the admission grace with
/// three queued messages behind it — the orphan-token sweep's target shape.
async fn seed_orphan_with_queue(
    provider: &ProviderKind,
    channel: ChannelId,
    anchor: MessageId,
    tmux_session: Option<&str>,
) -> (HealthRegistry, Arc<SharedData>, Arc<CancelToken>) {
    let registry = HealthRegistry::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;
    let token = Arc::new(CancelToken::new());
    if let Some(name) = tmux_session {
        token.bind_unmanaged_session_name(name);
    }
    assert!(
        crate::services::discord::mailbox_try_start_turn(
            &shared,
            channel,
            token.clone(),
            UserId::new(7),
            anchor,
        )
        .await
    );
    for id in 1..=3 {
        crate::services::discord::mailbox_enqueue_intervention(
            &shared,
            provider,
            channel,
            queued(anchor.get() + id),
        )
        .await;
    }
    shared
        .mailbox(channel)
        .age_active_turn_for_test(PAST_ADMISSION_GRACE)
        .await;
    shared.restart.global_active.store(1, Ordering::Relaxed);
    (registry, shared, token)
}

async fn assert_anchor_and_queue_kept(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    anchor: MessageId,
    token: &Arc<CancelToken>,
) {
    let after = crate::services::discord::mailbox_snapshot(shared, channel).await;
    assert!(
        after
            .cancel_token
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, token)),
        "an unwitnessed orphan token must keep its anchor"
    );
    assert_eq!(after.active_user_message_id, Some(anchor));
    assert_eq!(
        after.intervention_queue.len(),
        3,
        "the queued messages behind the anchor must survive the sweep"
    );
    assert!(!token.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
}

fn i20_refusals(
    channel: ChannelId,
) -> Vec<crate::services::observability::events::StructuredEvent> {
    crate::services::observability::events::recent(500)
        .into_iter()
        .filter(|event| event.event_type == "invariant_violation")
        .filter(|event| event.channel_id == Some(channel.get()))
        .filter(|event| {
            event.payload["invariant"]
                == crate::services::observability::LIVE_TURN_PROVEN_BY_PROGRESS_INVARIANT
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unmeasured_orphan_token_keeps_anchor_and_queue_and_is_graded_once() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().expect("runtime root");
    let _env =
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
    let provider = ProviderKind::Codex;
    let channel = ChannelId::new(5_996_101);
    let anchor = MessageId::new(5_996_110);
    let (registry, shared, token) = seed_orphan_with_queue(&provider, channel, anchor, None).await;

    for _ in 0..2 {
        super::run_orphan_token_auto_heal_pass(&registry, &provider, &[shared.clone()]).await;
    }

    assert_anchor_and_queue_kept(&shared, channel, anchor, &token).await;
    let refusals = i20_refusals(channel);
    assert_eq!(
        refusals.len(),
        1,
        "one wedged episode is graded once across ticks: {refusals:?}"
    );
    let details = &refusals[0].payload["details"];
    assert_eq!(
        details["refused_reason"],
        "orphan_token_producer_liveness_unmeasured"
    );
    assert_eq!(details["retired"], false);
    assert_eq!(details["queue_depth"], 3);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirmed_dead_rowless_orphan_token_still_needs_a_warrant() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    if !crate::services::platform::tmux::is_available() {
        eprintln!("skipping confirmed-dead orphan sweep: tmux unavailable");
        return;
    }
    let root = tempfile::tempdir().expect("runtime root");
    let _env =
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
    let provider = ProviderKind::Codex;
    let channel = ChannelId::new(5_996_201);
    let anchor = MessageId::new(5_996_210);
    let dead_session = format!("plain-shell-5996-dead-{}", std::process::id());
    let (registry, shared, token) =
        seed_orphan_with_queue(&provider, channel, anchor, Some(&dead_session)).await;
    let watcher_state = registry
        .snapshot_watcher_state_for_shared(&provider, shared.clone(), channel.get())
        .await
        .expect("orphan snapshot");
    assert_eq!(
        watcher_state.relay_health.tmux_alive,
        Some(false),
        "fixture must be the measured-death shape"
    );

    super::run_orphan_token_auto_heal_pass(&registry, &provider, &[shared.clone()]).await;

    assert_anchor_and_queue_kept(&shared, channel, anchor, &token).await;
    let refusals = i20_refusals(channel);
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(
        refusals[0].payload["details"]["refused_reason"],
        "axis_b_orphan_token_reachability_unobserved"
    );
}
