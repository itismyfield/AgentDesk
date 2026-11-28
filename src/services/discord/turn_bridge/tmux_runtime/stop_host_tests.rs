//! Stop host verdicts at the stop, bind, cleanup, release and runtime-clear entry points.
#![cfg(unix)]

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use crate::services::claude_tui::host_input::InputRefusal;
use crate::services::discord::InflightRestartMode;
use crate::services::discord::TmuxCleanupPolicy;
use crate::services::discord::turn_bridge::tmux_runtime::stop_host::{
    StopTarget, interrupt_unhosted,
};
use crate::services::discord::turn_bridge::tmux_runtime::{
    bind_cancel_token_tmux_runtime, stop_active_turn, stop_active_turn_on,
};
use crate::services::provider::cancel_token_cleanup::executor::{
    TmuxCleanupIntent, pid_kill_dispatches_for_test, take_requested_intents_for_test,
    tmux_kill_dispatches_for_test, with_executor_dispatch_seam,
};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::session_host::HostKind;
use crate::services::session_host::{HostError, HostMutation, HostRefusal};

use crate::services::discord::turn_bridge::tmux_runtime::stop_host::tests::*;

// A stop on a session whose marker is not tmux sends no key, signal, PID probe or kill on any
// tmux condition, still raises the cancel flag, and asks the executor only to preserve.
#[test]
fn a_stop_on_another_host_reaches_no_tmux_signal_or_kill() {
    let fx = Fixture::new();
    let drain = InflightRestartMode::DrainRestart;
    let policies = [
        TmuxCleanupPolicy::PreserveSession,
        TmuxCleanupPolicy::PreserveSessionAndInflight {
            restart_mode: drain,
        },
        TmuxCleanupPolicy::CleanupSession {
            termination_reason_code: Some("test_stop"),
        },
    ];
    let providers = [
        ProviderKind::Claude,
        ProviderKind::Codex,
        ProviderKind::Qwen,
    ];
    with_executor_dispatch_seam(|| {
        run(async {
            let mut n = 0;
            for server in SERVERS {
                fx.serve(server);
                for host in NOT_TMUX {
                    for provider in &providers {
                        for policy in policies {
                            n += 1;
                            let name = format!("AgentDesk-{}-p6ao-stop-{n}", provider.as_str());
                            mark(&name, host);
                            let token = bound_token(provider, &name);
                            token.store_child_pid(fx.pid());
                            let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
                            let handle =
                                crate::services::session_backend::SessionHandle::TestProcess {
                                    pid: fx.pid(),
                                    alive: alive.clone(),
                                };
                            crate::services::session_backend::insert_process_session(
                                name.clone(),
                                handle,
                            );
                            take_requested_intents_for_test();

                            let recorded =
                                stop_active_turn(provider, &token, policy, "explicit_stop").await;

                            let case = format!("{server:?} {host:?} {provider:?} {policy:?}");
                            assert!(!recorded, "{case}");
                            assert_eq!(fx.take_calls(), Vec::<String>::new(), "{case}");
                            assert_eq!(fx.take_sigints(), 0, "{case}");
                            assert!(alive.load(Ordering::SeqCst), "{case}");
                            assert!(
                                !crate::services::session_backend::process_session_was_stopped(
                                    &name
                                ),
                                "{case}"
                            );
                            assert!(token.cancelled.load(Ordering::SeqCst), "{case}");
                            assert_eq!(token.restart_mode(), policy.preserves_inflight(), "{case}");
                            let intents = take_requested_intents_for_test();
                            assert_eq!(intents, vec![TmuxCleanupIntent::PreserveSession], "{case}");
                            assert_eq!(token.child_pid_value(), Some(fx.pid()), "{case}");
                            crate::services::session_backend::remove_process_session(&name);
                        }
                    }
                }
            }
            assert_eq!(pid_kill_dispatches_for_test(), 0);
            assert_eq!(tmux_kill_dispatches_for_test(), 0);

            // The same stop on a legacy session reaches the pane, so the log above saw no call.
            for server in SERVERS {
                fx.serve(server);
                let name = format!("AgentDesk-codex-p6ao-legacy-{server:?}");
                mark(&name, Mark::Absent);
                let token = bound_token(&ProviderKind::Codex, &name);
                let policy = TmuxCleanupPolicy::PreserveSession;
                stop_active_turn(&ProviderKind::Codex, &token, policy, "explicit_stop").await;
                let calls = fx.take_calls();
                assert!(
                    calls.iter().any(|call| call.starts_with("send-keys")),
                    "{server:?} {calls:?}"
                );
            }
        })
    });
}

