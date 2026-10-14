//! History retry input admission: a protected channel's retry fetches, claims and queues nothing.
use super::*;
use crate::services::discord::input_runtime::fence::{Gate, Mode, test_health};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// Registers a Gate for `channel` and moves it to `mode` through the supervisor transitions.
pub(in crate::services::discord) async fn gate_in(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: u64,
    mode: Mode,
) -> (Arc<Gate>, test_health::Clear) {
    let gate = Gate::protect(provider.clone(), channel).unwrap();
    let health = test_health::Clear::new(&gate);
    if mode != Mode::LegacyOpen {
        let closing = Arc::new(gate.close().unwrap());
        if mode != Mode::Closing {
            let context = crate::services::turn_orchestrator::QueuePersistenceContext::new(
                provider, "g2e-pa", None,
            );
            let freeze = shared.freeze_legacy_queue(closing.clone(), context);
            let transition = freeze.await.unwrap();
            match mode {
                Mode::Frozen => {}
                Mode::LedgerOpen => closing.open_ledger().unwrap(),
                Mode::Held => closing.hold().unwrap(),
                Mode::Handback => closing.begin_handback().unwrap(),
                Mode::LegacyOpen | Mode::Closing => unreachable!(),
            }
            drop(transition);
        }
    }
    assert_eq!(gate.mode(), mode);
    (gate, health)
}

/// Discord REST stand-in: counts history reads and, while held, answers one only on release.
struct DiscordHistory {
    http: Arc<serenity::http::Http>,
    reads: Arc<AtomicUsize>,
    hold: Arc<AtomicBool>,
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl DiscordHistory {
    async fn start() -> Self {
        let (reads, hold) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let reached = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let state = (
            reads.clone(),
            hold.clone(),
            reached.clone(),
            release.clone(),
        );
        let app = axum::Router::new().fallback(move || {
            let (reads, hold, reached, release) = state.clone();
            async move {
                reads.fetch_add(1, Ordering::SeqCst);
                reached.notify_one();
                if hold.load(Ordering::SeqCst) {
                    release.notified().await;
                }
                axum::Json(serde_json::json!([]))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = poise::serenity_prelude::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            http: Arc::new(http),
            reads,
            hold,
            reached,
            release,
        }
    }

    fn take_reads(&self) -> usize {
        self.reads.swap(0, Ordering::SeqCst)
    }
}

async fn queued(shared: &Arc<SharedData>, channel: ChannelId, user: MessageId) -> bool {
    let snapshot = shared.mailbox(channel).snapshot().await;
    let queue = snapshot.intervention_queue;
    queue.iter().any(|item| item.message_id == user)
}

// Every non-Legacy mode, including another provider's Gate on the channel, refuses the retry
// before its dedup claim, history read, retry claim, context write and enqueue.
#[tokio::test]
async fn g2e_protected_retry_has_zero_claim_or_context() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let discord = DiscordHistory::start().await;
    let cases = [
        (ProviderKind::Claude, Mode::Closing),
        (ProviderKind::Claude, Mode::LedgerOpen),
        (ProviderKind::Claude, Mode::Held),
        (ProviderKind::Codex, Mode::Held),
    ];
    for (n, (owner, mode)) in cases.into_iter().enumerate() {
        let channel = ChannelId::new(6_325_960_200 + n as u64);
        let user = MessageId::new(6_325_960_300 + n as u64);
        let _gate = gate_in(&shared, &owner, channel.get(), mode).await;
        let retry = auto_retry_with_history(
            &discord.http,
            &shared,
            &ProviderKind::Claude,
            channel,
            user,
            "g2e",
        );
        let returned = tokio::time::timeout(Duration::from_secs(10), retry).await;
        returned.expect("a refused retry returns without waiting on Discord");
        let case = format!("{owner:?} {mode:?}");
        assert!(!retry_pending_contains(channel), "{case}: no dedup claim");
        assert_eq!(discord.take_reads(), 0, "{case}: no history read");
        assert!(!queued(&shared, channel, user).await, "{case}: no enqueue");
    }
}

// Without a Gate, and under a Legacy-open one, the retry runs as in main; an admitted retry keeps
// its permit, so a close begun during the history read waits for the follow-up to be queued.
#[tokio::test]
async fn g2e_legacy_open_and_off_keep_history_retry() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let discord = DiscordHistory::start().await;
    discord.hold.store(true, Ordering::SeqCst);
    for (n, protected) in [false, true].into_iter().enumerate() {
        let channel = ChannelId::new(6_325_960_400 + n as u64);
        let user = MessageId::new(6_325_960_500 + n as u64);
        let provider = ProviderKind::Claude;
        let gate = match protected {
            true => Some(gate_in(&shared, &provider, channel.get(), Mode::LegacyOpen).await),
            false => None,
        };
        let retry = {
            let (http, shared) = (discord.http.clone(), shared.clone());
            tokio::spawn(async move {
                auto_retry_with_history(&http, &shared, &provider, channel, user, "g2e").await
            })
        };
        let reached = tokio::time::timeout(Duration::from_secs(10), discord.reached.notified());
        reached.await.expect("the retry reads the history");
        assert!(
            retry_pending_contains(channel),
            "{protected}: dedup claim as in main"
        );
        let closing = gate.as_ref().map(|(gate, _)| gate.close().unwrap());
        if let Some(closing) = &closing {
            let early = tokio::time::timeout(Duration::from_millis(300), closing.drain()).await;
            assert!(early.is_err(), "close waits for the admitted retry");
        }
        discord.release.notify_one();
        retry.await.unwrap();
        if let Some(closing) = &closing {
            let drained = tokio::time::timeout(Duration::from_secs(10), closing.drain()).await;
            drained.expect("the effect ends with the retry");
        }
        assert_eq!(discord.take_reads(), 1, "{protected}: one history read");
        assert!(queued(&shared, channel, user).await, "{protected}: queued");
        release_retry_pending(channel);
    }
}
