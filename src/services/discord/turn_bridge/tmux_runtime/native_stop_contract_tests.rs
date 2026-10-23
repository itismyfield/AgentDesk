use super::*;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::turn_bridge::bridge_entry_persist::entry_abort_mailbox_tests::{
    cold_error::Shown, seed_context,
};
use crate::services::discord::{self as discord, Data};
use poise::serenity_prelude as serenity;
use std::io::Write;

fn run<F: std::future::Future>(future: F) -> F::Output {
    // This fixture owns Legacy delivery with a verified empty O selection.
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    super::run(future)
}

fn running() -> String {
    include_str!("../../../../../tests/fixtures/tui_input/claude-2.1.293-tool-running.txt")
        .lines()
        .filter(|line| !line.contains("Tomfoolering"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn append(path: &std::path::Path, line: serde_json::Value) {
    writeln!(
        std::fs::OpenOptions::new().append(true).open(path).unwrap(),
        "{line}"
    )
    .unwrap();
}

async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        while !ready() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native stop contract must converge");
}

async fn mailbox_released(
    shared: &SharedData,
    channel: ChannelId,
    token: &Arc<CancelToken>,
    reader: &std::thread::JoinHandle<()>,
    fx: &Fixture,
    path: &std::path::Path,
) {
    // turn_duration completes through the reader's 15s idle fallback and EOF probes.
    let started = std::time::Instant::now();
    let released = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while mailbox_holds(shared, channel, token).await {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        released.is_ok(),
        "mailbox release timed out: channel={} elapsed={:?} reader_finished={} cancelled={} completion_cleanup={} transcript={:?} tmux_calls={:?}",
        channel.get(),
        started.elapsed(),
        reader.is_finished(),
        token.cancelled.load(Ordering::Acquire),
        token.is_completion_cleanup(),
        std::fs::read_to_string(path),
        fx.calls(),
    );
}

fn bridge(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    token: &Arc<CancelToken>,
    session: &str,
    path: &std::path::Path,
) -> (std::thread::JoinHandle<()>, Arc<Shown>) {
    let mut row =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
    row.turn_nonce = token.turn_nonce().map(str::to_owned);
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    row.output_path = Some(path.display().to_string());
    inflight::save_inflight_state(&row).unwrap();
    let shown = Arc::new(Shown::default());
    let mut context = seed_context("", row);
    context.provider = ProviderKind::Claude;
    context.user_msg_id = Some(MessageId::new(channel.get() + 1));
    context.current_msg_id = Some(MessageId::new(channel.get() + 2));
    context.gateway = shown.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    discord::increment_global_active(shared, "native_stop_contract");
    discord::turn_bridge::spawn_turn_bridge(shared.clone(), token.clone(), rx, context);
    let (token, path, session) = (
        token.clone(),
        path.display().to_string(),
        session.to_owned(),
    );
    let worker = std::thread::spawn(move || {
        let (_hooks, events) = tokio::sync::broadcast::channel(8);
        crate::services::claude::read_claude_tui_transcript_until_done(
            &path,
            0,
            tx,
            Some(token),
            &session,
            "native-stop-fixture",
            events,
            chrono::Utc::now(),
        )
        .unwrap();
    });
    (worker, shown)
}

async fn transport(
    shared: &Arc<SharedData>,
    channel: ChannelId,
) -> (
    discord_mock::DiscordMockState,
    serenity::Context,
    tokio::task::JoinHandle<()>,
    Data,
) {
    let mock = discord_mock::DiscordMockState::new();
    mock.allow_channel(channel.get());
    mock.park_first_placeholder.store(false, Ordering::SeqCst);
    let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    shared.http.cached_serenity_ctx.set(ctx.clone()).unwrap();
    shared.settings.write().await.owner_user_id = Some(7);
    let mut voice_config = crate::voice::VoiceConfig::default();
    let recordings = tempfile::tempdir().unwrap();
    voice_config.keep_recordings = true;
    voice_config.audio.recordings_dir = recordings.path().into();
    let data = Data {
        shared: shared.clone(),
        token: "test-token".into(),
        provider: ProviderKind::Claude,
        voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
        voice_config,
    };
    (mock, ctx, server, data)
}

async fn command(ctx: &serenity::Context, data: &Data, channel: ChannelId) {
    let mut message = serenity::Message::default();
    message.id = MessageId::new(channel.get() + 900);
    message.channel_id = channel;
    message.author.id = UserId::new(7);
    message.content = "!stop".into();
    assert!(
        Box::pin(discord::commands::handle_text_command_with_uploads(
            ctx,
            &message,
            data,
            channel,
            "!stop",
            &[],
            &mut None,
        ))
        .await
        .unwrap()
    );
}

mod high_risk_recovery {
    use super::*;

    #[test]
    fn preserve_stop_native_reader_and_finalizer_wait_for_same_turn_exit_pg() {
        let fx = Fixture::new();
        fx.pane(&running());
        run(async {
            let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
                "native_stop_contract",
                "native stop contract",
            )
            .await;
            let pool = db.connect_and_migrate_with_max_connections(4).await;
            for natural_first in [false, true] {
                let shared = discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
                let channel = ChannelId::new(7_193_101 + u64::from(natural_first));
                let (mock, ctx, server, data) = transport(&shared, channel).await;
                let session = format!("native-reader-stop-{natural_first}");
                let (token, path) = generating_turn(&fx, &session);
                start(&shared, channel, &token).await;
                native_inflight_row(channel, &token, &session);
                let mut events = shared.turn_completion_events.subscribe();
                let (reader, shown) = bridge(&shared, channel, &token, &session, &path);
                let _ = fx.take_calls();
                if natural_first {
                    let stop =
                        begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop")
                            .await;
                    let CommandStop::Native(stop) = stop else {
                        panic!("native stop result required");
                    };
                    append(
                        &path,
                        serde_json::json!({"type":"system", "subtype":"turn_duration"}),
                    );
                    mailbox_released(&shared, channel, &token, &reader, &fx, &path).await;
                    assert!(stop.finish().await.token.is_none());
                    assert!(token.is_completion_cleanup(), "natural completion must win");
                    assert_eq!(tombstone(channel), None);
                } else {
                    let request = command(&ctx, &data, channel);
                    let observe = async {
                        until(|| {
                            fx.calls().iter().any(|call| {
                                call.starts_with("send-keys") && call.ends_with("Escape")
                            })
                        })
                        .await;
                        // A second EOF liveness probe proves the reader survived the first.
                        let reader_live =
                            tokio::time::timeout(std::time::Duration::from_secs(60), async {
                                loop {
                                    if fx
                                        .calls()
                                        .iter()
                                        .filter(|call| call.starts_with("list-panes"))
                                        .count()
                                        >= 2
                                    {
                                        break;
                                    }
                                    assert!(
                                        !reader.is_finished(),
                                        "native reader exited before terminal records: {:?}",
                                        fx.calls()
                                    );
                                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                                }
                            })
                            .await;
                        assert!(
                            reader_live.is_ok(),
                            "native reader must keep polling its live pane: {:?}",
                            fx.calls()
                        );
                        assert!(
                            !token.cancelled.load(Ordering::Acquire),
                            "delivery is not native termination"
                        );
                        assert!(
                            mailbox_holds(&shared, channel, &token).await,
                            "live native turn retains mailbox"
                        );
                        assert!(inflight::inflight_state_file_exists(
                            &ProviderKind::Claude,
                            channel.get()
                        ));
                        assert!(
                            events.try_recv().is_err(),
                            "no finalizer release before native exit"
                        );
                        assert!(
                            !shown
                                .0
                                .lock()
                                .unwrap()
                                .iter()
                                .any(|body| body.contains("[Stopped]"))
                        );
                        append(
                            &path,
                            serde_json::json!({"type":"assistant", "message":{"content":[{"type":"text","text":"NATIVE_READER_CONTINUES"}]}}),
                        );
                        append(
                            &path,
                            serde_json::json!({"type":"system", "subtype":"turn_duration"}),
                        );
                    };
                    tokio::join!(request, observe);
                }
                mailbox_released(&shared, channel, &token, &reader, &fx, &path).await;
                until(|| {
                    !inflight::inflight_state_file_exists(&ProviderKind::Claude, channel.get())
                })
                .await;
                let mut releases = 0;
                while let Ok(event) = events.try_recv() {
                    if event.phase
                        == discord::turn_completion_events::TurnCompletionPhase::MailboxReleased
                    {
                        releases += 1;
                    }
                }
                assert_eq!(releases, 1, "the canonical finalizer releases once");
                assert!(!inflight::inflight_state_file_exists(
                    &ProviderKind::Claude,
                    channel.get()
                ));
                assert!(
                    !mock
                        .written_texts()
                        .iter()
                        .any(|text| text.contains("중단 미확인"))
                );
                tokio::task::spawn_blocking(move || reader.join().unwrap())
                    .await
                    .unwrap();
                crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
                server.abort();
            }
            pool.close().await;
            db.drop().await;
        });
    }
}

#[test]
fn preserve_stop_failed_command_and_mailbox_notice_once_and_reader_continues() {
    let fx = Fixture::new();
    run(async {
        for (n, failure) in ["modal", "capture", "io", "unknown", "draft"]
            .into_iter()
            .enumerate()
        {
            fx.serve(Server::Live);
            fx.reject_escape(failure == "io");
            let pane = if failure == "draft" {
                "· Working… (4s · esc to interrupt)\n❯ preserve draft".to_owned()
            } else if failure == "modal" {
                "Allow\nDeny\nEnter to confirm\n❯".to_owned()
            } else {
                running()
            };
            fx.pane(&pane);
            fx.reject_capture(failure == "capture");
            let shared = discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_193_200 + n as u64);
            let (mock, ctx, server, data) = transport(&shared, channel).await;
            let session = format!("native-stop-failed-{failure}");
            let (token, path) = generating_turn(&fx, &session);
            if failure == "unknown" {
                append(&path, serde_json::json!({"type":"permission-mode"}));
                assert_eq!(
                    crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(
                        &path
                    ),
                    crate::services::tui_turn_state::TuiTurnState::Unknown
                );
            }
            start(&shared, channel, &token).await;
            native_inflight_row(channel, &token, &session);
            let mut events = shared.turn_completion_events.subscribe();
            let (reader, shown) = bridge(&shared, channel, &token, &session, &path);
            let before = inflight::InflightTurnIdentity::from_state(
                &inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get())
                    .unwrap(),
            );
            assert!(
                discord::mailbox_stop::active_native(
                    &shared,
                    channel,
                    Some(MessageId::new(channel.get() + 1))
                )
                .await
            );
            let mailbox = discord::mailbox_cancel_active_turn_if_handoff_user_message_with_reason(
                &shared,
                channel,
                MessageId::new(channel.get() + 1),
                "mailbox_cancel",
            );
            let ((), result) = tokio::join!(command(&ctx, &data, channel), mailbox);
            assert!(result.token.is_none());
            command(&ctx, &data, channel).await;
            assert!(!token.cancelled.load(Ordering::Acquire));
            assert!(mailbox_holds(&shared, channel, &token).await);
            let after =
                inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get())
                    .expect("the running reader keeps its row");
            assert_eq!(inflight::InflightTurnIdentity::from_state(&after), before);
            assert!(
                events.try_recv().is_err(),
                "failed stop never submits a finalizer"
            );
            assert_eq!(tombstone(channel), None);
            // The mock exposes each POST in its request history and minted-message map.
            assert_eq!(
                mock.written_texts()
                    .iter()
                    .filter(|text| text.contains("중단 미확인"))
                    .count(),
                2,
                "one successful notice POST, including concurrent and repeated stops: {failure}"
            );
            assert!(
                !mock
                    .written_texts()
                    .iter()
                    .any(|text| text.contains("[Stopped]"))
            );
            fx.reject_capture(false);
            fx.reject_escape(false);
            append(
                &path,
                serde_json::json!({"type":"assistant", "message":{"content":[{"type":"text","text":"NATIVE_READER_CONTINUES"}]}}),
            );
            append(
                &path,
                serde_json::json!({"type":"system", "subtype":"turn_duration"}),
            );
            mailbox_released(&shared, channel, &token, &reader, &fx, &path).await;
            assert!(
                token.is_completion_cleanup(),
                "an unconfirmed stop leaves normal completion intact"
            );
            until(|| {
                shown
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|body| body.contains("NATIVE_READER_CONTINUES"))
            })
            .await;
            assert!(
                !shown
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|body| body.contains("[Stopped]"))
            );
            tokio::task::spawn_blocking(move || reader.join().unwrap())
                .await
                .unwrap();
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
            server.abort();
        }
    });
}