// Every stage of a stop acts on the session it decided on: a token rebound to another name
// afterwards gets no probe, key or kill there, and the executor refuses the moved cleanup.
#[test]
fn a_stop_acts_only_on_the_session_it_decided() {
    let fx = Fixture::new();
    let cleanup = TmuxCleanupPolicy::CleanupSession {
        termination_reason_code: Some("test_stop"),
    };
    with_executor_dispatch_seam(|| {
        run(async {
            for (policy, moved) in [
                (TmuxCleanupPolicy::PreserveSession, true),
                (cleanup, true),
                (cleanup, false),
            ] {
                let first = format!(
                    "AgentDesk-codex-p6ao-first-{moved}-{}",
                    policy.should_cleanup_tmux()
                );
                let second = format!("{first}-second");
                mark(&first, Mark::Absent);
                mark(&second, Mark::Absent);
                let token = bound_token(&ProviderKind::Codex, &first);
                token.store_child_pid(fx.pid());
                let target = StopTarget::for_token(&token);
                if moved {
                    token.bind_unmanaged_session_name(&second);
                }
                let (pids, names) = (
                    pid_kill_dispatches_for_test(),
                    tmux_kill_dispatches_for_test(),
                );

                stop_active_turn_on(
                    &target,
                    &ProviderKind::Codex,
                    &token,
                    policy,
                    "explicit_stop",
                )
                .await;

                let calls = fx.take_calls();
                assert!(
                    calls
                        .iter()
                        .any(|call| call.contains(&format!("={first}:"))),
                    "{calls:?}"
                );
                assert!(
                    !calls.iter().any(|call| call.contains(&second)),
                    "{calls:?}"
                );
                let killed = (
                    pid_kill_dispatches_for_test() - pids,
                    tmux_kill_dispatches_for_test() - names,
                );
                let expected = if policy.should_cleanup_tmux() && !moved {
                    (1, 1)
                } else {
                    (0, 0)
                };
                assert_eq!(killed, expected, "{policy:?} moved={moved}");
                let _ = fx.take_sigints();
            }
        })
    });
}

// Binding a token to a session another host holds records its name but never searches that
// pane for a provider PID, so no later kill can target one; a legacy name registers as before.
#[test]
fn bind_registers_no_pid_for_another_hosts_session() {
    let fx = Fixture::new();
    for server in SERVERS {
        fx.serve(server);
        for host in NOT_TMUX {
            let name = format!("AgentDesk-codex-p6ao-bind-{server:?}-{host:?}");
            mark(&name, host);
            let token = Arc::new(CancelToken::new());
            let pid = bind_cancel_token_tmux_runtime(&ProviderKind::Codex, &token, &name, "test");
            assert_eq!(
                (pid, token.child_pid_value()),
                (None, None),
                "{server:?} {host:?}"
            );
            assert_eq!(token.tmux_session_name().as_deref(), Some(name.as_str()));
            assert_eq!(fx.take_calls(), Vec::<String>::new(), "{server:?} {host:?}");
        }
    }
    fx.serve(Server::Live);
    let name = "AgentDesk-codex-p6ao-bind-legacy";
    mark(name, Mark::Absent);
    let token = Arc::new(CancelToken::new());
    let pid = bind_cancel_token_tmux_runtime(&ProviderKind::Codex, &token, name, "test");
    assert_eq!(
        (pid, token.child_pid_value()),
        (Some(fx.pid()), Some(fx.pid()))
    );
    assert!(
        fx.take_calls()
            .iter()
            .any(|call| call.starts_with("display-message"))
    );
}

/// A legacy-bound token holding the stand-in's PID, whose marker then turns to `host`.
fn registered_then_moved(fx: &Fixture, name: &str, host: Mark) -> Arc<CancelToken> {
    fx.serve(Server::Live);
    mark(name, Mark::Absent);
    let token = Arc::new(CancelToken::new());
    bind_cancel_token_tmux_runtime(&ProviderKind::Codex, &token, name, "test");
    assert_eq!(
        token.child_pid_value(),
        Some(fx.pid()),
        "the bind registered the PID"
    );
    let _ = fx.take_calls();
    mark(name, host);
    token
}

