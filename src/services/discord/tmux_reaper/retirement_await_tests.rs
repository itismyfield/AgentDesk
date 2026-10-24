use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::BoxFuture;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use tokio::sync::Notify;

use super::tests::{admit_any_host, seed_busy_channel, still_busy};
use crate::config::TestEnvVarGuard;
use crate::services::discord::health::test_support::fingerprint;
use crate::services::discord::inflight::{
    GuardedSaveOutcome, InflightTurnState, load_inflight_state,
    patch_bridge_entry_state_if_identity_unchanged, save_inflight_state,
    save_inflight_state_create_new,
};
use crate::services::provider::{CancelToken, ProviderKind};

/// A dead session seen by both probes is finalized after the second probe await.
async fn final_probe_case(periodic: bool, channel: u64) {
    let temp = tempfile::tempdir().unwrap();
    let _root =
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", temp.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.settings.write().await.provider = ProviderKind::Claude;
    let token = seed_busy_channel(&shared, channel).await;
    // The row and mailbox must name the same episode for guarded finalizer cleanup.
    let mut state = load_inflight_state(&ProviderKind::Claude, channel).unwrap();
    state.turn_nonce = token.turn_nonce().map(str::to_owned);
    save_inflight_state(&state).unwrap();
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let row = crate::services::discord::inflight::inflight_state_path(
        &root,
        &ProviderKind::Claude,
        channel,
    );
    assert!(fingerprint(&row).is_some());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = {
        let (entered, release, calls) = (entered.clone(), release.clone(), calls.clone());
        move |_: String| -> BoxFuture<'static, bool> {
            let (entered, release, calls) = (entered.clone(), release.clone(), calls.clone());
            Box::pin(async move {
                if calls.fetch_add(1, Ordering::Relaxed) == 1 {
                    entered.notify_one();
                    release.notified().await;
                }
                false
            })
        }
    };
    let work = async {
        if periodic {
            super::reap_stale_busy_mailboxes_with_probe(&shared, &probe, &admit_any_host).await;
        } else {
            let healed = super::heal_stale_busy_mailbox_with_probe(
                &shared,
                &ProviderKind::Claude,
                ChannelId::new(channel),
                "intake_await_test",
                false,
                &probe,
                &admit_any_host,
            )
            .await;
            assert!(healed);
        }
    };
    let transition = async {
        entered.notified().await;
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert!(still_busy(&shared, channel).await);
        release.notify_one();
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(work, transition);
    })
    .await
    .expect("final probe barrier");
    assert!(!still_busy(&shared, channel).await);
    assert!(token.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    assert!(
        fingerprint(&row).is_none(),
        "finalizer clears the legacy row"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn intake_finalizes_after_final_probe() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    final_probe_case(false, 6_325_412_002).await;
}

#[tokio::test(flavor = "current_thread")]
async fn periodic_finalizes_after_final_probe() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    final_probe_case(true, 6_325_412_004).await;
}