#[test]
fn preserve_stop_concurrent_attempt_shares_delivery_and_rejects_previous_native_proof() {
    let fx = Fixture::new();
    fx.pane(&running());
    run(async {
        for success in [true, false] {
            fx.reject_escape(!success);
            let shared = discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_193_040 + u64::from(success));
            let session = format!("native-stop-coalesced-{success}");
            let (token, path) = generating_turn(&fx, &session);
            start(&shared, channel, &token).await;
            native_inflight_row(channel, &token, &session);
            let (holding, held) = std::sync::mpsc::channel();
            let (release, released) = std::sync::mpsc::channel();
            let session_locked = session.clone();
            let lock = std::thread::spawn(move || {
                crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
                    &session_locked,
                    || {
                        holding.send(()).unwrap();
                        released.recv().unwrap();
                    },
                );
            });
            held.recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            let _ = fx.take_calls();
            let first = begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop");
            let second = begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "/stop");
            let unblock = async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                release.send(()).unwrap();
            };
            let (first, second, ()) = tokio::join!(first, second, unblock);
            lock.join().unwrap();
            let (CommandStop::Native(first), CommandStop::Native(second)) = (first, second) else {
                panic!("native replies");
            };
            assert_eq!(first.wire()["effect_started"], success);
            assert_eq!(second.wire()["effect_started"], success);
            assert_eq!(
                fx.take_calls()
                    .iter()
                    .filter(|call| call.starts_with("send-keys") && call.ends_with("Escape"))
                    .count(),
                1
            );
            assert!(!token.cancelled.load(Ordering::Acquire));
            if success {
                let third =
                    begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await;
                assert!(matches!(third, CommandStop::Native(_)));
                assert!(
                    !fx.take_calls()
                        .iter()
                        .any(|call| call.starts_with("send-keys"))
                );
                append(
                    &path,
                    serde_json::json!({"type":"user", "uuid":"successor", "message":{"content":"next"}}),
                );
                append(
                    &path,
                    serde_json::json!({"type":"assistant", "message":{"content":[]}}),
                );
                let next =
                    begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await;
                let CommandStop::Native(next) = next else {
                    panic!("a new native identity cannot reuse the previous proof");
                };
                assert_eq!(next.wire()["effect_started"], false);
                assert!(!token.cancelled.load(Ordering::Acquire));
                assert!(
                    !fx.take_calls()
                        .iter()
                        .any(|call| call.starts_with("send-keys"))
                );
            } else {
                assert_eq!(
                    usize::from(first.should_reply()) + usize::from(second.should_reply()),
                    1
                );
                let reset = discord::mailbox_cancel_active_turn(&shared, channel).await;
                assert!(
                    reset
                        .token
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &token))
                );
                assert!(
                    token.cancelled.load(Ordering::Acquire),
                    "forced restart keeps its raw cancellation contract after a failed preserve stop"
                );
            }
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
        let shared = discord::make_shared_data_for_tests();
        let channel = ChannelId::new(7_193_042);
        let session = "native-stop-cancelled-caller";
        fx.reject_escape(false);
        let (token, _) = generating_turn(&fx, session);
        start(&shared, channel, &token).await;
        native_inflight_row(channel, &token, session);
        let (holding, held) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
                session,
                || {
                    holding.send(()).unwrap();
                    released.recv().unwrap();
                },
            );
        });
        held.recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        let _ = fx.take_calls();
        let mut caller = Box::pin(begin_user_stop(
            &shared,
            &ProviderKind::Claude,
            channel,
            true,
            "!stop",
        ));
        tokio::select! {
            _ = &mut caller => panic!("delivery must wait on the composer"),
            () = until(|| fx.calls().iter().any(|call| call.starts_with("capture-pane"))) => {}
        }
        drop(caller);
        release.send(()).unwrap();
        worker.join().unwrap();
        until(|| {
            fx.calls()
                .iter()
                .any(|call| call.starts_with("send-keys") && call.ends_with("Escape"))
        })
        .await;
        let result = begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await;
        let CommandStop::Native(result) = result else {
            panic!("shared delivered result");
        };
        assert_eq!(result.wire()["effect_started"], true);
        assert_eq!(
            fx.take_calls()
                .iter()
                .filter(|call| call.starts_with("send-keys") && call.ends_with("Escape"))
                .count(),
            1
        );
        assert!(!token.cancelled.load(Ordering::Acquire));
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
    });
}