// A PID a legacy bind registered is not killed once the session's marker stops reading tmux:
// stall cleanup, the legacy cleanup adapter and the cancel watchdog take no claim and no kill.
#[test]
fn a_registered_pid_is_not_killed_once_the_host_is_not_tmux() {
    let fx = Fixture::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    with_executor_dispatch_seam(|| {
        let mut n = 0;
        for server in SERVERS {
            for host in [Mark::Unreadable, Mark::Herdr] {
                for entry in ["stall", "adapter", "watchdog"] {
                    n += 1;
                    let name = format!("AgentDesk-codex-p6ao-registered-{n}");
                    let token = registered_then_moved(&fx, &name, host);
                    fx.serve(server);
                    let case = format!("{server:?} {host:?} {entry}");
                    match entry {
                        "stall" => {
                            crate::services::discord::stall_recovery::finalize_orphaned_clear(
                                &shared,
                                ChannelId::new(5_340_000 + n),
                                Some(token.clone()),
                                "stall_test",
                            )
                        }
                        "adapter" => token.cancel_with_tmux_cleanup(),
                        _ => {
                            token.cancelled.store(true, Ordering::SeqCst);
                            let spawn = crate::services::provider::spawn_cancel_watchdog;
                            let watchdog = spawn(Some(token.clone()), "p6ao");
                            std::thread::sleep(std::time::Duration::from_millis(350));
                            drop(watchdog);
                        }
                    }
                    assert_eq!(pid_kill_dispatches_for_test(), 0, "{case}");
                    assert_eq!(tmux_kill_dispatches_for_test(), 0, "{case}");
                    assert_eq!(token.pid_kill_claim.load(Ordering::SeqCst), 0, "{case}");
                    assert_eq!(token.name_kill_claim.load(Ordering::SeqCst), 0, "{case}");
                    assert!(token.cancelled.load(Ordering::SeqCst), "{case}");
                    assert_eq!(fx.take_calls(), Vec::<String>::new(), "{case}");
                }
            }
        }
        let token =
            registered_then_moved(&fx, "AgentDesk-codex-p6ao-registered-legacy", Mark::Absent);
        token.cancel_with_tmux_cleanup();
        let killed = (
            pid_kill_dispatches_for_test(),
            tmux_kill_dispatches_for_test(),
        );
        assert_eq!(killed, (1, 1), "a legacy registration is killed as before");
    });
}

// A stopped turn on another host's session is never released as idle from a tmux reading,
// whatever tmux answers; a legacy session tmux reports missing is idle as before.
#[test]
fn zombie_release_never_reads_another_hosts_session_as_idle() {
    let fx = Fixture::new();
    let idle = crate::services::discord::host_liveness::tui_structurally_idle;
    for server in SERVERS {
        fx.serve(server);
        for host in NOT_TMUX {
            let name = format!("AgentDesk-claude-p6ao-zombie-{server:?}-{host:?}");
            mark(&name, host);
            let token = bound_token(&ProviderKind::Claude, &name);
            assert_eq!(
                idle(&ProviderKind::Claude, &token),
                None,
                "{server:?} {host:?}"
            );
            assert_eq!(fx.take_calls(), Vec::<String>::new(), "{server:?} {host:?}");
        }
    }
    fx.serve(Server::NoSocket);
    let name = "AgentDesk-claude-p6ao-zombie-legacy";
    mark(name, Mark::Absent);
    let token = bound_token(&ProviderKind::Claude, name);
    assert_eq!(idle(&ProviderKind::Claude, &token), Some(true));
    fx.serve(Server::Live);
    assert_eq!(idle(&ProviderKind::Claude, &token), Some(false));
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        name,
        crate::services::agent_protocol::RuntimeHandoffKind::LegacyTmuxWrapper,
    )
    .unwrap();
    fx.ready(name);
    assert_eq!(idle(&ProviderKind::Claude, &token), Some(true));
}

