use super::*;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::turn_bridge::bridge_entry_persist::entry_abort_mailbox_tests::{
    cold_error::Shown, seed_context,
};
use crate::services::discord::{self as discord, Data};
use poise::serenity_prelude as serenity;
use std::io::Write;

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
        ingress_shadow: None,
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
                inflight_row(&ProviderKind::Claude, channel, &session, false);
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
                    tokio::time::timeout(std::time::Duration::from_secs(8), async {
                        while mailbox_holds(&shared, channel, &token).await {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    assert!(stop.finish().await.token.is_none());
                    assert!(token.is_completion_cleanup(), "natural completion must win");
                    assert_eq!(tombstone(channel), None);
                } else {
                    let request = command(&ctx, &data, channel);
                    let observe = async {
                        until(|| {
                            fx.take_calls().iter().any(|call| {
                                call.starts_with("send-keys") && call.ends_with("Escape")
                            })
                        })
                        .await;
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
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
                tokio::time::timeout(std::time::Duration::from_secs(8), async {
                    while mailbox_holds(&shared, channel, &token).await {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
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
        for (n, failure) in ["modal", "capture", "io", "unknown"]
            .into_iter()
            .enumerate()
        {
            fx.serve(Server::Live);
            fx.reject_escape(failure == "io");
            let pane = if failure == "modal" {
                "Allow\nDeny\nEnter to confirm\n❯".to_owned()
            } else {
                running()
            };
            fx.pane(&pane);
            if failure == "capture" {
                fx.serve(Server::NoSocket);
            }
            let shared = discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_193_200 + n as u64);
            let (mock, ctx, server, data) = transport(&shared, channel).await;
            let session = format!("native-stop-failed-{failure}");
            let (token, path) = generating_turn(&fx, &session);
            if failure == "unknown" {
                std::fs::write(&path, "{\"type\":\"unknown\"}\n").unwrap();
            }
            start(&shared, channel, &token).await;
            let row = inflight_row(&ProviderKind::Claude, channel, &session, false);
            let before = file_state(&row);
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
            assert_eq!(file_state(&row), before);
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
            fx.serve(Server::Live);
            fx.reject_escape(false);
            std::fs::write(&path, "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n{\"type\":\"assistant\",\"message\":{\"content\":[]}}\n").unwrap();
            let (reader, shown) = bridge(&shared, channel, &token, &session, &path);
            append(
                &path,
                serde_json::json!({"type":"assistant", "message":{"content":[{"type":"text","text":"NATIVE_READER_CONTINUES"}]}}),
            );
            append(
                &path,
                serde_json::json!({"type":"system", "subtype":"turn_duration"}),
            );
            tokio::time::timeout(std::time::Duration::from_secs(8), async {
                while mailbox_holds(&shared, channel, &token).await {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
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
                    panic!("refused old native proof");
                };
                assert_eq!(next.wire()["effect_started"], false);
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
            }
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
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
            fx.serve(Server::Live);
            fx.pane(&running());
            let shared = discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_193_070 + n as u64);
            let session = format!("native-stop-window-{change}");
            let (token, path) = generating_turn(&fx, &session);
            start(&shared, channel, &token).await;
            inflight_row(&ProviderKind::Claude, channel, &session, false);
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
                    fx.take_calls()
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
                        tokio::time::timeout(std::time::Duration::from_secs(8), async {
                            while mailbox_holds(&shared, channel, &token).await {
                                tokio::task::yield_now().await;
                            }
                        })
                        .await
                        .unwrap();
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