#[test]
fn preserve_stop_rechecks_after_composer_wait_without_retargeting() {
    let fx = Fixture::new();
    run(async {
        for (n, change) in ["done", "successor", "draft", "modal", "capture"]
            .into_iter()
            .enumerate()
        {
            eprintln!("native stop composer window: {change}");
            fx.serve(Server::Live);
            fx.pane(&running());
            let shared = discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_193_070 + n as u64);
            let session = format!("native-stop-window-{change}");
            let (token, path) = generating_turn(&fx, &session);
            start(&shared, channel, &token).await;
            native_inflight_row(channel, &token, &session);
            let reader =
                (change == "done").then(|| bridge(&shared, channel, &token, &session, &path).0);
            let (holding, held) = std::sync::mpsc::channel();
            let (release, released) = std::sync::mpsc::channel();
            let locked = session.clone();
            let thread = std::thread::spawn(move || {
                crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
                    &locked,
                    || {
                        holding.send(()).unwrap();
                        released.recv().unwrap();
                    },
                );
            });
            held.recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            let _ = fx.take_calls();
            let request = begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop");
            let update = async {
                until(|| {
                    fx.calls()
                        .iter()
                        .any(|call| call.starts_with("capture-pane"))
                })
                .await;
                match change {
                    "done" => {
                        append(
                            &path,
                            serde_json::json!({"type":"system","subtype":"turn_duration"}),
                        );
                        mailbox_released(
                            &shared,
                            channel,
                            &token,
                            reader.as_ref().expect("done case has a native reader"),
                            &fx,
                            &path,
                        )
                        .await;
                    }
                    "successor" => {
                        discord::mailbox_finish_turn(&shared, &ProviderKind::Claude, channel).await;
                        let next = bound_token(&ProviderKind::Claude, &session);
                        start(&shared, channel, &next).await;
                        append(
                            &path,
                            serde_json::json!({"type":"user","uuid":"new","message":{"content":"successor"}}),
                        );
                    }
                    "draft" => {
                        fx.pane("· Working… (4s · esc to interrupt)\n❯ do not erase my draft")
                    }
                    "modal" => fx.pane(
                        "· Working… (4s · esc to interrupt)\nAllow\nDeny\nEnter to confirm\n❯",
                    ),
                    "capture" => fx.serve(Server::NoSocket),
                    _ => unreachable!(),
                }
                release.send(()).unwrap();
            };
            let (result, ()) = tokio::join!(request, update);
            thread.join().unwrap();
            assert!(matches!(result, CommandStop::Native(_)));
            assert!(
                !fx.take_calls()
                    .iter()
                    .any(|call| call.starts_with("send-keys")),
                "stale pane write: {change}"
            );
            assert_eq!(tombstone(channel), None);
            if change == "successor" {
                let current = shared
                    .mailbox_peek(channel)
                    .unwrap()
                    .cancel_token()
                    .await
                    .unwrap()
                    .unwrap();
                assert!(!Arc::ptr_eq(&current, &token));
                assert!(!current.cancelled.load(Ordering::Acquire));
            } else if change != "done" {
                assert!(mailbox_holds(&shared, channel, &token).await);
                assert!(!token.cancelled.load(Ordering::Acquire));
            }
            if let Some(reader) = reader {
                tokio::task::spawn_blocking(move || reader.join().unwrap())
                    .await
                    .unwrap();
            }
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}

#[test]
fn preserve_stop_idle_native_api_uses_existing_runtime_recovery() {
    let fx = Fixture::new();
    run(async {
        let (shared, registry) = runtime().await;
        let channel = ChannelId::new(7_193_800);
        let session = "native-idle-api-recovery";
        let (token, path) = generating_turn(&fx, session);
        append(
            &path,
            serde_json::json!({"type":"system","subtype":"turn_duration"}),
        );
        fx.ready(session);
        start(&shared, channel, &token).await;
        let target = TurnLifecycleTarget {
            provider: Some(ProviderKind::Claude),
            channel_id: Some(channel),
            tmux_name: session.into(),
        };
        let result = crate::services::turn_lifecycle::stop_turn_preserving_queue(
            Some(&registry),
            &target,
            "native_idle_api",
        )
        .await;
        assert!(
            !result.host_guard_kept(),
            "idle native API is a tmux recovery, not a host refusal"
        );
        assert_eq!(result.mailbox_foreground_free, Some(true));
        assert!(
            shared
                .mailbox(channel)
                .snapshot()
                .await
                .cancel_token
                .is_none()
        );
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
        for done_first in [false, true] {
            let channel = ChannelId::new(7_193_810 + u64::from(done_first));
            let token = Arc::new(CancelToken::new());
            start(&shared, channel, &token).await;
            let idle = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let observed = idle.clone();
            assert!(
                token
                    .native_stop
                    .handoff_idle(Arc::new(move || observed.load(Ordering::Acquire)))
            );
            if done_first {
                token.native_stop.completed();
            } else {
                idle.store(false, Ordering::Release);
            }
            let result = shared
                .mailbox(channel)
                .cancel_active_turn_if_current_unless_herdr(
                    token.clone(),
                    "idle_publish_race".into(),
                )
                .await;
            assert!(matches!(result,crate::services::provider::cancel_token_claude_interrupt::StopCancel::NotCurrent),"idle handoff rechecks Done and resumed execution at the actor");
            assert!(!token.cancelled.load(Ordering::Acquire));
            assert!(mailbox_holds(&shared, channel, &token).await);
        }
    });
}

#[test]
fn preserve_stop_raw_draft_matches_main_escape() {
    let fx = Fixture::new();
    let pane = "· Actioning… (4m 7s · esc to interrupt)\n❯ draft";
    assert!(
        crate::services::tmux_common::tmux_capture_indicates_claude_tui_actively_streaming(pane)
    );
    fx.pane(pane);
    run(async {
        let session = "native-raw-draft-control";
        let (token, _) = generating_turn(&fx, session);
        let _ = fx.take_calls();
        discord::turn_bridge::stop_active_turn(
            &ProviderKind::Claude,
            &token,
            TmuxCleanupPolicy::PreserveSession,
            "raw_control",
        )
        .await;
        assert_eq!(
            fx.take_calls()
                .iter()
                .filter(|call| call.starts_with("send-keys") && call.ends_with("Escape"))
                .count(),
            1,
            "raw draft Escape matches main"
        );
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
    });
}

#[test]
fn preserve_stop_unconfirmed_native_api_reports_delivery_state() {
    let fx = Fixture::new();

    run(async {
        for (n, (probe_failed, row_error)) in
            [(false, false), (true, false), (false, true), (false, false)]
                .into_iter()
                .enumerate()
        {
            fx.serve(if probe_failed {
                Server::Missing
            } else {
                Server::Live
            });
            if row_error {
                fx.pane(&running());
            } else {
                fx.pane("Allow\nDeny\nEnter to confirm\n❯");
            }
            let (shared, registry) = runtime().await;
            let channel = ChannelId::new(7_193_801 + n as u64 * 3);
            let session = format!("native-unconfirmed-api-recovery-{n}");
            let (token, transcript) = generating_turn(&fx, &session);
            if n == 3 {
                eprintln!("NATIVE_TAIL_UNOBSERVED bytes=300000 same_recorded_input=true");
                fx.pane(&running());
                append(
                    &transcript,
                    serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"x".repeat(300_000)}]}}),
                );
                assert!(
                    super::super::super::claude_stop_delivery::ClaudeStopTurnIdentity::capture(
                        transcript.to_str().unwrap()
                    )
                    .is_none(),
                    "current user is beyond the bounded tail"
                );
            }
            start(&shared, channel, &token).await;
            let row = native_inflight_row(channel, &token, &session);
            if row_error {
                std::fs::write(&row, "unreadable row").unwrap();
            }
            let before = file_state(&row);
            let _ = fx.take_calls();
            let target = TurnLifecycleTarget {
                provider: Some(ProviderKind::Claude),
                channel_id: Some(channel),
                tmux_name: session.clone(),
            };
            let result = crate::services::turn_lifecycle::stop_turn_preserving_queue(
                Some(&registry),
                &target,
                "native_unconfirmed_api",
            )
            .await;
            assert_eq!(
                result.refusal_reason(),
                "native_stop_unconfirmed",
                "a native delivery failure is not a non-tmux host"
            );
            assert!(mailbox_holds(&shared, channel, &token).await);
            assert!(!token.cancelled.load(Ordering::Acquire));
            assert_eq!(file_state(&row), before);
            assert_eq!(tombstone(channel), None);
            assert!(
                !fx.take_calls()
                    .iter()
                    .any(|call| call.starts_with("send-keys")),
                "unobserved ownership never authorizes an Escape"
            );
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}