// The runtime cancel reads no pane of another host and keeps its turn: a non-tmux session gets
// no capture or any other tmux call and is not cancelled; a legacy session is stopped as in main.
#[test]
fn runtime_cancel_reads_no_pane_of_another_host() {
    let fx = Fixture::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    run(async {
        registry
            .register("claude".to_string(), shared.clone())
            .await;
        for (n, host) in NOT_TMUX.into_iter().chain([Mark::Absent]).enumerate() {
            let channel = ChannelId::new(5_340_100 + n as u64);
            let name = format!("AgentDesk-claude-p6ao-runtime-{n}");
            mark(&name, host);
            let token = bound_token(&ProviderKind::Claude, &name);
            let start = crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel,
                token.clone(),
                UserId::new(7),
                MessageId::new(channel.get() + 1),
            );
            assert!(start.await);
            let stop = crate::services::discord::health::stop_provider_channel_runtime_with_policy;
            let policy = TmuxCleanupPolicy::PreserveSession;
            let result = stop(&registry, "claude", channel, "p6ao", policy).await;
            let kept = crate::services::engine_runtime::health_port::InflightDisposition::PreservedByHostGuard;
            let legacy = matches!(host, Mark::Absent);
            assert_eq!(result.unwrap().inflight == kept, !legacy, "{host:?}");
            assert_eq!(token.cancelled.load(Ordering::SeqCst), legacy, "{host:?}");
            let calls = fx.take_calls();
            let reached = calls.iter().any(|call| call.contains(&name));
            assert_eq!(reached, legacy, "{host:?} {calls:?}");
            assert_eq!(calls.is_empty(), !legacy, "{host:?} {calls:?}");
        }
    });
}

/// A Herdr host recording each operation, answering with the scripted capture and key result.
async fn herdr_stop(target: &StopTarget, token: &Arc<CancelToken>) {
    let policy = TmuxCleanupPolicy::PreserveSession;
    stop_active_turn_on(
        target,
        &ProviderKind::Claude,
        token,
        policy,
        "explicit_stop",
    )
    .await;
}

// A verified Herdr Claude turn takes one Escape on its own pane, never the C-c interrupt; a
// sent or possibly sent Escape spends the claim, and only a surely unsent one is retried.
#[test]
fn a_herdr_claude_stop_sends_one_escape_and_spends_the_claim_only_once_sent() {
    let fx = Fixture::new();
    let precondition = HostRefusal::Precondition("restore resume not off".to_string());
    let unsupported = HostRefusal::Unsupported {
        kind: HostKind::Herdr,
        op: "send_keys",
    };
    with_executor_dispatch_seam(|| {
        run(async {
            for (n, (keys, spent)) in [
                (Ok(HostMutation::Confirmed), true),
                (
                    Ok(HostMutation::Indeterminate("ack lost".to_string())),
                    true,
                ),
                (Ok(HostMutation::Refused(precondition.clone())), false),
                (Ok(HostMutation::Refused(unsupported.clone())), false),
                (Err(HostError::Transport("not sent".to_string())), false),
            ]
            .into_iter()
            .enumerate()
            {
                let session = format!("AgentDesk-claude-p6ao-herdr-{n}");
                let (token, _) = generating_turn(&fx, &session);
                let host = FakeHerdr::new(keys.clone());
                let target = herdr_target(&ProviderKind::Claude, &session, host.clone(), Ok(()));
                let confirmed = matches!(keys, Ok(HostMutation::Confirmed));
                let outcome =
                    interrupt_unhosted(&target, &ProviderKind::Claude, &token, "explicit_stop")
                        .await;
                assert_eq!(outcome.sent_keys, confirmed, "{keys:?}");
                herdr_stop(&target, &token).await;
                let ops = host.ops.lock().unwrap().clone();
                assert!(!ops.iter().any(|op| op.starts_with("interrupt")), "{ops:?}");
                let expected = if spent { 1 } else { 2 };
                assert_eq!(host.escapes(), expected, "{keys:?}: {ops:?}");
                assert_eq!(fx.take_calls(), Vec::<String>::new());
                crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
            }
            assert_eq!(fx.take_sigints(), 0);
            assert_eq!(pid_kill_dispatches_for_test(), 0);
            assert_eq!(tmux_kill_dispatches_for_test(), 0);
        })
    });
}

