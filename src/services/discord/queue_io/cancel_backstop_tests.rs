use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::*;
use crate::services::discord::health::{HealthRegistry, schedule_pending_queue_drain_after_cancel};
use crate::services::discord::inflight::{
    InflightTurnState, inflight_runtime_root, inflight_state_path, save_inflight_state,
};
use crate::services::discord::zombie_foreground_release::tests::fixtures::missing_tmux_fixture;
use crate::services::provider::CancelToken;
use crate::services::turn_orchestrator::{Intervention, InterventionMode};

fn queued(message: MessageId) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: message,
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: vec![message],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "preserved source after cancellation".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

async fn yield_tasks() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancelled_anchor_is_rechecked_on_later_slow_backstop_cycle() {
    let root = tempfile::tempdir().expect("isolated runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _tmux = missing_tmux_fixture(&root);
    let channel = ChannelId::new(6_016_201_000);
    let provider = ProviderKind::Claude;
    let shared = make_shared_data_for_tests();
    let registry = HealthRegistry::new();
    registry.register("claude".into(), shared.clone()).await;
    let token = Arc::new(CancelToken::new());
    token.bind_unmanaged_session_name("AgentDesk-claude-cancel-backstop-terminal");
    assert!(
        shared
            .mailbox(channel)
            .try_start_turn(
                token.clone(),
                UserId::new(7),
                MessageId::new(channel.get() + 1)
            )
            .await
    );
    token.cancelled.store(true, Ordering::Relaxed);
    increment_global_active(&shared, "cancel_backstop_fixture");
    shared
        .mailbox(channel)
        .replace_queue(
            vec![queued(MessageId::new(channel.get() + 2))],
            queue_persistence_context(&shared, &provider, channel),
        )
        .await;
    let mut inflight = InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        7,
        channel.get() + 1,
        0,
        "source owner still holds its row".into(),
        None,
        token.tmux_session_name(),
        None,
        None,
        0,
    );
    inflight.turn_nonce = token.turn_nonce().map(str::to_owned);
    save_inflight_state(&inflight).expect("persist isolated source owner");
    let row_path = inflight_state_path(
        &inflight_runtime_root().expect("isolated inflight root"),
        &provider,
        channel.get(),
    );
    let row_before = std::fs::read(&row_path).unwrap();
    let kicks_after_release = Arc::new(AtomicUsize::new(0));
    let witnessed_kicks = kicks_after_release.clone();
    let _hook = set_idle_queue_kick_hook_for_tests(Arc::new(move |runtime, _, candidate, _| {
        let witnessed_kicks = witnessed_kicks.clone();
        Box::pin(async move {
            if candidate == channel
                && runtime
                    .mailbox(candidate)
                    .snapshot()
                    .await
                    .cancel_token
                    .is_none()
            {
                witnessed_kicks.fetch_add(1, Ordering::SeqCst);
            }
            None
        })
    }));

    let drain = schedule_pending_queue_drain_after_cancel(
        &registry,
        "claude",
        channel,
        "cancel_backstop_fixture",
    )
    .await;
    assert!(
        drain.scheduled,
        "the real cancellation drain owns the queued backlog"
    );
    assert_eq!(drain.queue_depth_after, Some(1));
    let queue_before = format!(
        "{:?}",
        shared.mailbox(channel).snapshot().await.intervention_queue
    );
    yield_tasks().await;
    tokio::time::advance(DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY).await;
    wait_observation("the real initial cancellation kickoff completed", || {
        support::backstop_waiting(channel)
    })
    .await;
    fire(channel).await;
    let held = shared.mailbox(channel).snapshot().await;
    assert!(
        held.cancel_token
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &token)),
        "the first slow cycle cannot bypass the inflight source owner"
    );
    assert_eq!(std::fs::read(&row_path).unwrap(), row_before);
    assert_eq!(format!("{:?}", held.intervention_queue), queue_before);
    assert_eq!(kicks_after_release.load(Ordering::SeqCst), 0);

    // Only the isolated source owner's record disappears; the measured pane is still missing.
    std::fs::remove_file(&row_path).expect("remove isolated source-owner fixture row");
    fire(channel).await;
    let after = shared.mailbox(channel).snapshot().await;
    assert!(
        after.cancel_token.is_none(),
        "a later slow cycle must re-evaluate and release the cancelled anchor after its inflight owner disappears"
    );
    assert_eq!(format!("{:?}", after.intervention_queue), queue_before);
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    assert!(
        kicks_after_release.load(Ordering::SeqCst) > 0,
        "the existing kickoff path is reached only after the guarded anchor release"
    );
    assert!(
        std::fs::read_to_string(root.path().join("tmux.calls"))
            .unwrap()
            .contains("has-session"),
        "terminal evidence came from a measured local tmux probe"
    );
}