#[test]
fn preserve_stop_native_published_rearms_drain_after_watcher_cleanup() {
    let fx = Fixture::new();
    fx.pane(&running());
    run(async {
        let shared = discord::make_shared_data_for_tests();
        let channel = ChannelId::new(7_193_802);
        let session = "native-published-queue-drain";
        let (token, path) = generating_turn(&fx, session);
        start(&shared, channel, &token).await;
        native_inflight_row(channel, &token, session);
        let generation = discord::runtime_store::process_generation();
        let message = MessageId::new(channel.get() + 3);
        let pending = crate::services::turn_orchestrator::Intervention {
            author_id: UserId::new(7),
            author_is_bot: false,
            message_id: message,
            queued_generation: generation,
            source_message_ids: vec![message],
            source_message_queued_generations: vec![],
            source_text_segments: vec![],
            text: "queued after native stop".into(),
            mode: crate::services::turn_orchestrator::InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: vec![],
            voice_announcement: None,
        };
        shared
            .mailbox(channel)
            .replace_queue(
                vec![pending],
                discord::queue_persistence_context(&shared, &ProviderKind::Claude, channel),
            )
            .await;
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = count.clone();
        let _hook =
            discord::queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(move |_, _, at, _| {
                let seen = seen.clone();
                Box::pin(async move {
                    if at != channel {
                        return None;
                    }
                    seen.fetch_add(1, Ordering::SeqCst);
                    Some(discord::IdleQueueKickoffChannelOutcome { started: true })
                })
            }));
        let (_, ctx, server, data) = transport(&shared, channel).await;
        let _ = fx.take_calls();
        let watcher = async {
            until(|| {
                fx.calls()
                    .iter()
                    .any(|call| call.starts_with("send-keys") && call.ends_with("Escape"))
            })
            .await;
            append(
                &path,
                serde_json::json!({"type":"system","subtype":"turn_duration"}),
            );
            fx.ready(session);
            until(|| token.cancelled.load(Ordering::Acquire)).await;
            assert!(discord::clear_inflight_state(
                &ProviderKind::Claude,
                channel.get()
            ));
            discord::mailbox_finish_turn(&shared, &ProviderKind::Claude, channel).await;
        };
        tokio::join!(command(&ctx, &data, channel), watcher);
        until(|| count.load(Ordering::SeqCst) > 0).await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "native publication rearms one queued drain"
        );
        assert_eq!(
            fx.take_calls()
                .iter()
                .filter(|call| call.starts_with("send-keys") && call.ends_with("Escape"))
                .count(),
            1,
            "post-publish recovery must not interrupt again"
        );
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
        server.abort();
    });
}