// No Escape leaves for a Herdr turn the gate refuses, whose pane cannot be read or whose
// transcript moved to a newer turn; each keeps the claim for a later stop.
#[test]
fn a_herdr_claude_stop_writes_nothing_unless_every_fence_passes() {
    let fx = Fixture::new();
    run(async {
        let session = "AgentDesk-claude-p6ao-herdr-gate";
        let (token, _) = generating_turn(&fx, session);
        let host = FakeHerdr::new(Ok(HostMutation::Confirmed));
        let refused = herdr_target(
            &ProviderKind::Claude,
            session,
            host.clone(),
            Err(InputRefusal::Unknown),
        );
        herdr_stop(&refused, &token).await;
        assert_eq!(host.escapes(), 0, "the gate refused");
        let admitted = herdr_target(&ProviderKind::Claude, session, host.clone(), Ok(()));
        herdr_stop(&admitted, &token).await;
        assert_eq!(host.escapes(), 1, "the refused stop kept the claim");

        let session = "AgentDesk-claude-p6ao-herdr-capture";
        let (token, _) = generating_turn(&fx, session);
        let blind = Arc::new(FakeHerdr {
            ops: Mutex::default(),
            capture: Err(HostError::Timeout),
            keys: Ok(HostMutation::Confirmed),
        });
        herdr_stop(
            &herdr_target(&ProviderKind::Claude, session, blind.clone(), Ok(())),
            &token,
        )
        .await;
        assert_eq!(blind.escapes(), 0, "an unread pane is ambiguous");

        let session = "AgentDesk-claude-p6ao-herdr-unbound";
        let token = bound_token(&ProviderKind::Claude, session);
        let host = FakeHerdr::new(Ok(HostMutation::Confirmed));
        herdr_stop(
            &herdr_target(&ProviderKind::Claude, session, host.clone(), Ok(())),
            &token,
        )
        .await;
        assert_eq!(host.escapes(), 0, "no binding, no phase");

        let session = "AgentDesk-claude-p6ao-herdr-newer";
        let (token, transcript) = generating_turn(&fx, session);
        let host = FakeHerdr::new(Ok(HostMutation::Confirmed));
        let target = herdr_target(&ProviderKind::Claude, session, host.clone(), Ok(()));
        let (parked, release) = (
            Arc::new(std::sync::Barrier::new(2)),
            Arc::new(std::sync::Barrier::new(2)),
        );
        let (held, go) = (parked.clone(), release.clone());
        let holder = std::thread::spawn(move || {
            crate::services::claude_tui::composer_lock::with_composer_mutation_lock(session, || {
                held.wait();
                go.wait();
            })
        });
        parked.wait();
        let stop = tokio::spawn({
            let (target, token) = (target.clone(), token.clone());
            async move { herdr_stop(&target, &token).await }
        });
        while !host
            .ops
            .lock()
            .unwrap()
            .iter()
            .any(|op| op.starts_with("capture"))
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let newer =
            serde_json::json!({"type": "user", "message": {"role": "user", "content": "next"}});
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        writeln!(file, "{newer}").unwrap();
        release.wait();
        holder.join().unwrap();
        stop.await.unwrap();
        assert_eq!(
            host.escapes(),
            0,
            "a newer turn landed while the stop waited"
        );
        assert_eq!(fx.take_calls(), Vec::<String>::new());
    });
}

// Codex and Qwen on Herdr are refused before any I/O: no host, E7 or tmux call.
#[test]
fn a_herdr_stop_for_another_provider_is_refused_before_any_io() {
    let fx = Fixture::new();
    run(async {
        for provider in [ProviderKind::Codex, ProviderKind::Qwen] {
            let session = format!("AgentDesk-{}-p6ao-herdr", provider.as_str());
            let token = bound_token(&provider, &session);
            let host = FakeHerdr::new(Ok(HostMutation::Confirmed));
            let target = herdr_target(&provider, &session, host.clone(), Ok(()));
            assert!(matches!(target, StopTarget::Refused { .. }), "{provider:?}");
            let policy = TmuxCleanupPolicy::PreserveSession;
            stop_active_turn_on(&target, &provider, &token, policy, "explicit_stop").await;
            assert!(host.ops.lock().unwrap().is_empty(), "{provider:?}");
            assert!(token.cancelled.load(Ordering::SeqCst));
            assert_eq!(fx.take_calls(), Vec::<String>::new());
        }
    });
}

