use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use poise::serenity_prelude::{ChannelId, HttpBuilder, MessageId, UserId};
use tokio::sync::Notify;

use crate::services::discord;
use crate::services::provider::ProviderKind;
use discord::health::legacy_supervision::RetiredForTest;
use discord::health::legacy_supervision::test_support::{MockDiscord, fingerprint, message_json};
use discord::inflight::InflightTurnState;
use discord::turn_finalizer::tests::with_isolated_runtime_root;

#[derive(Clone)]
struct HeldAwait {
    site: &'static str,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    dispatch_failures: Arc<AtomicUsize>,
}

impl HeldAwait {
    fn new(site: &'static str) -> Self {
        Self {
            site,
            entered: Arc::default(),
            release: Arc::default(),
            dispatch_failures: Arc::default(),
        }
    }

    async fn hold(&self) {
        self.entered.notify_one();
        self.release.notified().await;
    }
}

tokio::task_local! {
    static HELD: HeldAwait;
}

pub(super) async fn checkpoint(site: &'static str) {
    if let Ok(held) = HELD.try_with(Clone::clone)
        && held.site == site
    {
        held.hold().await;
    }
}

// The dispatch reducer is replaced only inside this task-local fixture; no live DB is used.
pub(super) fn record_dispatch_failure() -> bool {
    HELD.try_with(|held| {
        held.dispatch_failures.fetch_add(1, Ordering::Relaxed);
    })
    .is_ok()
}

fn row(channel: u64, with_dispatch: bool, with_notice: bool) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Codex,
        channel,
        Some(format!("retirement-await-{channel}")),
        1,
        channel * 10,
        if with_notice { channel * 10 + 1 } else { 0 },
        "restart me".into(),
        Some("preserved-provider-session".into()),
        Some(format!("AgentDesk-codex-retirement-await-{channel}")),
        None,
        None,
        0,
    );
    state.dispatch_id = with_dispatch.then(|| format!("retirement-dispatch-{channel}"));
    state
}

