use super::*;
use axum::body::Bytes;
use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;
use poise::serenity_prelude::{Http, HttpBuilder};
use tokio::sync::{Notify, oneshot};

struct PausedDiscord {
    http: Arc<Http>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    requests: Arc<Mutex<Vec<String>>>,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
}

impl PausedDiscord {
    async fn start() -> Self {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let entered_post = entered.clone();
        let release_post = release.clone();
        let recorded = requests.clone();
        let app = axum::Router::new().fallback(axum::routing::any(
            move |method: Method, uri: Uri, body: Bytes| {
                let (entered, release, requests) =
                    (entered_post.clone(), release_post.clone(), recorded.clone());
                async move {
                    requests.lock().unwrap().push(format!(
                        "{method} {} {}",
                        uri.path(),
                        String::from_utf8_lossy(&body)
                    ));
                    if method == Method::POST {
                        entered.notify_one();
                        release.notified().await;
                    }
                    if method == Method::DELETE {
                        return StatusCode::NO_CONTENT.into_response();
                    }
                    axum::Json(test_support::message_json(
                        900_001,
                        CHANNEL,
                        1,
                        "",
                        "2026-10-02T00:00:00+00:00",
                    ))
                    .into_response()
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let http = HttpBuilder::new("test-token")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build();
        let (shutdown, stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Self {
            http: Arc::new(http),
            entered,
            release,
            requests,
            shutdown,
            server,
        }
    }

    async fn join(self) -> Vec<String> {
        self.shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), self.server)
            .await
            .unwrap()
            .unwrap();
        self.requests.lock().unwrap().clone()
    }
}

async fn after_await_case(replace_source: bool) {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let discord = PausedDiscord::start().await;
    let (fixture, writer) = configured_fixture(discord.http.clone()).await;
    let _retired = RetiredForTest::new("claude", CHANNEL);
    let _owned =
        cutover::test_override::force_channels(&[(CHANNEL, RuntimeHandoffKind::ClaudeTui)]);
    let inflight = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = test_support::tree_fingerprint(&inflight);
    let observed = Guard::new(&fixture.provider, CHANNEL);
    observed.register_source(Path::new(&fixture.output));
    let submitted = Scope::new(&fixture.shared);
    let barrier = TerminalBarrier::arm(&fixture.provider, CHANNEL);
    let (stop, actor) = writer.start();
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    let handle = fixture.handle();
    let ticket = watcher_completion::observe(&handle.cancel).unwrap();
    until(&observed, "initial EOF before awaited native card", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let payload = transcript(Turn::Task);
    append(&fixture, payload.as_bytes());
    barrier.wait().await;
    barrier.release();
    tokio::time::timeout(Duration::from_secs(15), discord.entered.notified())
        .await
        .unwrap();
    let successor_cancel = Arc::new(AtomicBool::new(false));
    if replace_source {
        std::fs::rename(&fixture.output, format!("{}.opened", fixture.output)).unwrap();
        std::fs::write(&fixture.output, &payload).unwrap();
    } else {
        fixture
            .shared
            .tmux_watchers
            .replace_paired_cancel_for_test(&fixture.session, successor_cancel.clone());
    }
    crate::services::discord::tmux::tmux_watcher::cancel_for_test(
        &handle.cancel,
        Ordering::Release,
    );
    discord.release.notify_one();
    joined(ticket).await;
    finish_writer(stop, actor).await;
    let completed = submitted.join_all().await;
    assert_eq!(submitted.submissions(), 1);
    assert_eq!(completed.len(), 1);
    assert!(
        completed
            .iter()
            .all(|(_, outcome)| *outcome == Outcome::Returned)
    );
    let requests = discord.join().await;
    let pending =
        crate::services::discord::tmux::tmux_watcher::cancel_handoff::capsule_observation::pending(
            &fixture.shared,
            fixture.channel,
        )
        .await;
    let key = observed.snapshot();
    let raw = observed.raw_process_snapshot();
    eprintln!(
        "T6_B_AFTER_AWAIT_SOURCE={replace_source}\nT6_B_KEY={key:?}\nT6_B_RAW={raw:?}\nT6_B_CAPSULES={pending:?}\nT6_B_HTTP={requests:?}"
    );
    for snapshot in [key.clone(), raw] {
        assert_eq!(snapshot.writable_inflight_load_calls, 0, "{snapshot:?}");
        assert_eq!(snapshot.readonly_inflight_load_calls, 0, "{snapshot:?}");
        assert_eq!(snapshot.compatibility_backfill_attempts, 0, "{snapshot:?}");
        assert_eq!(snapshot.legacy_prefix_restore_calls, 0, "{snapshot:?}");
        assert_eq!(
            snapshot.event_count("terminal_preflight_entries"),
            0,
            "{snapshot:?}"
        );
    }
    assert!(key.event_count("parsed_terminal") > 0);
    assert!(key.event_count("retired_terminal_retained") > 0, "{key:?}");
    assert!(key.retired_terminal_ranges.is_empty(), "{key:?}");
    assert_eq!(key.event_count("retired_terminal_consumed"), 0);
    assert_eq!(key.event_count("post_stream_exit"), 1);
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert!(pending[0].origin_is_retired());
    assert_eq!(pending[0].body, BODY);
    assert!(!pending[0].has_startup_snapshot && !pending[0].has_identity);
    assert_eq!(
        requests.iter().filter(|r| r.starts_with("POST ")).count(),
        1,
        "{requests:?}"
    );
    assert!(
        requests.iter().any(|r| r.contains("background work")),
        "{requests:?}"
    );
    assert!(requests.iter().all(|r| !r.contains(BODY)), "{requests:?}");
    assert_eq!(test_support::tree_fingerprint(&inflight), before);
    assert!(!successor_cancel.load(Ordering::Acquire));
}

#[tokio::test]
async fn t6_b_source_replacement_during_actual_card_post_retains_terminal_after_await() {
    if child(
        "await_races::t6_b_source_replacement_during_actual_card_post_retains_terminal_after_await",
    ) {
        after_await_case(true).await;
    }
}

#[tokio::test]
async fn t6_b_cancel_arc_replacement_during_actual_card_post_retains_terminal_after_await() {
    if child(
        "await_races::t6_b_cancel_arc_replacement_during_actual_card_post_retains_terminal_after_await",
    ) {
        after_await_case(false).await;
    }
}