/// A channel `channel_name` maps to, holding a provider session, history and a busy turn
/// bound to its tmux name, plus a live process session the reset would kill.
async fn seeded_runtime_channel(
    shared: &crate::services::discord::SharedData,
    channel: ChannelId,
    channel_name: &str,
    pid: u32,
) -> (String, Arc<CancelToken>, Arc<std::sync::atomic::AtomicBool>) {
    use crate::services::discord::host_defer_gate::tests::map_channel;
    map_channel(shared, channel, channel_name).await;
    let name = ProviderKind::Claude.build_tmux_session_name(channel_name);
    let mut core = shared.core.lock().await;
    let session = core.sessions.get_mut(&channel).unwrap();
    session.session_id = Some("sid".into());
    session.history.push(crate::ui::ai_screen::HistoryItem {
        item_type: crate::ui::ai_screen::HistoryType::User,
        content: "kept".into(),
    });
    drop(core);
    let token = bound_token(&ProviderKind::Claude, &name);
    let start = crate::services::discord::mailbox_try_start_turn(
        shared,
        channel,
        token.clone(),
        UserId::new(7),
        MessageId::new(channel.get() + 1),
    );
    assert!(start.await, "the seeded turn starts");
    let alive = crate::services::discord::admin_host_guard::tests::process(&name, pid);
    (name, token, alive)
}

/// Whether the channel still holds its session, history, uncleared flag and busy turn.
async fn runtime_kept(
    shared: &crate::services::discord::SharedData,
    channel: ChannelId,
    token: &CancelToken,
) -> bool {
    let snapshot = crate::services::discord::mailbox_snapshot(shared, channel).await;
    let core = shared.core.lock().await;
    let session = core.sessions.get(&channel).unwrap();
    session.session_id.as_deref() == Some("sid")
        && session.history.len() == 1
        && !session.cleared
        && snapshot.cancel_token.is_some()
        && !token.cancelled.load(Ordering::SeqCst)
}