async fn state_case(site: &'static str, channel: u64, retire: bool) {
    let shared = discord::make_shared_data_for_tests();
    let discord = MockDiscord::start().await;
    let channel_id = ChannelId::new(channel);
    let state = row(channel, site == "dispatch_lookup", false);
    discord::inflight::save_inflight_state(&state).unwrap();
    let root = discord::runtime_store::discord_inflight_root().unwrap();
    let path = discord::inflight::inflight_state_path(&root, &ProviderKind::Codex, channel);
    let before = fingerprint(&path);
    let held = HeldAwait::new(site);
    let operation = HELD.scope(
        held.clone(),
        super::start_restart_handoff_from_state(
            channel_id,
            &discord.http,
            &shared,
            &ProviderKind::Codex,
            state,
            "",
        ),
    );
    tokio::pin!(operation);
    tokio::select! {
        _ = held.entered.notified() => {},
        result = &mut operation => panic!("handoff escaped held await: {result}"),
    }
    let lock = if site == "core_lock" {
        Some(shared.core.lock().await)
    } else {
        None
    };
    held.release.notify_one();
    if lock.is_some() {
        assert!(
            futures::poll!(&mut operation).is_pending(),
            "core lock must actually block"
        );
    }
    let _retired = retire.then(|| RetiredForTest::new("codex", channel));
    drop(lock);
    assert_eq!(operation.await, !retire);
    assert_eq!(
        held.dispatch_failures.load(Ordering::Relaxed),
        usize::from(!retire && site == "dispatch_lookup")
    );
    let core = shared.core.lock().await;
    assert_eq!(core.sessions.contains_key(&channel_id), !retire);
    drop(core);
    if retire {
        assert!(before.is_some());
        assert_eq!(
            fingerprint(&path),
            before,
            "retired handoff preserves the durable row"
        );
    } else {
        assert!(
            fingerprint(&path).is_none(),
            "empty-set control clears the row"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn retirement_during_handoff_dispatch_lookup_blocks_failure() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    with_isolated_runtime_root(|| async {
        state_case("dispatch_lookup", 6_325_512_101, true).await;
        state_case("dispatch_lookup", 6_325_512_102, false).await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn retirement_during_handoff_core_lock_preserves_metadata_and_row() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    with_isolated_runtime_root(|| async {
        state_case("core_lock", 6_325_512_111, true).await;
        state_case("core_lock", 6_325_512_112, false).await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn retirement_during_handoff_rowless_snapshot_blocks_kickoff() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    with_isolated_runtime_root(|| async {
        for (channel, retire) in [(6_325_512_121, true), (6_325_512_122, false)] {
            let shared = discord::make_shared_data_for_tests();
            let discord = MockDiscord::start().await;
            let channel_id = ChannelId::new(channel);
            let message = MessageId::new(channel * 10);
            let queued = crate::services::turn_orchestrator::Intervention {
                author_id: UserId::new(1),
                author_is_bot: false,
                message_id: message,
                queued_generation: shared.restart.current_generation,
                source_message_ids: vec![message],
                source_message_queued_generations: Vec::new(),
                source_text_segments: Vec::new(),
                text: "resume queued input".into(),
                mode: crate::services::turn_orchestrator::InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            };
            discord::queue_io::with_post_enqueue_idle_queue_kick_suppressed(
                discord::mailbox_enqueue_intervention(
                    &shared,
                    &ProviderKind::Codex,
                    channel_id,
                    queued,
                ),
            )
            .await;
            assert!(
                !shared
                    .restart
                    .deferred_hook_channels
                    .contains_key(&channel_id)
            );
            let held = HeldAwait::new("rowless_snapshot");
            let session = format!("AgentDesk-codex-retirement-await-{channel}");
            let operation = HELD.scope(
                held.clone(),
                super::resume_aborted_restart_turn(
                    channel_id,
                    &discord.http,
                    &shared,
                    &session,
                    "",
                ),
            );
            tokio::pin!(operation);
            tokio::select! {
                _ = held.entered.notified() => {},
                result = &mut operation => panic!("rowless handoff escaped held await: {result}"),
            }
            let _retired = retire.then(|| RetiredForTest::new("codex", channel));
            held.release.notify_one();
            assert!(!operation.await);
            assert_eq!(
                shared
                    .restart
                    .deferred_hook_channels
                    .contains_key(&channel_id),
                !retire
            );
            assert_eq!(
                shared.restart.deferred_hook_backlog.load(Ordering::Relaxed),
                usize::from(!retire)
            );
            assert_eq!(
                discord::mailbox_snapshot(&shared, channel_id)
                    .await
                    .intervention_queue
                    .len(),
                1
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn retirement_during_handoff_notice_preserves_later_effects() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    with_isolated_runtime_root(|| async {
        for (channel, retire) in [(6_325_512_131, true), (6_325_512_132, false)] {
            let shared = discord::make_shared_data_for_tests();
            let held = HeldAwait::new("notice");
            let http_held = held.clone();
            let notices = Arc::new(AtomicUsize::new(0));
            let received = notices.clone();
            let app = axum::Router::new().fallback(axum::routing::any(move || {
                let (http_held, received) = (http_held.clone(), received.clone());
                async move {
                    received.fetch_add(1, Ordering::Relaxed);
                    http_held.hold().await;
                    axum::Json(message_json(
                        channel * 10 + 1,
                        channel,
                        1,
                        "",
                        "2026-10-02T00:00:00+00:00",
                    ))
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let http = Arc::new(
                HttpBuilder::new("test-token")
                    .proxy(format!("http://127.0.0.1:{port}"))
                    .ratelimiter_disabled(true)
                    .build(),
            );
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let state = row(channel, true, true);
            discord::inflight::save_inflight_state(&state).unwrap();
            let root = discord::runtime_store::discord_inflight_root().unwrap();
            let path = discord::inflight::inflight_state_path(&root, &ProviderKind::Codex, channel);
            let before = fingerprint(&path);
            let operation = HELD.scope(
                held.clone(),
                super::start_restart_handoff_from_state(
                    ChannelId::new(channel),
                    &http,
                    &shared,
                    &ProviderKind::Codex,
                    state,
                    "",
                ),
            );
            tokio::pin!(operation);
            tokio::select! {
                _ = held.entered.notified() => {},
                result = &mut operation => panic!("notice escaped HTTP barrier: {result}"),
            }
            let _retired = retire.then(|| RetiredForTest::new("codex", channel));
            held.release.notify_one();
            assert_eq!(operation.await, !retire);
            assert_eq!(
                notices.load(Ordering::Relaxed),
                1,
                "in-flight notice completes once"
            );
            assert_eq!(
                held.dispatch_failures.load(Ordering::Relaxed),
                usize::from(!retire)
            );
            assert_eq!(
                shared
                    .core
                    .lock()
                    .await
                    .sessions
                    .contains_key(&ChannelId::new(channel)),
                !retire
            );
            if retire {
                assert_eq!(fingerprint(&path), before);
            } else {
                assert!(fingerprint(&path).is_none());
            }
            server.abort();
        }
    })
    .await;
}
