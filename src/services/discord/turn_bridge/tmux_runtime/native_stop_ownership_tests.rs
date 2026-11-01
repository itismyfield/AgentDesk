use super::*;
use crate::services::discord::turn_bridge::bridge_entry_persist::entry_abort_mailbox_tests::seed_context;
use crate::services::discord::turn_bridge::cancel_backstop_test_support::BridgePause;
use crate::services::discord::turn_finalizer::{FinalizeContext, TerminalEvent, TurnKey};
use crate::services::discord::{self as discord, inflight::RelayOwnerKind};
use std::time::Duration;

type Step = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
thread_local! {
    static AFTER_NATIVE: std::cell::RefCell<Option<Step>> = const { std::cell::RefCell::new(None) };
}

pub(in super::super) async fn after_native() {
    let step = AFTER_NATIVE.with_borrow_mut(Option::take);
    if let Some(step) = step {
        step.await;
    }
}

async fn paused_bridge(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    token: &Arc<CancelToken>,
    direct: bool,
) -> (
    BridgePause,
    std::sync::mpsc::Sender<crate::services::agent_protocol::StreamMessage>,
) {
    let row =
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
    let mut context = seed_context("", row);
    context.provider = ProviderKind::Claude;
    context.is_external_input_tui_direct = direct;
    let pause = BridgePause::install(channel);
    let (tx, rx) = std::sync::mpsc::channel();
    discord::turn_bridge::spawn_turn_bridge(shared.clone(), token.clone(), rx, context);
    assert!(pause.wait_for_entry(Duration::from_secs(8)).await);
    assert!(
        discord::live_bridge::retain_original(&ProviderKind::Claude, channel.get(), token)
            .is_none()
    );
    (pause, tx)
}

async fn native_finished(shared: &Arc<SharedData>, channel: ChannelId, token: &Arc<CancelToken>) {
    let result = begin_user_stop(shared, &ProviderKind::Claude, channel, true, "!stop").await;
    let CommandStop::Native(stop) = result else {
        panic!("natural completion must remain Native, never publish Stopped");
    };
    assert_eq!(stop.lifecycle_path(), "native-stop-completed");
    assert!(stop.finish().await.token.is_none());
    assert!(!token.cancelled.load(Ordering::Acquire));
    assert!(mailbox_holds(shared, channel, token).await);
    assert_eq!(tombstone(channel), None);
}