#[test]
fn preserve_stop_completed_owner_keeps_natural_first() {
    let fx = Fixture::new();
    run(async {
        let shared = discord::make_shared_data_for_tests();
        let channel = ChannelId::new(7_193_900);
        let session = "native-completed-owned";
        let (token, path) = generating_turn(&fx, session);
        start(&shared, channel, &token).await;
        let row = native_inflight_row(channel, &token, session);
        append(
            &path,
            serde_json::json!({"type":"system","subtype":"turn_duration"}),
        );
        token.native_stop.completed();
        let before = file_state(&row);
        let result = begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await;
        assert!(
            !token.cancelled.load(Ordering::Acquire),
            "an owned Done waits for natural finalization"
        );
        let CommandStop::Native(stop) = result else {
            panic!("completed owned turn remains native");
        };
        assert!(stop.finish().await.token.is_none());
        assert!(mailbox_holds(&shared, channel, &token).await);
        assert_eq!(file_state(&row), before);
        assert_eq!(tombstone(channel), None);
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
    });
}

#[test]
fn preserve_stop_unbound_or_different_row_uses_main() {
    let fx = Fixture::new();
    fx.pane(&running());
    run(async {
        for entry in ["command", "mailbox"] {
            for (n, case) in [
                "unbound",
                "foreign_nonce",
                "foreign_session",
                "completed_no_owner",
                "completed_other_arc",
            ]
            .into_iter()
            .enumerate()
            {
                let shared = discord::make_shared_data_for_tests();
                let channel =
                    ChannelId::new(7_193_910 + n as u64 + if entry == "mailbox" { 30 } else { 0 });
                let session = format!("native-recovery-{case}");
                let (bound, _) = generating_turn(&fx, &session);
                let token = if case == "unbound" {
                    Arc::new(CancelToken::new())
                } else {
                    bound
                };
                start(&shared, channel, &token).await;
                let row_path = native_inflight_row(channel, &token, &session);
                let mut other_owner = None;
                let mut other_token = None;
                match case {
                    "completed_no_owner" | "completed_other_arc" => {
                        if case == "completed_other_arc" {
                            let row = inflight::load_inflight_state_read_only(
                                &ProviderKind::Claude,
                                channel.get(),
                            )
                            .unwrap();
                            let other = Arc::new(CancelToken::from_persisted_turn_nonce(
                                token.turn_nonce().map(str::to_owned),
                            ));
                            other_owner = discord::live_bridge::register_without_requeue(
                                &shared,
                                &ProviderKind::Claude,
                                &row,
                                &other,
                            )
                            .await
                            .unwrap();
                            other_token = Some(other);
                        }
                        std::fs::remove_file(&row_path).unwrap();
                        token.native_stop.completed();
                    }
                    "foreign_nonce" | "foreign_session" => {
                        let mut row = inflight::load_inflight_state_read_only(
                            &ProviderKind::Claude,
                            channel.get(),
                        )
                        .unwrap();
                        if case == "foreign_nonce" {
                            row.turn_nonce = Some("foreign_nonce".into());
                        } else {
                            row.tmux_session_name = Some("different-native-session".into());
                        }
                        inflight::save_inflight_state(&row).unwrap();
                    }
                    _ => {}
                }
                let _ = fx.take_calls();
                if entry == "command" {
                    let result =
                        begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop")
                            .await;
                    assert!(
                        matches!(result, CommandStop::Stop(_)),
                        "main recovery required for {case}"
                    );
                } else {
                    let result =
                        discord::mailbox_cancel_active_turn_if_handoff_user_message_with_reason(
                            &shared,
                            channel,
                            MessageId::new(channel.get() + 1),
                            "mailbox_cancel",
                        )
                        .await;
                    assert!(
                        result
                            .token
                            .as_ref()
                            .is_some_and(|published| Arc::ptr_eq(published, &token)),
                        "guarded mailbox recovery required for {case}"
                    );
                }
                assert!(
                    token.cancelled.load(Ordering::Acquire),
                    "main publishes the stale token for {case}"
                );
                assert!(
                    !fx.take_calls()
                        .iter()
                        .any(|call| call.starts_with("send-keys")),
                    "judgment must not send keys for {case}"
                );
                drop((other_owner, other_token));
                crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
            }
        }
    });
}