/// Builds the soft intervention intake queues while the channel is busy.
fn queued_prompt(message: MessageId) -> crate::services::turn_orchestrator::Intervention {
    use crate::services::turn_orchestrator::{
        Intervention, InterventionMode, SourceMessageQueuedGeneration,
    };
    let generation = crate::services::discord::runtime_store::process_generation();
    Intervention {
        author_id: UserId::new(11),
        author_is_bot: false,
        message_id: message,
        queued_generation: generation,
        source_message_ids: vec![message],
        source_message_queued_generations: vec![SourceMessageQueuedGeneration::user_instruction(
            message, generation,
        )],
        source_text_segments: Vec::new(),
        text: "next prompt".to_string(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

/// The next turn's intake from mailbox claim through row create and bridge-entry patch;
/// returns whether its row create met another turn's durable row.
async fn admit_next_turn(
    shared: &Arc<crate::services::discord::SharedData>,
    channel: ChannelId,
    message: MessageId,
) -> bool {
    let token = Arc::new(CancelToken::new());
    assert!(
        crate::services::discord::mailbox_try_start_turn(
            shared,
            channel,
            token.clone(),
            UserId::new(11),
            message
        )
        .await
    );
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        channel.get(),
        None,
        11,
        message.get(),
        0,
        "next prompt".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    state.turn_nonce = token.turn_nonce().map(str::to_owned);
    if save_inflight_state_create_new(&state).is_err() {
        return true;
    }
    let before = state.clone();
    let entry = patch_bridge_entry_state_if_identity_unchanged(&before, &mut state, "test_entry");
    assert_eq!(
        entry,
        GuardedSaveOutcome::Saved,
        "bridge entry owns its row"
    );
    false
}

/// The stale episode's row carries the mailbox nonce but a drifted finalizer id. No production
/// writer of this drift is known; the state is synthetic.
async fn drifted_row_case(periodic: bool, queued: bool, channel: u64) {
    let temp = tempfile::tempdir().unwrap();
    let _root =
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", temp.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.settings.write().await.provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(channel);
    let token = seed_busy_channel(&shared, channel).await;
    let mut state = load_inflight_state(&ProviderKind::Claude, channel).unwrap();
    state.turn_nonce = token.turn_nonce().map(str::to_owned);
    state.user_msg_id = channel * 10 + 1;
    save_inflight_state(&state).unwrap();
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let next = MessageId::new(channel * 10 + 2);
    if queued {
        let provider = ProviderKind::Claude;
        let enqueued = crate::services::discord::mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            queued_prompt(next),
        )
        .await;
        assert!(enqueued.enqueued);
    }
    // Kickoff stand-in: dequeue and admit on the first release or queue-eligible event.
    let mut events =
        crate::services::discord::turn_completion_events::subscribe_turn_completion_events(&shared);
    let kickoff = {
        let shared = shared.clone();
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if event.channel_id != channel_id {
                    continue;
                }
                let provider = ProviderKind::Claude;
                let taken = crate::services::discord::idle_queue_take_next_soft_if_ready(
                    &shared, &provider, channel_id,
                )
                .await;
                let Some((item, _, _lease)) = taken.into_intervention() else {
                    return None;
                };
                return Some(admit_next_turn(&shared, channel_id, item.message_id).await);
            }
            None
        })
    };
    let dead = |_: String| -> BoxFuture<'static, bool> { Box::pin(async { false }) };
    if periodic {
        super::reap_stale_busy_mailboxes_with_probe(&shared, &dead, &admit_any_host).await;
    } else {
        let healed = super::heal_stale_busy_mailbox_with_probe(
            &shared,
            &ProviderKind::Claude,
            channel_id,
            "intake_drift_test",
            false,
            &dead,
            &admit_any_host,
        )
        .await;
        assert!(healed);
    }
    let kicked = tokio::time::timeout(std::time::Duration::from_secs(10), kickoff)
        .await
        .expect("release event")
        .unwrap();
    assert!(!still_busy(&shared, channel).await || queued);
    assert!(token.cancelled.load(Ordering::Relaxed));
    if queued {
        assert_eq!(
            kicked,
            Some(false),
            "queued B starts once without a deferral"
        );
        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert!(snapshot.intervention_queue.is_empty());
        assert_eq!(snapshot.active_user_message_id, Some(next));
        let row = load_inflight_state(&ProviderKind::Claude, channel).expect("B keeps its row");
        assert_eq!(row.user_msg_id, next.get());
    } else {
        assert_eq!(kicked, None);
        assert!(
            load_inflight_state(&ProviderKind::Claude, channel).is_none(),
            "the finalizer retires the same episode's drifted row"
        );
        assert!(!admit_next_turn(&shared, channel_id, next).await);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn intake_heal_retires_a_drifted_same_episode_row() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    drifted_row_case(false, false, 6_484_201).await;
    drifted_row_case(false, true, 6_484_202).await;
}

#[tokio::test(flavor = "current_thread")]
async fn periodic_heal_retires_a_drifted_same_episode_row() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    drifted_row_case(true, false, 6_484_203).await;
    drifted_row_case(true, true, 6_484_204).await;
}

#[cfg(unix)]
mod host_gate_pg_tests;