use super::cancel_backstop_test_support as support;
use crate::services::discord::health::legacy_supervision::RetiredForTest;
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::discord::input_runtime::fence::{Gate, effect};
use crate::services::discord::turn_finalizer::{GuardedFinishResidue, TurnKey};
use crate::services::discord::zombie_foreground_release::cancel_backstop_test_support as release_support;
use std::time::Duration;

async fn wait_observation(label: &str, mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {label}"
        );
        tokio::task::yield_now().await;
    }
}

async fn wait_idle(shared: &Arc<SharedData>, channel: ChannelId) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while shared
        .mailbox(channel)
        .snapshot()
        .await
        .cancel_token
        .is_some()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for guarded mailbox release"
        );
        tokio::task::yield_now().await;
    }
}

async fn anchor_fixture(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    cancelled: bool,
    inflight: bool,
) -> (Arc<CancelToken>, Option<std::path::PathBuf>) {
    let token = Arc::new(CancelToken::new());
    token.bind_unmanaged_session_name(&format!("AgentDesk-claude-cancel-unit-{}", channel.get()));
    assert!(
        shared
            .mailbox(channel)
            .try_start_turn(
                token.clone(),
                UserId::new(7),
                MessageId::new(channel.get() + 1)
            )
            .await
    );
    token.cancelled.store(cancelled, Ordering::Relaxed);
    increment_global_active(shared, "cancel_backstop_unit_fixture");
    shared
        .mailbox(channel)
        .replace_queue(
            vec![queued(MessageId::new(channel.get() + 2))],
            queue_persistence_context(shared, &ProviderKind::Claude, channel),
        )
        .await;
    let row = if inflight {
        let mut row = InflightTurnState::new(
            ProviderKind::Claude,
            channel.get(),
            None,
            7,
            channel.get() + 1,
            0,
            "held source owner".into(),
            None,
            token.tmux_session_name(),
            None,
            None,
            0,
        );
        row.turn_nonce = token.turn_nonce().map(str::to_owned);
        save_inflight_state(&row).unwrap();
        Some(inflight_state_path(
            &inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            channel.get(),
        ))
    } else {
        None
    };
    (token, row)
}

async fn arm(shared: &Arc<SharedData>, channel: ChannelId) -> Arc<BackstopSlot> {
    assert!(
        arm_slow_idle_queue_backstop_if_queue_nonempty(
            shared,
            &ProviderKind::Claude,
            channel,
            "cancel_backstop_unit_fixture"
        )
        .await
    );
    wait_observation("backstop sleep registration", || {
        support::backstop_waiting(channel)
    })
    .await;
    shared
        .restart
        .deferred_hook_channels
        .get(&channel)
        .expect("one armed slot")
        .clone()
}

async fn fire(channel: ChannelId) {
    let before = support::completed_fires(channel);
    tokio::time::advance(DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY).await;
    wait_observation("actual slow cycle completion", || {
        support::completed_fires(channel) > before
    })
    .await;
}