#[test]
fn a1_recovery_and_direct_live_bridge_survive_row_removal() {
    let fx = Fixture::new();
    run(async {
        for direct in [false, true] {
            let shared = discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_212_100 + u64::from(direct));
            let session = format!("ownership-live-{direct}");
            let (original, _) = generating_turn(&fx, &session);
            let token = if direct {
                original
            } else {
                let restored = Arc::new(CancelToken::from_persisted_turn_nonce(
                    original.turn_nonce().map(str::to_owned),
                ));
                restored.bind_claude_tmux_session(&session);
                restored
            };
            if direct {
                start(&shared, channel, &token).await;
            } else {
                assert!(
                    discord::queue_io::mailbox_recovery_kickoff(
                        &shared,
                        channel,
                        token.clone(),
                        UserId::new(7),
                        Some(MessageId::new(channel.get() + 1))
                    )
                    .await
                    .activated_turn()
                );
            }
            let row = native_inflight_row(channel, &token, &session);
            let (pause, _tx) = paused_bridge(&shared, channel, &token, direct).await;
            token.native_stop.completed();
            std::fs::remove_file(row).unwrap();
            native_finished(&shared, channel, &token).await;
            assert!(pause.abort_and_wait(Duration::from_secs(8)).await);
            assert!(matches!(
                begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await,
                CommandStop::Stop(_)
            ));
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}

#[test]
fn a2_completed_zombie_nonce_does_not_own_after_bridge_abort() {
    let fx = Fixture::new();
    run(async {
        let shared = discord::make_shared_data_for_tests();
        let channel = ChannelId::new(7_212_200);
        let session = "ownership-zombie";
        let (token, _) = generating_turn(&fx, session);
        start(&shared, channel, &token).await;
        let row = native_inflight_row(channel, &token, session);
        let (pause, _tx) = paused_bridge(&shared, channel, &token, false).await;
        token.native_stop.completed();
        assert!(pause.abort_and_wait(Duration::from_secs(8)).await);
        assert!(
            !shared
                .turn_finalizer
                .has_live_watcher_pending(channel, shared.restart.current_generation)
                .await
        );
        let before = file_state(&row);
        assert!(
            matches!(
                begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await,
                CommandStop::Stop(_)
            ),
            "a matching zombie nonce must return to main recovery"
        );
        assert!(token.cancelled.load(Ordering::Acquire));
        assert_eq!(file_state(&row), before);
        assert!(tombstone(channel).is_some());
        assert!(
            !fx.take_calls()
                .iter()
                .any(|call| call.starts_with("send-keys"))
        );
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
    });
}

#[test]
fn a2b_watcher_pending_preserves_done_with_and_without_row() {
    let fx = Fixture::new();
    run(async {
        for row_present in [true, false] {
            let shared = discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_212_300 + u64::from(row_present));
            let session = format!("ownership-watcher-{row_present}");
            let (token, _) = generating_turn(&fx, &session);
            start(&shared, channel, &token).await;
            let row = native_inflight_row(channel, &token, &session);
            let (pause, _tx) = paused_bridge(&shared, channel, &token, false).await;
            token.native_stop.completed();
            let key = TurnKey::new(
                channel,
                channel.get() + 1,
                shared.restart.current_generation,
            )
            .with_episode_nonce(token.turn_nonce());
            shared.turn_finalizer.register_start(
                key,
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                &shared,
            );
            assert!(
                shared
                    .turn_finalizer
                    .has_live_watcher_pending(channel, shared.restart.current_generation)
                    .await
            );
            assert!(pause.abort_and_wait(Duration::from_secs(8)).await);
            if !row_present {
                std::fs::remove_file(row).unwrap();
            }
            native_finished(&shared, channel, &token).await;
            let _ = shared
                .turn_finalizer
                .submit_terminal(
                    key,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                !shared
                    .turn_finalizer
                    .has_live_watcher_pending(channel, shared.restart.current_generation)
                    .await
            );
            assert!(!mailbox_holds(&shared, channel, &token).await);
            assert_eq!(tombstone(channel), None);
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}

#[test]
fn a3_late_binding_rejudges_native_on_same_actor_once() {
    let fx = Fixture::new();
    fx.pane(&native_contract::running());
    run(async {
        let shared = discord::make_shared_data_for_tests();
        let channel = ChannelId::new(7_212_400);
        let session = "ownership-late-bind";
        let (_unused, _) = generating_turn(&fx, session);
        let token = Arc::new(CancelToken::new());
        start(&shared, channel, &token).await;
        native_inflight_row(channel, &token, session);
        let bound = token.clone();
        BEFORE_CANCEL.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move || bound.bind_claude_tmux_session(session)))
        });
        let result = begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await;
        let CommandStop::Native(stop) = result else {
            panic!("same actor late binding must receive Native rejudgement");
        };
        assert_eq!(stop.lifecycle_path(), "native-stop-pending");
        assert!(!token.cancelled.load(Ordering::Acquire));
        assert_eq!(
            fx.take_calls()
                .iter()
                .filter(|call| call.starts_with("send-keys") && call.ends_with("Escape"))
                .count(),
            1
        );
        assert_eq!(tombstone(channel), None);
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
    });
}

#[test]
fn a3b_late_binding_cannot_retarget_replaced_actor_even_with_same_token() {
    let fx = Fixture::new();
    fx.pane(&native_contract::running());
    run(async {
        let shared = discord::make_shared_data_for_tests();
        for late_binding in [false, true] {
            let channel = ChannelId::new(7_212_500 + u64::from(late_binding));
            let session = if late_binding {
                "ownership-new-actor-bound"
            } else {
                "ownership-new-actor-unbound"
            };
            let (_unused, _) = generating_turn(&fx, session);
            let token = Arc::new(CancelToken::new());
            start(&shared, channel, &token).await;
            native_inflight_row(channel, &token, session);
            let (other_shared, other_token) = (shared.clone(), token.clone());
            AFTER_NATIVE.with_borrow_mut(|step| {
                *step = Some(Box::pin(async move {
                    other_shared.mailboxes.remove_fixture_for_test(channel);
                    start(&other_shared, channel, &other_token).await;
                    if late_binding {
                        other_token.bind_claude_tmux_session(session);
                    }
                }))
            });
            let result =
                begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await;
            assert!(
                matches!(result, CommandStop::NoActiveTurn),
                "replacement actor cannot receive native rejudgement"
            );
            assert!(!token.cancelled.load(Ordering::Acquire));
            assert!(mailbox_holds(&shared, channel, &token).await);
            assert!(
                !fx.take_calls()
                    .iter()
                    .any(|call| call.starts_with("send-keys"))
            );
            assert_eq!(tombstone(channel), None);
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
        }
    });
}