// A runtime clear of a non-legacy session changes nothing, by routine reset or by an auto-queue
// slot clear the safety filter let through; a legacy session clears and kills as in main.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_clear_refuses_another_hosts_session_before_any_change_pg() {
    use crate::services::discord::health::routine_ports::build_routine_session_controller;
    use crate::services::discord::health::slot_row_host_refusal;
    use crate::services::discord::host_defer_gate::tests::{Case, postgres};
    use crate::services::discord::host_teardown_gate::test_support::{
        Stored, busy_turn, channel_key, runtime, seed,
    };
    use crate::services::routines::session_control::{
        RoutineSessionCommand::Reset, seed_started_control_session,
    };
    let fx = Fixture::new();
    let (db, pool) = postgres().await;
    let (shared, registry) = runtime(&pool).await;
    let controller =
        build_routine_session_controller(Arc::new(pool.clone()), Some(registry.clone()));
    for (n, case) in Case::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_302_534_000 + n as u64);
        let (agent, channel_name) = (format!("agent-p6ao-{n}"), format!("p6ao-reset-{n}"));
        sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_cc) VALUES ($1, $1, 'claude', $2)")
            .bind(&agent)
            .bind(channel.get().to_string())
            .execute(&pool)
            .await
            .unwrap();
        let pid = 65_000 + n as u32;
        let (name, token, alive) =
            seeded_runtime_channel(&shared, channel, &channel_name, pid).await;
        let key = channel_key(&shared, &name);
        case.seed(&pool, &key, &name, channel.get()).await;
        sqlx::query("UPDATE sessions SET agent_id=$1 WHERE session_key=$2")
            .bind(&agent)
            .bind(&key)
            .execute(&pool)
            .await
            .unwrap();
        let active = shared.restart.global_active.load(Ordering::SeqCst);
        let _ = fx.take_calls();
        let routine = crate::services::routines::store::RoutineRecord {
            id: format!("routine-p6ao-{n}"),
            agent_id: Some(agent),
            fallback_agent_id: None,
            max_retries: 0,
            script_ref: "script".into(),
            name: "p6ao".into(),
            status: "enabled".into(),
            execution_strategy: "persistent".into(),
            schedule: None,
            next_due_at: None,
            last_run_at: None,
            last_result: None,
            checkpoint: None,
            discord_thread_id: None,
            timeout_secs: None,
            in_flight_run_id: None,
            pause_reason: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        seed_started_control_session(&pool, &routine, "claude", &channel.to_string(), &key).await;
        let result = controller
            .control_persistent_session(&routine, Reset, "p6ao", Some(registry.as_ref()))
            .await;
        if !case.has_row() {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("session is not provable"),
                "{case:?}"
            );
            assert!(runtime_kept(&shared, channel, &token).await, "{case:?}");
            assert!(alive.load(Ordering::SeqCst), "{case:?}");
            assert!(fx.take_calls().is_empty(), "{case:?}");
            crate::services::session_backend::remove_process_session(&name);
            continue;
        }
        let result = result.unwrap();
        if case.admitted() {
            assert_eq!(result.lifecycle_path, "runtime-clear", "{case:?}");
            assert!(!alive.load(Ordering::SeqCst), "{case:?}: main kills");
            continue;
        }
        assert_eq!(result.lifecycle_path, "runtime-clear-refused", "{case:?}");
        assert!(!result.runtime_cleared, "{case:?}");
        assert!(runtime_kept(&shared, channel, &token).await, "{case:?}");
        assert_eq!(
            shared.restart.global_active.load(Ordering::SeqCst),
            active,
            "{case:?}"
        );
        assert!(
            alive.load(Ordering::SeqCst),
            "{case:?}: the process is kept"
        );
        assert_eq!(fx.take_calls(), Vec::<String>::new(), "{case:?}");
        crate::services::session_backend::remove_process_session(&name);
    }

    // One slot, three threads: the slot filter keeps both hosted threads from any change and
    // only the legacy third reaches the runtime clear.
    sqlx::query(
        "INSERT INTO agents (id, name, provider) VALUES ('agent-p6ao-slot', 'slot', 'claude')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let threads: Vec<ChannelId> = (0..3)
        .map(|n| ChannelId::new(1_479_671_302_535_000 + n))
        .collect();
    let map: serde_json::Map<String, serde_json::Value> = threads
        .iter()
        .enumerate()
        .map(|(n, thread)| (n.to_string(), serde_json::json!(thread.get().to_string())))
        .collect();
    sqlx::query("INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map) VALUES ('agent-p6ao-slot', 0, $1)")
        .bind(serde_json::Value::Object(map))
        .execute(&pool)
        .await
        .unwrap();
    let mut seeded = Vec::new();
    for (n, (thread, stored)) in threads
        .iter()
        .zip([Stored::Hosted, Stored::Hosted, Stored::Legacy])
        .enumerate()
    {
        let channel_name = format!("p6ao-slot-{n}");
        let (name, token, alive) = if n == 0 {
            crate::services::discord::host_defer_gate::tests::map_channel(
                &shared,
                *thread,
                &channel_name,
            )
            .await;
            let name = ProviderKind::Claude.build_tmux_session_name(&channel_name);
            let token = busy_turn(&shared, *thread, &name).await;
            let alive = crate::services::discord::admin_host_guard::tests::process(&name, 66_000);
            (name, token, alive)
        } else {
            seeded_runtime_channel(&shared, *thread, &channel_name, 66_000 + n as u32).await
        };
        let key = channel_key(&shared, &name);
        seed(&pool, &key, &name, thread.get(), stored).await;
        sqlx::query("UPDATE sessions SET thread_channel_id = $2 WHERE session_key = $1")
            .bind(&key)
            .bind(thread.get().to_string())
            .execute(&pool)
            .await
            .unwrap();
        seeded.push((name, token, alive));
    }
    let _ = fx.take_calls();
    crate::services::discord::install_engine_probes();
    let clear = crate::services::auto_queue::runtime::clear_slot_threads_for_slot_pg;
    clear(
        slot_row_host_refusal,
        Some(registry.clone()),
        &pool,
        "agent-p6ao-slot",
        0,
    )
    .await
    .unwrap();
    let ids: Vec<u64> = threads.iter().map(|thread| thread.get()).collect();
    let after = crate::ke_assembly_tests::slot_reset_host_pg_tests::runtime_clears_after_done;
    let clears = after(&ids).await;
    assert!(
        matches!(clears.as_slice(), [(thread, Ok(_))] if *thread == ids[2]),
        "{clears:?}"
    );
    assert!(runtime_kept(&shared, threads[1], &seeded[1].1).await);
    assert!(
        seeded[1].2.load(Ordering::SeqCst),
        "the refused thread's process is kept"
    );
    assert!(
        !seeded[2].2.load(Ordering::SeqCst),
        "the legacy thread is killed as in main"
    );
    for (name, _, _) in &seeded {
        crate::services::session_backend::remove_process_session(name);
    }
    pool.close().await;
    db.drop().await;
}