async fn assert_owner_kept(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    token: &Arc<CancelToken>,
    queue: &str,
) {
    let snapshot = shared.mailbox(channel).snapshot().await;
    assert!(
        snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, token))
    );
    assert_eq!(
        snapshot.active_user_message_id,
        Some(MessageId::new(channel.get() + 1))
    );
    assert_eq!(format!("{:?}", snapshot.intervention_queue), queue);
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
}

fn record_kicks(channel: ChannelId) -> (Arc<AtomicUsize>, IdleQueueKickHookResetForTests) {
    let count = Arc::new(AtomicUsize::new(0));
    let captured = count.clone();
    let hook = set_idle_queue_kick_hook_for_tests(Arc::new(move |_, _, candidate, _| {
        let count = captured.clone();
        Box::pin(async move {
            if candidate == channel {
                count.fetch_add(1, Ordering::SeqCst);
            }
            None
        })
    }));
    (count, hook)
}

async fn routing_http(
    channel: ChannelId,
) -> (
    Arc<AtomicUsize>,
    crate::services::discord::shared_state::test_rest::Guard,
    tokio::task::JoinHandle<()>,
) {
    let requests = Arc::new(AtomicUsize::new(0));
    let captured = requests.clone();
    let app = axum::Router::new().fallback(move |method: axum::http::Method, uri: axum::http::Uri| {
        captured.fetch_add(1, Ordering::SeqCst);
        async move {
            assert_eq!(method, axum::http::Method::GET, "the live guard must not post queued input");
            assert!(uri.path().starts_with("/api/v10/channels/"));
            axum::Json(serde_json::json!({
                "id": channel.get().to_string(), "type": 0, "name": "queue-claude", "guild_id": "6016200",
                "position": 0, "permission_overwrites": [], "nsfw": false, "parent_id": null,
            }))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::HttpBuilder::new("cancel-test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let rest = crate::services::discord::shared_state::test_rest::install(http);
    (requests, rest, server)
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn live_foreground_consumes_its_real_guard_skip_request_without_rearming() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(6_016_202_100);
    let (token, _) = anchor_fixture(&shared, channel, false, false).await;
    shared.settings.write().await.allowed_channel_ids = vec![channel.get()];
    shared
        .http
        .cached_bot_token
        .set("cancel-test-token".into())
        .unwrap();
    let (requests, _rest, server) = routing_http(channel).await;
    let queue_before = format!(
        "{:?}",
        shared.mailbox(channel).snapshot().await.intervention_queue
    );
    schedule_deferred_idle_queue_kickoff_immediate(
        shared.clone(),
        ProviderKind::Claude,
        channel,
        "real_guard_skip_fixture",
    );
    wait_observation("real initial kickoff completed before slow sleep", || {
        support::backstop_waiting(channel)
    })
    .await;
    assert!(
        requests.load(Ordering::SeqCst) > 0,
        "actual Discord REST routing precedes the mailbox guard"
    );
    let slot = shared
        .restart
        .deferred_hook_channels
        .get(&channel)
        .unwrap()
        .clone();
    assert!(
        slot.pending_request.load(Ordering::Acquire),
        "actual guard-skip armed the occupied slot"
    );
    let fired_before = support::completed_fires(channel);
    fire(channel).await;
    wait_observation("live completion owner retires the slow task", || {
        !shared.restart.deferred_hook_channels.contains_key(&channel)
    })
    .await;
    assert!(
        !slot.pending_request.load(Ordering::Acquire),
        "the real guard's own arm request was evaluated once"
    );
    assert_owner_kept(&shared, channel, &token, &queue_before).await;
    assert!(!token.cancelled.load(Ordering::Relaxed));
    assert_eq!(
        shared.restart.deferred_hook_backlog.load(Ordering::Relaxed),
        0
    );
    let requests_after = requests.load(Ordering::SeqCst);
    for _ in 0..5 {
        tokio::time::advance(DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY).await;
        yield_tasks().await;
    }
    assert_eq!(
        support::completed_fires(channel),
        fired_before + 1,
        "a self-request cannot create a live-turn retry loop"
    );
    assert_eq!(requests.load(Ordering::SeqCst), requests_after);
    server.abort();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancelled_live_inflight_is_preserved_for_five_actual_slow_cycles() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _tmux = missing_tmux_fixture(&root);
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(6_016_202_200);
    let (token, row) = anchor_fixture(&shared, channel, true, true).await;
    let row = row.unwrap();
    let row_before = std::fs::read(&row).unwrap();
    let queue_before = format!(
        "{:?}",
        shared.mailbox(channel).snapshot().await.intervention_queue
    );
    let (kicks, _hook) = record_kicks(channel);
    let slot = arm(&shared, channel).await;
    let before = support::completed_fires(channel);
    for index in 1..=5 {
        fire(channel).await;
        wait_observation("held source owner retained its slow retry", || {
            support::backstop_waiting(channel)
        })
        .await;
        assert_eq!(support::completed_fires(channel), before + index);
        assert_owner_kept(&shared, channel, &token, &queue_before).await;
        assert_eq!(std::fs::read(&row).unwrap(), row_before);
        assert!(Arc::ptr_eq(
            shared
                .restart
                .deferred_hook_channels
                .get(&channel)
                .unwrap()
                .value(),
            &slot
        ));
        assert_eq!(
            shared.restart.deferred_hook_backlog.load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            kicks.load(Ordering::SeqCst),
            0,
            "a retained inflight owner cannot reach kickoff"
        );
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn matching_residue_keeps_release_priority_over_the_cancel_backstop() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _tmux = missing_tmux_fixture(&root);
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(6_016_205_000);
    let (token, _) = anchor_fixture(&shared, channel, true, false).await;
    let snapshot = shared.mailbox(channel).snapshot().await;
    let queue_before = format!("{:?}", snapshot.intervention_queue);
    let residue = GuardedFinishResidue {
        expected_user_msg_id: channel.get() + 1,
        active_user_msg_id: channel.get() + 1,
        generation: 0,
        provider: ProviderKind::Claude,
        terminal_turn_nonce: snapshot.active_turn_nonce.clone(),
        active_turn_nonce: snapshot.active_turn_nonce.clone(),
        observed_before: std::time::Instant::now(),
        allow_completion_cleanup: false,
        drain_voice: false,
        terminal_was_cancel: true,
    };
    assert!(residue.matches_observed_owner(&snapshot));
    shared
        .turn_finalizer
        .guarded_finish_residues()
        .insert(channel, residue);
    let (kicks, _hook) = record_kicks(channel);
    arm(&shared, channel).await;
    fire(channel).await;
    assert_owner_kept(&shared, channel, &token, &queue_before).await;
    assert!(
        shared
            .turn_finalizer
            .guarded_finish_residues()
            .contains_key(&channel)
    );
    assert_eq!(kicks.load(Ordering::SeqCst), 0);
    assert!(
        !root.path().join("tmux.calls").exists(),
        "matching residue is judged before zombie terminal probes"
    );

    shared.turn_finalizer.register_start(
        TurnKey::new(channel, channel.get() + 1, 0).with_episode_nonce(token.turn_nonce()),
        ProviderKind::Claude,
        RelayOwnerKind::None,
        &shared,
    );
    yield_tasks().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_idle(&shared, channel).await;
    wait_observation("existing reconciler consumed its matching residue", || {
        !shared
            .turn_finalizer
            .guarded_finish_residues()
            .contains_key(&channel)
    })
    .await;
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    assert_eq!(
        format!(
            "{:?}",
            shared.mailbox(channel).snapshot().await.intervention_queue
        ),
        queue_before
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn matching_residue_inserted_after_terminal_evidence_preserves_its_owner() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _tmux = missing_tmux_fixture(&root);
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(6_016_205_100);
    let (token, _) = anchor_fixture(&shared, channel, true, false).await;
    let queue_before = format!(
        "{:?}",
        shared.mailbox(channel).snapshot().await.intervention_queue
    );
    assert!(
        !shared
            .turn_finalizer
            .guarded_finish_residues()
            .contains_key(&channel),
        "no earlier residue check can mask the mutation boundary"
    );
    let barrier = release_support::install_release_pause(channel);
    let (kicks, _hook) = record_kicks(channel);
    let slot = arm(&shared, channel).await;
    let before = support::completed_fires(channel);
    tokio::time::advance(DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY).await;
    barrier.wait(Duration::from_secs(10)).await;
    assert!(
        std::fs::read_to_string(root.path().join("tmux.calls"))
            .unwrap()
            .contains("has-session"),
        "the residue arrives only after the actual terminal Release evidence"
    );
    let observed = shared.mailbox(channel).snapshot().await;
    assert!(
        observed
            .cancel_token
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &token))
    );
    let residue = GuardedFinishResidue {
        expected_user_msg_id: channel.get() + 1,
        active_user_msg_id: channel.get() + 1,
        generation: 0,
        provider: ProviderKind::Claude,
        terminal_turn_nonce: observed.active_turn_nonce.clone(),
        active_turn_nonce: observed.active_turn_nonce.clone(),
        observed_before: std::time::Instant::now(),
        allow_completion_cleanup: false,
        drain_voice: false,
        terminal_was_cancel: true,
    };
    assert!(residue.matches_observed_owner(&observed));
    let residue_before = format!("{residue:?}");
    shared
        .turn_finalizer
        .guarded_finish_residues()
        .insert(channel, residue);
    barrier.release();
    wait_observation("late-residue candidate completed", || {
        support::completed_fires(channel) > before
    })
    .await;
    wait_observation("late residue retained the recovery owner", || {
        support::backstop_waiting(channel)
    })
    .await;
    assert_owner_kept(&shared, channel, &token, &queue_before).await;
    assert_eq!(release_support::finish_status(channel), None);
    assert_eq!(kicks.load(Ordering::SeqCst), 0);
    assert_eq!(
        format!(
            "{:?}",
            shared
                .turn_finalizer
                .guarded_finish_residues()
                .get(&channel)
                .expect("the existing residue retains release responsibility")
                .value()
        ),
        residue_before
    );
    assert!(Arc::ptr_eq(
        shared
            .restart
            .deferred_hook_channels
            .get(&channel)
            .unwrap()
            .value(),
        &slot
    ));
    assert_eq!(
        shared.restart.deferred_hook_backlog.load(Ordering::Relaxed),
        1
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn backstop_refuses_an_already_closing_or_retired_channel() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _tmux = missing_tmux_fixture(&root);
    for (index, closing) in [true, false].into_iter().enumerate() {
        let shared = make_shared_data_for_tests();
        let channel = ChannelId::new(6_016_213_100 + index as u64);
        let (token, _) = anchor_fixture(&shared, channel, true, false).await;
        let queue_before = format!(
            "{:?}",
            shared.mailbox(channel).snapshot().await.intervention_queue
        );
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _closing = closing.then(|| gate.close().unwrap());
        let _retired = (!closing).then(|| RetiredForTest::new("claude", channel.get()));
        let (kicks, _hook) = record_kicks(channel);
        arm(&shared, channel).await;
        fire(channel).await;
        assert_owner_kept(&shared, channel, &token, &queue_before).await;
        assert_eq!(kicks.load(Ordering::SeqCst), 0);
        assert!(
            !root.path().join("tmux.calls").exists(),
            "closing/retired channels cannot reach terminal probing"
        );
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn handoff_before_backstop_admission_preserves_the_successor_owner() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _tmux = missing_tmux_fixture(&root);
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(6_016_213_200);
    let (a, _) = anchor_fixture(&shared, channel, true, false).await;
    let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
    let fixture_permit = gate.admit().unwrap();
    let barrier = support::BeforeAdmitRace::install(channel);
    arm(&shared, channel).await;
    tokio::time::advance(DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY).await;
    assert!(
        barrier.wait(Duration::from_secs(10)).await,
        "the real candidate reached the pre-admission barrier"
    );
    let closing = gate.close().unwrap();
    let b = effect::scope(Some(fixture_permit), async {
        let finished = crate::services::discord::mailbox_finish::mailbox_finish_judged_turn(
            &shared,
            &ProviderKind::Claude,
            channel,
            Some(&a),
            crate::services::discord::mailbox_finish::MailboxLookup::Peek,
        )
        .await;
        assert!(matches!(
            finished,
            crate::services::turn_orchestrator::TokenFinish::Finished(_)
        ));
        saturating_decrement_global_active(&shared);
        let b = Arc::new(CancelToken::new());
        assert!(
            shared
                .mailbox(channel)
                .try_start_turn(b.clone(), UserId::new(7), MessageId::new(channel.get() + 1))
                .await
        );
        increment_global_active(&shared, "handoff_successor_fixture");
        b
    })
    .await;
    let registered = shared.mailbox(channel).snapshot().await;
    assert_eq!(registered.intervention_queue.len(), 1);
    assert_eq!(
        registered.intervention_queue[0].source_message_ids,
        vec![MessageId::new(channel.get() + 2)]
    );
    assert_eq!(
        registered.intervention_queue[0].text,
        "preserved source after cancellation"
    );
    let queue_before = format!("{:?}", registered.intervention_queue);
    let queue_path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .expect("isolated queue root")
        .join(ProviderKind::Claude.as_str())
        .join(&shared.token_hash)
        .join(format!("{}.json", channel.get()));
    let queue_bytes_before = std::fs::read(&queue_path).unwrap();
    let before = support::completed_fires(channel);
    barrier.release();
    wait_observation("admission refusal cycle completed", || {
        support::completed_fires(channel) > before
    })
    .await;
    assert_owner_kept(&shared, channel, &b, &queue_before).await;
    assert_eq!(std::fs::read(&queue_path).unwrap(), queue_bytes_before);
    assert!(!b.cancelled.load(Ordering::Relaxed));
    assert!(
        !root.path().join("tmux.calls").exists(),
        "handoff closed admission before any terminal probe"
    );
    closing.drain().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn admitted_backstop_holds_its_permit_through_evidence_and_mutation() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _tmux = missing_tmux_fixture(&root);
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(6_016_213_300);
    let (_, _) = anchor_fixture(&shared, channel, true, false).await;
    let queue_before = format!(
        "{:?}",
        shared.mailbox(channel).snapshot().await.intervention_queue
    );
    let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
    let barrier = release_support::install_release_pause(channel);
    let (_kicks, _hook) = record_kicks(channel);
    arm(&shared, channel).await;
    let before = support::completed_fires(channel);
    tokio::time::advance(DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY).await;
    barrier.wait(Duration::from_secs(10)).await;
    assert!(
        std::fs::read_to_string(root.path().join("tmux.calls"))
            .unwrap()
            .contains("has-session"),
        "the admitted candidate measured terminal evidence before handoff"
    );
    let closing = gate.close().unwrap();
    let drain = closing.drain();
    tokio::pin!(drain);
    assert!(
        futures::poll!(drain.as_mut()).is_pending(),
        "handoff must wait for the admitted evidence-to-mutation effect"
    );
    barrier.release();
    wait_observation("admitted effect completed after handoff", || {
        support::completed_fires(channel) > before
    })
    .await;
    drain.await;
    let after = shared.mailbox(channel).snapshot().await;
    assert!(
        after.cancel_token.is_none(),
        "the already-admitted permit remains valid in Closing through exact-token finish"
    );
    assert_eq!(format!("{:?}", after.intervention_queue), queue_before);
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn retirement_after_terminal_evidence_prevents_backstop_mutation() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _tmux = missing_tmux_fixture(&root);
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(6_016_213_400);
    let (token, _) = anchor_fixture(&shared, channel, true, false).await;
    let queue_before = format!(
        "{:?}",
        shared.mailbox(channel).snapshot().await.intervention_queue
    );
    let _gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
    let barrier = release_support::install_release_pause(channel);
    let (kicks, _hook) = record_kicks(channel);
    arm(&shared, channel).await;
    let before = support::completed_fires(channel);
    tokio::time::advance(DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY).await;
    barrier.wait(Duration::from_secs(10)).await;
    assert!(
        std::fs::read_to_string(root.path().join("tmux.calls"))
            .unwrap()
            .contains("has-session"),
        "retirement is introduced after the actual Release evidence"
    );
    let _retired = RetiredForTest::new("claude", channel.get());
    barrier.release();
    wait_observation("retired evidence candidate completed", || {
        support::completed_fires(channel) > before
    })
    .await;
    assert_owner_kept(&shared, channel, &token, &queue_before).await;
    assert_eq!(kicks.load(Ordering::SeqCst), 0);
    assert!(
        shared.restart.deferred_hook_channels.contains_key(&channel),
        "the denied mutation preserves the recovery request alongside its source queue"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn external_arm_at_shutdown_is_inherited_by_the_original_backstop_task() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(6_016_215_000);
    let (token, _) = anchor_fixture(&shared, channel, false, false).await;
    let queue_before = format!(
        "{:?}",
        shared.mailbox(channel).snapshot().await.intervention_queue
    );
    let (kicks, _hook) = record_kicks(channel);
    let barrier = support::ShutdownRace::install(channel);
    let slot = arm(&shared, channel).await;
    tokio::time::advance(DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY).await;
    assert!(
        barrier.wait(Duration::from_secs(10)).await,
        "the live-owner cycle reached its actual shutdown boundary"
    );
    let before = support::completed_fires(channel);
    assert_eq!(before, 1);
    assert!(!slot.pending_request.load(Ordering::Acquire));
    let finished = crate::services::discord::mailbox_finish::mailbox_finish_judged_turn(
        &shared,
        &ProviderKind::Claude,
        channel,
        Some(&token),
        crate::services::discord::mailbox_finish::MailboxLookup::Peek,
    )
    .await;
    assert!(matches!(
        finished,
        crate::services::turn_orchestrator::TokenFinish::Finished(_)
    ));
    saturating_decrement_global_active(&shared);
    assert!(
        !arm_slow_idle_queue_backstop_if_queue_nonempty(
            &shared,
            &ProviderKind::Claude,
            channel,
            "external_shutdown_arm_fixture"
        )
        .await,
        "external arm coalesces into the task that has not shut down yet"
    );
    assert!(slot.pending_request.load(Ordering::Acquire));
    barrier.release();
    wait_observation(
        "inherited request evaluated without another 60s delay",
        || support::completed_fires(channel) > before,
    )
    .await;
    wait_observation("inherited request retained queued work", || {
        support::backstop_waiting(channel)
    })
    .await;
    assert!(Arc::ptr_eq(
        shared
            .restart
            .deferred_hook_channels
            .get(&channel)
            .unwrap()
            .value(),
        &slot
    ));
    assert!(!slot.pending_request.load(Ordering::Acquire));
    assert_eq!(
        shared.restart.deferred_hook_backlog.load(Ordering::Relaxed),
        1
    );
    assert!(kicks.load(Ordering::SeqCst) > 0);
    assert_eq!(
        format!(
            "{:?}",
            shared.mailbox(channel).snapshot().await.intervention_queue
        ),
        queue_before
    );
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);

    shared
        .mailbox(channel)
        .replace_queue(
            Vec::new(),
            queue_persistence_context(&shared, &ProviderKind::Claude, channel),
        )
        .await;
    slot.wake.notify_one();
    wait_observation("empty queue retired original task", || {
        !shared.restart.deferred_hook_channels.contains_key(&channel)
    })
    .await;
    assert_eq!(
        shared.restart.deferred_hook_backlog.load(Ordering::Relaxed),
        0
    );
}