#[test]
fn a2b_bridge_to_watcher_race_checks_bridge_before_watcher() {
    let fx = Fixture::new();
    run(async {
        let shared = discord::make_shared_data_for_tests();
        let channel = ChannelId::new(7_212_320);
        let session = "ownership-watcher-race";
        let (token, _) = generating_turn(&fx, session);
        start(&shared, channel, &token).await;
        native_inflight_row(channel, &token, session);
        let (pause, _tx) = paused_bridge(&shared, channel, &token, false).await;
        token.native_stop.completed();
        let (other_shared, other_token) = (shared.clone(), token.clone());
        super::super::super::native_stop::AFTER_BRIDGE_PROBE.with_borrow_mut(|step| {
            *step = Some(Box::pin(async move {
                let generation = other_shared.restart.current_generation;
                assert!(
                    !other_shared
                        .turn_finalizer
                        .has_live_watcher_pending(channel, generation)
                        .await
                );
                let key = TurnKey::new(channel, channel.get() + 1, generation)
                    .with_episode_nonce(other_token.turn_nonce());
                other_shared.turn_finalizer.register_start(
                    key,
                    ProviderKind::Claude,
                    RelayOwnerKind::Watcher,
                    &other_shared,
                );
                assert!(
                    other_shared
                        .turn_finalizer
                        .has_live_watcher_pending(channel, generation)
                        .await
                );
                assert!(pause.abort_and_wait(Duration::from_secs(8)).await);
            }));
        });
        native_finished(&shared, channel, &token).await;
        assert!(
            !fx.take_calls()
                .iter()
                .any(|call| call.starts_with("send-keys"))
        );
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
    });
}