#[test]
fn preserve_stop_recovery_rechecks_owner_before_publish() {
    let fx = Fixture::new();
    run(async {
        for (n, change) in ["owner", "live_bridge", "binding", "source", "cleanup"]
            .into_iter()
            .enumerate()
        {
            let shared = discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_193_930 + n as u64);
            let session = format!("native-orphan-race-{change}");
            let (token, path) = generating_turn(&fx, &session);
            start(&shared, channel, &token).await;
            token.native_stop.completed();
            let mut owner = None;
            assert!(
                discord::turn_bridge::NativeStop::begin(
                    &shared,
                    &ProviderKind::Claude,
                    channel,
                    &token,
                    &session,
                    "!stop"
                )
                .await
                .is_none()
            );
            match change {
                "owner" => {
                    native_inflight_row(channel, &token, &session);
                }
                "live_bridge" => {
                    let row_path = native_inflight_row(channel, &token, &session);
                    let row = inflight::load_inflight_state_read_only(
                        &ProviderKind::Claude,
                        channel.get(),
                    )
                    .unwrap();
                    owner = discord::live_bridge::register_without_requeue(
                        &shared,
                        &ProviderKind::Claude,
                        &row,
                        &token,
                    )
                    .await
                    .unwrap();
                    std::fs::remove_file(row_path).unwrap();
                }
                "binding" => {
                    token.bind_claude_tmux_session("native-successor-binding");
                }
                "source" => {
                    let mut binding =
                        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
                            &session,
                        )
                        .unwrap();
                    let next = path.with_extension("successor.jsonl");
                    std::fs::copy(&path, &next).unwrap();
                    binding.output_path = next.display().to_string();
                    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
                        &session, binding,
                    );
                }
                "cleanup" => token.mark_completion_cleanup(),
                _ => unreachable!(),
            }
            let result = shared
                .mailbox(channel)
                .cancel_active_turn_if_current_unless_herdr(token.clone(), "recovery_race".into())
                .await;
            assert!(matches!(result, crate::services::provider::cancel_token_claude_interrupt::StopCancel::NotCurrent), "changed recovery authority must be rechecked: {change}");
            drop(owner);
            assert!(!token.cancelled.load(Ordering::Acquire));
            assert!(mailbox_holds(&shared, channel, &token).await);
            assert_eq!(tombstone(channel), None);
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}