#[test]
fn a2b_committed_terminal_protects_only_its_exact_episode() {
    let fx = Fixture::new();
    run(async {
        for same in [true, false] {
            let shared = discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_212_330 + u64::from(same));
            let session = format!("ownership-committed-{same}");
            let (token, _) = generating_turn(&fx, &session);
            start(&shared, channel, &token).await;
            native_inflight_row(channel, &token, &session);
            let mut row =
                inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get())
                    .unwrap();
            row.terminal_delivery_committed = true;
            if !same {
                row.turn_nonce = Some("successor-episode".into());
            }
            inflight::save_inflight_state(&row).unwrap();
            token.native_stop.completed();
            if same {
                native_finished(&shared, channel, &token).await;
            } else {
                assert!(matches!(
                    begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await,
                    CommandStop::Stop(_)
                ));
                assert!(token.cancelled.load(Ordering::Acquire));
            }
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}

#[test]
fn a3b_late_binding_cannot_retarget_successor_token_or_replaced_incarnation() {
    let fx = Fixture::new();
    fx.pane(&native_contract::running());
    run(async {
        for (n, change) in ["token", "inode", "spawn"].into_iter().enumerate() {
            let shared = discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7_212_550 + n as u64);
            let session = format!("ownership-successor-{change}");
            let (_unused, path) = generating_turn(&fx, &session);
            let token = Arc::new(CancelToken::new());
            start(&shared, channel, &token).await;
            native_inflight_row(channel, &token, &session);
            let marker = std::path::PathBuf::from(crate::services::tmux_common::session_temp_path(
                &session,
                "spawn_nonce",
            ));
            std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
            std::fs::write(&marker, "before").unwrap();
            let successor = Arc::new(CancelToken::new());
            let (other_shared, other_token, other_successor, name) = (
                shared.clone(),
                token.clone(),
                successor.clone(),
                session.clone(),
            );
            AFTER_NATIVE.with_borrow_mut(|step| {
                *step = Some(Box::pin(async move {
                    if change == "token" {
                        discord::mailbox_finish_turn(&other_shared, &ProviderKind::Claude, channel)
                            .await;
                        other_successor.bind_claude_tmux_session(&name);
                        start(&other_shared, channel, &other_successor).await;
                    } else {
                        if change == "inode" {
                            let old = path.with_extension("old");
                            std::fs::rename(&path, &old).unwrap();
                            std::fs::copy(old, path).unwrap();
                        } else {
                            std::fs::write(marker, "after").unwrap();
                        }
                        other_token.bind_claude_tmux_session(&name);
                    }
                }))
            });
            assert!(matches!(
                begin_user_stop(&shared, &ProviderKind::Claude, channel, true, "!stop").await,
                CommandStop::NoActiveTurn
            ));
            if change != "token" {
                assert!(!token.cancelled.load(Ordering::Acquire));
            }
            assert!(!successor.cancelled.load(Ordering::Acquire));
            assert!(
                mailbox_holds(
                    &shared,
                    channel,
                    if change == "token" {
                        &successor
                    } else {
                        &token
                    }
                )
                .await
            );
            assert!(
                !fx.take_calls()
                    .iter()
                    .any(|call| call.starts_with("send-keys"))
            );
            assert_eq!(tombstone(channel), None);
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}

#[test]
fn a1_overlapping_bridge_lifetimes_keep_the_remaining_owner() {
    let state = crate::services::provider::native_stop::State::default();
    let first = state.register_bridge();
    let second = state.register_bridge();
    drop(second);
    assert!(state.has_live_bridge());
    drop(first);
    assert!(!state.has_live_bridge());
}

#[test]
fn a2b_terminal_commit_between_native_and_publication_keeps_done() {
    let fx = Fixture::new();
    run(async {
        let shared = discord::make_shared_data_for_tests();
        let channel = ChannelId::new(7_212_340);
        let session = "ownership-late-terminal";
        let (_source, _) = generating_turn(&fx, session);
        let token = Arc::new(CancelToken::new());
        start(&shared, channel, &token).await;
        native_inflight_row(channel, &token, session);
        token.native_stop.completed();
        let judged = ChannelStop::judge(&shared, &ProviderKind::Claude, channel, None, true)
            .await
            .unwrap()
            .unwrap();
        assert!(judged.native("!stop").await.is_none());
        let mut row =
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
        row.terminal_delivery_committed = true;
        inflight::save_inflight_state(&row).unwrap();
        assert!(
            matches!(judged.cancel_user().await, StopCancel::NotCurrent),
            "a committed natural terminal must veto a late recovery publication"
        );
        assert!(!token.cancelled.load(Ordering::Acquire));
        assert_eq!(tombstone(channel), None);
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
    });
}

#[test]
fn a2b_watcher_query_rechecks_row_episode_nonce() {
    let fx = Fixture::new();
    run(async {
        let shared = discord::make_shared_data_for_tests();
        let channel = ChannelId::new(7_212_350);
        let session = "ownership-row-nonce-race";
        let (token, _) = generating_turn(&fx, session);
        start(&shared, channel, &token).await;
        native_inflight_row(channel, &token, session);
        let (pause, _tx) = paused_bridge(&shared, channel, &token, false).await;
        token.native_stop.completed();
        assert!(pause.abort_and_wait(Duration::from_secs(8)).await);
        super::super::super::native_stop::AFTER_BRIDGE_PROBE.with_borrow_mut(|step| {
            *step = Some(Box::pin(async move {
                let mut row =
                    inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get())
                        .unwrap();
                row.turn_nonce = Some("successor-after-owner-probe".into());
                inflight::save_inflight_state(&row).unwrap();
            }));
        });
        native_finished(&shared, channel, &token).await;
        assert!(
            !fx.take_calls()
                .iter()
                .any(|call| call.starts_with("send-keys"))
        );
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
    });
}

#[test]
fn a3b_actor_replaced_during_last_token_probe_never_receives_escape() {
    let fx = Fixture::new();
    fx.pane(&native_contract::running());
    run(async {
        let shared = discord::make_shared_data_for_tests();
        let channel = ChannelId::new(7_212_560);
        let session = "ownership-actor-probe-race";
        let (token, _) = generating_turn(&fx, session);
        start(&shared, channel, &token).await;
        native_inflight_row(channel, &token, session);
        let (other_shared, other_token) = (shared.clone(), token.clone());
        super::super::super::native_stop::AFTER_TOKEN_PROBE.with_borrow_mut(|step| {
            *step = Some(Box::pin(async move {
                super::super::super::native_stop::AFTER_TOKEN_PROBE.with_borrow_mut(|step| {
                    *step = Some(Box::pin(async move {
                        other_shared.mailboxes.remove_fixture_for_test(channel);
                        start(&other_shared, channel, &other_token).await;
                    }));
                });
            }));
        });
        native_finished(&shared, channel, &token).await;
        assert!(
            !fx.take_calls()
                .iter()
                .any(|call| call.starts_with("send-keys")),
            "a replacement actor must not receive Escape after the token reply"
        );
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
    });
}