#[test]
fn preserve_stop_live_owner_keeps_prelaunch_native() {
    let fx = Fixture::new();
    fx.pane(&running());
    run(async {
        for (n, completed) in [false, true].into_iter().enumerate() {
            let shared = discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_193_950 + n as u64);
            let session = format!("native-live-prelaunch-{completed}");
            let (token, _) = generating_turn(&fx, &session);
            start(&shared, channel, &token).await;
            let row_path = inflight_row(&ProviderKind::Claude, channel, &session, false);
            let mut row =
                inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get())
                    .unwrap();
            row.turn_nonce = token.turn_nonce().map(str::to_owned);
            assert!(
                row.output_path.is_none(),
                "production prelaunch has no transcript path"
            );
            inflight::save_inflight_state(&row).unwrap();
            let owner = discord::live_bridge::register_without_requeue(
                &shared,
                &ProviderKind::Claude,
                &row,
                &token,
            )
            .await
            .unwrap()
            .expect("register exact live Arc");
            assert!(
                discord::live_bridge::retain_original(&ProviderKind::Claude, channel.get(), &token)
                    .is_some()
            );
            if completed {
                token.native_stop.completed();
            }
            let before = file_state(&row_path);
            let _ = fx.take_calls();
            let result =
                begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await;
            assert!(
                !token.cancelled.load(Ordering::Acquire),
                "a live owner with prelaunch row must not publish cancel"
            );
            let CommandStop::Native(stop) = result else {
                panic!("live prelaunch remains Native");
            };
            assert!(
                stop.finish().await.token.is_none(),
                "delivery is not native termination"
            );
            assert!(mailbox_holds(&shared, channel, &token).await);
            assert_eq!(file_state(&row_path), before);
            assert_eq!(tombstone(channel), None);
            assert_eq!(
                fx.take_calls()
                    .iter()
                    .filter(|call| call.starts_with("send-keys") && call.ends_with("Escape"))
                    .count(),
                usize::from(!completed)
            );
            if completed {
                std::fs::remove_file(&row_path).unwrap();
                assert!(
                    matches!(
                        begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop")
                            .await,
                        CommandStop::Native(_)
                    ),
                    "exact live owner protects completed rowless turn"
                );
                assert!(!token.cancelled.load(Ordering::Acquire));
                drop(owner);
                assert!(
                    matches!(
                        begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop")
                            .await,
                        CommandStop::Stop(_)
                    ),
                    "completed orphan returns to main after its exact owner leaves"
                );
                assert!(token.cancelled.load(Ordering::Acquire));
            }
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}
