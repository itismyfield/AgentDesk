//! Judged channel stops at their production entries: the judge writes nothing, a refused host
//! keeps its turn, and the cancel, bind and tombstone act on what the judge read.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::super::stop_host::tests::{
    FakeHerdr, Fixture, Mark, NOT_TMUX, SERVERS, Server, bound_token, generating_turn,
    herdr_target, mark, run,
};
use super::*;
use crate::services::discord::health::{HealthRegistry, InflightDisposition};
use crate::services::session_host::HostMutation;
use crate::services::turn_lifecycle::TurnLifecycleTarget;

async fn runtime() -> (Arc<SharedData>, Arc<HealthRegistry>) {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let registry = Arc::new(HealthRegistry::new());
    registry
        .register("claude".to_string(), shared.clone())
        .await;
    (shared, registry)
}

async fn start(shared: &SharedData, channel: ChannelId, token: &Arc<CancelToken>) {
    let user_msg = MessageId::new(channel.get() + 1);
    let start = crate::services::discord::mailbox_try_start_turn;
    assert!(start(shared, channel, token.clone(), UserId::new(7), user_msg).await);
}

/// Saves `provider`'s inflight row on `channel` naming `name`; with `compat`, as an older build
/// stored it with no finalizer id, so a writable load backfills and saves it again.
fn inflight_row(provider: &ProviderKind, channel: ChannelId, name: &str, compat: bool) -> PathBuf {
    let user_msg = channel.get() + 1;
    let row = inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        1,
        user_msg,
        user_msg + 1,
        "judged stop fixture".to_string(),
        None,
        Some(name.to_string()),
        None,
        Some("fifo".to_string()),
        0,
    );
    inflight::save_inflight_state_create_new(&row).expect("persist the inflight row");
    let root = inflight::inflight_runtime_root().expect("inflight root");
    let path = inflight::inflight_state_path(&root, provider, channel.get());
    if compat {
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut raw: serde_json::Value = serde_json::from_str(&raw).unwrap();
        raw.as_object_mut().unwrap().remove("finalizer_turn_id");
        std::fs::write(&path, raw.to_string()).unwrap();
    }
    path
}

/// The row's bytes and modification time, which any save changes.
fn file_state(path: &PathBuf) -> Option<(Vec<u8>, std::time::SystemTime)> {
    let bytes = std::fs::read(path).ok()?;
    Some((bytes, std::fs::metadata(path).ok()?.modified().ok()?))
}

fn tombstone(channel: ChannelId) -> Option<Option<String>> {
    let stop = crate::services::discord::tmux::recent_turn_stop_for_channel(channel);
    stop.map(|stop| stop.tmux_session_name)
}

async fn mailbox_holds(shared: &SharedData, channel: ChannelId, token: &Arc<CancelToken>) -> bool {
    let snapshot = crate::services::discord::mailbox_snapshot(shared, channel).await;
    snapshot
        .cancel_token
        .is_some_and(|held| Arc::ptr_eq(&held, token))
}

// A preserve stop on another host's turn, named or found by its row, is kept before any write
// on every tmux condition: no tombstone, cancel, backfill or tmux call; a legacy turn stops.
#[test]
fn a_preserve_stop_keeps_a_turn_on_another_host_before_any_write() {
    let fx = Fixture::new();
    run(async {
        let (shared, registry) = runtime().await;
        let mut n = 0;
        for server in SERVERS {
            fx.serve(server);
            let hosts = NOT_TMUX.into_iter().chain([Mark::Absent]);
            for (host, named) in hosts.flat_map(|host| [(host, true), (host, false)]) {
                n += 1;
                let channel = ChannelId::new(5_340_610_000 + n * 10);
                let name = format!("AgentDesk-claude-p6asb-tl-{n}");
                mark(&name, host);
                let token = bound_token(&ProviderKind::Claude, &name);
                start(&shared, channel, &token).await;
                let row = inflight_row(&ProviderKind::Claude, channel, &name, true);
                let before = file_state(&row);
                let _ = fx.take_calls();
                let target = TurnLifecycleTarget {
                    provider: Some(ProviderKind::Claude),
                    channel_id: Some(channel),
                    tmux_name: if named { name.clone() } else { String::new() },
                };
                let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;
                let result = stop(Some(&registry), &target, "p6asb").await;

                let case = format!("{server:?} {host:?} named={named}");
                let legacy = matches!(host, Mark::Absent);
                assert_eq!(result.host_guard_kept(), !legacy, "{case}");
                assert_eq!(token.cancelled.load(Ordering::SeqCst), legacy, "{case}");
                if legacy {
                    continue;
                }
                assert!(mailbox_holds(&shared, channel, &token).await, "{case}");
                assert_eq!(tombstone(channel), None, "{case}");
                assert_eq!(file_state(&row), before, "{case}: no backfill");
                assert_eq!(fx.take_calls(), Vec::<String>::new(), "{case}");
            }
        }
    });
}

// A user stop leaves another host's turn and row untouched; an admitted unbound turn is judged,
// cancelled and bound (not stopped here), tombstoned under its row's name, its row unsaved.
#[test]
fn a_command_stop_judges_before_any_write_and_binds_what_it_judged() {
    let fx = Fixture::new();
    run(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let mut n = 0;
        for server in SERVERS {
            fx.serve(server);
            for host in NOT_TMUX {
                n += 1;
                let channel = ChannelId::new(5_340_620_000 + n * 10);
                let name = format!("AgentDesk-claude-p6asb-cmd-refused-{n}");
                mark(&name, host);
                let token = Arc::new(CancelToken::new());
                start(&shared, channel, &token).await;
                let row = inflight_row(&ProviderKind::Claude, channel, &name, true);
                let before = file_state(&row);
                let _ = fx.take_calls();

                let stop = begin_command_stop(&shared, &ProviderKind::Claude, channel, true).await;

                let case = format!("{server:?} {host:?}");
                assert!(matches!(stop, CommandStop::HostRefused), "{case}");
                assert!(!token.cancelled.load(Ordering::SeqCst), "{case}");
                assert!(mailbox_holds(&shared, channel, &token).await, "{case}");
                assert_eq!(token.tmux_session_name(), None, "{case}: not bound");
                assert_eq!(tombstone(channel), None, "{case}");
                assert_eq!(file_state(&row), before, "{case}: no backfill");
                assert_eq!(fx.take_calls(), Vec::<String>::new(), "{case}");
            }
            for provider in [ProviderKind::Claude, ProviderKind::Codex] {
                n += 1;
                let channel = ChannelId::new(5_340_620_000 + n * 10);
                let name = format!("AgentDesk-{}-p6asb-cmd-admitted-{n}", provider.as_str());
                mark(&name, Mark::Absent);
                let token = Arc::new(CancelToken::new());
                start(&shared, channel, &token).await;
                let row = inflight_row(&provider, channel, &name, true);
                let before = file_state(&row);

                let stop = begin_command_stop(&shared, &provider, channel, true).await;

                let case = format!("{server:?} {provider:?}");
                assert!(matches!(stop, CommandStop::Stop(_)), "{case}");
                assert!(token.cancelled.load(Ordering::SeqCst), "{case}");
                assert_eq!(token.tmux_session_name().as_deref(), Some(name.as_str()));
                assert_eq!(tombstone(channel), Some(Some(name.clone())), "{case}");
                assert_eq!(file_state(&row), before, "{case}: no backfill");
            }
        }
    });
}

// The cancel binds the name the judge admitted and registers its pane's provider PID, even when
// the marker turns Herdr between the judge and the cancel: the verdict is not taken again.
#[test]
fn a_command_stop_binds_its_verdict_when_the_marker_changes_after_it() {
    let fx = Fixture::new();
    run(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        fx.serve(Server::Live);
        let channel = ChannelId::new(5_340_630_000);
        let name = "AgentDesk-codex-p6asb-cmd-marker-after";
        mark(name, Mark::Absent);
        let token = Arc::new(CancelToken::new());
        start(&shared, channel, &token).await;
        inflight_row(&ProviderKind::Codex, channel, name, false);
        let judge = ChannelStop::judge(&shared, &ProviderKind::Codex, channel, None, true);
        let stop = judge.await.expect("a judged turn").expect("an active turn");
        assert!(!stop.refused());
        mark(name, Mark::Herdr);

        assert!(stop.cancel().await.token.is_some());

        assert_eq!(token.tmux_session_name().as_deref(), Some(name));
        assert_eq!(
            token.child_pid_value(),
            Some(fx.pid()),
            "the judged pane's PID"
        );
    });
}

// A cancel acts only on the turn it judged: a turn that replaced it meanwhile is neither
// cancelled nor tombstoned.
#[test]
fn a_judged_cancel_leaves_a_turn_that_replaced_the_judged_one() {
    let _fx = Fixture::new();
    run(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(5_340_640_000);
        let name = "AgentDesk-claude-p6asb-cas";
        mark(name, Mark::Absent);
        let judged = bound_token(&ProviderKind::Claude, name);
        start(&shared, channel, &judged).await;
        let judge = ChannelStop::judge(&shared, &ProviderKind::Claude, channel, None, false);
        let stop = judge.await.expect("a judged turn").expect("an active turn");
        let finish = crate::services::discord::mailbox_finish_turn;
        finish(&shared, &ProviderKind::Claude, channel).await;
        let next = bound_token(&ProviderKind::Claude, name);
        start(&shared, channel, &next).await;

        assert!(stop.cancel().await.token.is_none());

        assert!(!next.cancelled.load(Ordering::SeqCst));
        assert!(mailbox_holds(&shared, channel, &next).await);
        assert_eq!(tombstone(channel), None);
    });
}

// The idle-skip reads the session the stop judged: a token rebound afterwards to a pane that
// reads ready does not make the stop skip the interrupt and clear a row it was told to keep.
#[test]
fn an_idle_skip_reads_the_judged_session_not_a_later_binding() {
    let fx = Fixture::new();
    run(async {
        let (shared, registry) = runtime().await;
        fx.serve(Server::Live);
        let channel = ChannelId::new(5_340_650_000);
        let (judged, later) = (
            "AgentDesk-claude-p6asb-idle-a",
            "AgentDesk-claude-p6asb-idle-b",
        );
        mark(judged, Mark::Absent);
        mark(later, Mark::Absent);
        fx.ready(later);
        let token = bound_token(&ProviderKind::Claude, judged);
        start(&shared, channel, &token).await;
        let row = inflight_row(&ProviderKind::Claude, channel, judged, false);
        let judge = crate::services::discord::health::judge_provider_channel_stop;
        let stop = judge(&registry, "claude", channel).await;
        token.bind_claude_tmux_session(later);
        let restart_mode = crate::services::discord::InflightRestartMode::DrainRestart;
        let policy = TmuxCleanupPolicy::PreserveSessionAndInflight { restart_mode };

        let stop_judged = crate::services::discord::health::stop_judged_provider_channel;
        let result = stop_judged(stop, "p6asb", policy).await.unwrap();

        assert_eq!(result.inflight, InflightDisposition::NotNeeded);
        assert!(row.exists(), "the busy judged pane keeps the row");
        assert!(token.cancelled.load(Ordering::SeqCst));
    });
}

/// A Herdr target for the generating Claude turn on `session`, admitted by its gate.
fn herdr_turn(fx: &Fixture, session: &str) -> (Arc<CancelToken>, StopTarget) {
    let (token, _) = generating_turn(fx, session);
    let host = FakeHerdr::new(Ok(HostMutation::Confirmed));
    (
        token,
        herdr_target(&ProviderKind::Claude, session, host, Ok(())),
    )
}

/// Finishes the channel's turn once it is cancelled, as its source owner's exit would.
fn owner_exits_after_cancel(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    token: &Arc<CancelToken>,
) {
    let (shared, token) = (shared.clone(), token.clone());
    tokio::spawn(async move {
        while !token.cancelled.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        crate::services::discord::mailbox_finish_turn(&shared, &ProviderKind::Claude, channel)
            .await;
    });
}

// A Herdr stop leaves the turn and its row to the source owner, past the wait or after it, at
// the recovery stop and at the preserve-stop entry, which reports the turn kept.
#[test]
fn a_host_owned_stop_keeps_the_turn_row_for_its_owner() {
    let fx = Fixture::new();
    run(async {
        let (shared, registry) = runtime().await;
        let judge = crate::services::discord::health::judge_provider_channel_stop;
        let stop_judged = crate::services::discord::health::stop_judged_provider_channel;
        let policy = TmuxCleanupPolicy::PreserveSession;

        let (channel, session) = (ChannelId::new(5_340_660_000), "AgentDesk-claude-p6asb-ho-a");
        let (token, target) = herdr_turn(&fx, session);
        start(&shared, channel, &token).await;
        let row = inflight_row(&ProviderKind::Claude, channel, session, false);
        let active = shared.restart.global_active.load(Ordering::SeqCst);
        judge_next_as(target);
        let stop = judge(&registry, "claude", channel).await;
        let result = stop_judged(stop, "p6asb", policy).await.unwrap();
        assert_eq!(result.inflight, InflightDisposition::PreservedByHostGuard);
        assert!(
            mailbox_holds(&shared, channel, &token).await,
            "no fallback finish"
        );
        assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), active);
        assert!(row.exists());

        let (channel, session) = (ChannelId::new(5_340_660_010), "AgentDesk-claude-p6asb-ho-d");
        let (token, target) = herdr_turn(&fx, session);
        start(&shared, channel, &token).await;
        let row = inflight_row(&ProviderKind::Claude, channel, session, false);
        owner_exits_after_cancel(&shared, channel, &token);
        judge_next_as(target);
        let stop = judge(&registry, "claude", channel).await;
        let result = stop_judged(stop, "p6asb", policy).await.unwrap();
        assert_eq!(result.lifecycle_path, "canonical");
        assert_eq!(result.inflight, InflightDisposition::PreservedByHostGuard);
        assert!(row.exists(), "the canonical exit clears no host-owned row");

        let (channel, session) = (
            ChannelId::new(5_340_660_020),
            "AgentDesk-claude-p6asb-ho-tl",
        );
        let (token, target) = herdr_turn(&fx, session);
        start(&shared, channel, &token).await;
        let row = inflight_row(&ProviderKind::Claude, channel, session, false);
        owner_exits_after_cancel(&shared, channel, &token);
        judge_next_as(target);
        let target = TurnLifecycleTarget {
            provider: Some(ProviderKind::Claude),
            channel_id: Some(channel),
            tmux_name: session.to_string(),
        };
        let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;
        let result = stop(Some(&registry), &target, "p6asb").await;
        assert!(result.host_guard_kept(), "the caller changes nothing more");
        assert!(row.exists());
        for session in ["ho-a", "ho-d", "ho-tl"] {
            let session = format!("AgentDesk-claude-p6asb-{session}");
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}

// A preserve stop that knows only the tmux name keeps a turn on another host it finds by that
// name; the turn found for a legacy name is finished as in main.
#[test]
fn a_name_only_preserve_stop_keeps_a_turn_on_another_host() {
    let fx = Fixture::new();
    run(async {
        let (shared, registry) = runtime().await;
        fx.serve(Server::NoSocket);
        for (n, host) in [Mark::Herdr, Mark::Absent].into_iter().enumerate() {
            let channel = ChannelId::new(5_340_670_000 + n as u64 * 10);
            let channel_name = format!("p6asb-by-name-{n}");
            let map = crate::services::discord::host_defer_gate::tests::map_channel;
            map(&shared, channel, &channel_name).await;
            let name = ProviderKind::Claude.build_tmux_session_name(&channel_name);
            mark(&name, host);
            let token = bound_token(&ProviderKind::Claude, &name);
            start(&shared, channel, &token).await;
            let target = TurnLifecycleTarget {
                provider: None,
                channel_id: None,
                tmux_name: name.clone(),
            };
            let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;
            let result = stop(Some(&registry), &target, "p6asb").await;
            let legacy = matches!(host, Mark::Absent);
            assert_eq!(result.host_guard_kept(), !legacy, "{host:?}");
            assert_eq!(mailbox_holds(&shared, channel, &token).await, !legacy);
        }
    });
}

// A turn the stop cannot read is kept as a refused host's: a user stop on an unbound turn whose
// row fails to read or parse, and a runtime stop behind an unreachable mailbox, change nothing.
#[test]
fn a_stop_keeps_a_turn_it_could_not_read() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    run(async {
        let (shared, registry) = runtime().await;
        let root = inflight::inflight_runtime_root().expect("inflight root");
        for (n, row) in ["unreadable", "unparsable"].into_iter().enumerate() {
            let channel = ChannelId::new(5_340_680_000 + n as u64 * 10);
            let token = Arc::new(CancelToken::new());
            start(&shared, channel, &token).await;
            let path = inflight::inflight_state_path(&root, &ProviderKind::Claude, channel.get());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            match row {
                "unreadable" => std::fs::create_dir(&path).unwrap(),
                _ => std::fs::write(&path, "{\"channel_id\": ").unwrap(),
            }
            let before = std::fs::read(&path).ok();

            let stop = begin_command_stop(&shared, &ProviderKind::Claude, channel, true).await;

            assert!(!token.cancelled.load(Ordering::SeqCst), "{row}");
            assert!(mailbox_holds(&shared, channel, &token).await, "{row}");
            assert_eq!(token.tmux_session_name(), None, "{row}: not bound");
            assert_eq!(tombstone(channel), None, "{row}");
            assert_eq!(std::fs::read(&path).ok(), before, "{row}: row as it was");
            assert_eq!(path.is_dir(), row == "unreadable", "{row}");
            assert!(matches!(stop, CommandStop::HostRefused), "{row}");
        }
        let channel = ChannelId::new(5_340_680_100);
        let row = inflight_row(&ProviderKind::Claude, channel, "p6asb-unreachable", false);
        shared.mailboxes.insert_unreachable_for_test(channel);
        let before = file_state(&row);
        let stop = crate::services::discord::health::stop_provider_channel_runtime_with_policy;
        let policy = TmuxCleanupPolicy::PreserveSession;
        let result = stop(&registry, "claude", channel, "p6asb", policy).await;
        shared.mailboxes.remove_fixture_for_test(channel);
        assert_eq!(
            file_state(&row),
            before,
            "the row is neither cleared nor saved"
        );
        assert_eq!(tombstone(channel), None);
        let kept = InflightDisposition::PreservedByHostGuard;
        assert_eq!(result.map(|result| result.inflight), Some(kept));
    });
}

/// The variable a real-tmux child reads its condition from.
const REAL_TMUX_CHILD: &str = "ADK_P6ASB_REAL_TMUX";

/// The child's exec of any `tmux` is refused by the OS, wherever the runtime PATH finds it.
const DENY_TMUX_EXEC: &str = r#"(version 1)(allow default)(deny process-exec (regex #"/tmux$"))"#;

// Under a real tmux (live private server, no runnable binary, no socket; no stand-in on PATH), a
// stop on another host's turn writes nothing and leaves its pane; a legacy stop reaches the pane.
#[test]
fn a_stop_on_another_host_leaves_a_real_tmux_session_as_it_was() {
    let Some(condition) = std::env::var_os(REAL_TMUX_CHILD) else {
        for condition in ["live", "nobinary", "nosocket"] {
            real_tmux_child(condition);
        }
        return;
    };
    real_tmux_cells(condition.to_str().unwrap());
}

/// Runs the test again in a child process holding `condition` alone in its environment.
fn real_tmux_child(condition: &str) {
    let sockets = tempfile::Builder::new()
        .prefix("p6asb")
        .tempdir_in("/tmp")
        .unwrap();
    let test = "a_stop_on_another_host_leaves_a_real_tmux_session_as_it_was";
    let name = format!("{}::{test}", module_path!().split_once("::").unwrap().1);
    let exe = std::env::current_exe().unwrap();
    let sandboxed = condition == "nobinary" && cfg!(target_os = "macos");
    let mut command = match sandboxed {
        true => std::process::Command::new("/usr/bin/sandbox-exec"),
        false => std::process::Command::new(&exe),
    };
    if sandboxed {
        command.args(["-p", DENY_TMUX_EXEC]).arg(&exe);
    }
    command
        .args(["--exact", &name, "--nocapture", "--test-threads=1"])
        .env(REAL_TMUX_CHILD, condition)
        .env("TMUX_TMPDIR", sockets.path())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    if condition == "nobinary" {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let dirs = std::env::split_paths(&path).filter(|dir| !dir.join("tmux").is_file());
        command.env("PATH", std::env::join_paths(dirs).unwrap());
    }
    // Spawned under the env lock, so no concurrent fixture's tmux stand-in is on the inherited PATH.
    let lock = crate::config::shared_test_env_lock().lock();
    let env = lock.unwrap_or_else(|error| error.into_inner());
    let piped = std::process::Stdio::piped;
    let child = command.stdout(piped()).stderr(piped()).spawn();
    drop(env);
    let output = child
        .unwrap()
        .wait_with_output()
        .expect("run the real-tmux child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("[real tmux {condition}]\n{stderr}");
    assert!(output.status.success(), "{condition}\n{stdout}\n{stderr}");
    assert!(
        stdout.contains("1 passed"),
        "{condition}\n{stdout}\n{stderr}"
    );
}

/// Runs tmux as production finds it (the runtime PATH) on the private socket directory.
fn real_tmux(sockets: &std::path::Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut command = std::process::Command::new("tmux");
    crate::services::platform::binary_resolver::apply_runtime_path(&mut command);
    command
        .args(args)
        .env("TMUX_TMPDIR", sockets)
        .env_remove("TMUX");
    command.output()
}

/// The session's pane text and process, or `None` when tmux cannot show it.
fn pane(sockets: &std::path::Path, session: &str) -> Option<(String, String)> {
    let target = format!("={session}:");
    let read = |args: &[&str]| {
        let output = real_tmux(sockets, args).ok()?;
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        output.status.success().then_some(text)
    };
    let text = read(&["capture-pane", "-p", "-t", &target])?;
    Some((
        text,
        read(&["display-message", "-p", "-t", &target, "#{pane_pid}"])?,
    ))
}

fn real_tmux_cells(condition: &str) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let sockets = std::path::PathBuf::from(std::env::var_os("TMUX_TMPDIR").unwrap());
    let session = format!("p6asb-real-{}", std::process::id());
    let probe = real_tmux(&sockets, &["has-session", "-t", "=p6asb-none:"]);
    let realized = match (condition, &probe) {
        ("live", _) => {
            let args = [
                "new-session",
                "-d",
                "-s",
                &session,
                "-x",
                "80",
                "-y",
                "10",
                "cat",
            ];
            let started = real_tmux(&sockets, &args);
            eprintln!("private server started {session}: {started:?}");
            started.is_ok_and(|output| output.status.success())
        }
        ("nobinary", Err(error)) => {
            eprintln!("tmux cannot run: {error}");
            true
        }
        ("nosocket", Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("tmux runs with no server: {}", stderr.trim());
            let absent = ["no server running", "error connecting"];
            !output.status.success() && absent.iter().any(|text| stderr.contains(text))
        }
        _ => false,
    };
    if !realized {
        eprintln!("skipping the {condition} cells: the condition is not realized here {probe:?}");
        return;
    }
    let live = condition == "live";
    let _server = PrivateServer(sockets.clone());
    run(async {
        let (shared, registry) = runtime().await;
        let mut n = 0;
        let entries = [
            ("runtime", ProviderKind::Codex),
            ("preserve", ProviderKind::Claude),
            ("command", ProviderKind::Codex),
        ];
        for (entry, provider) in entries {
            n += 1;
            let channel = ChannelId::new(5_340_690_000 + n * 10);
            mark(&session, Mark::Herdr);
            let token = bound_token(&provider, &session);
            start(&shared, channel, &token).await;
            let row = inflight_row(&provider, channel, &session, false);
            let before = (file_state(&row), pane(&sockets, &session));
            let kept = match entry {
                "runtime" => {
                    let stop = crate::services::discord::health::stop_channel_runtime;
                    let policy = TmuxCleanupPolicy::PreserveSession;
                    let result = stop(&shared, &provider, channel, "p6asb", policy, None).await;
                    result.inflight == InflightDisposition::PreservedByHostGuard
                }
                "preserve" => {
                    let target = TurnLifecycleTarget {
                        provider: Some(provider.clone()),
                        channel_id: Some(channel),
                        tmux_name: session.clone(),
                    };
                    let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;
                    stop(Some(&registry), &target, "p6asb")
                        .await
                        .host_guard_kept()
                }
                _ => {
                    let stop = begin_command_stop(&shared, &provider, channel, true);
                    matches!(stop.await, CommandStop::HostRefused)
                }
            };
            let case = format!("{condition} {entry}");
            assert!(!token.cancelled.load(Ordering::SeqCst), "{case}");
            assert!(mailbox_holds(&shared, channel, &token).await, "{case}");
            assert_eq!(tombstone(channel), None, "{case}");
            let after = (file_state(&row), pane(&sockets, &session));
            assert_eq!(after, before, "{case}: row, session and pane as they were");
            assert_eq!(before.1.is_some(), live, "{case}: {:?}", before.1);
            assert!(kept, "{case}");
        }
        if live {
            let channel = ChannelId::new(5_340_690_100);
            mark(&session, Mark::Absent);
            let token = bound_token(&ProviderKind::Codex, &session);
            start(&shared, channel, &token).await;
            let before = pane(&sockets, &session).expect("the live pane");
            let stop = crate::services::discord::health::stop_channel_runtime;
            let policy = TmuxCleanupPolicy::PreserveSession;
            let codex = ProviderKind::Codex;
            let _ = stop(&shared, &codex, channel, "p6asb", policy, None).await;
            let after = pane(&sockets, &session).expect("the session survives a preserve stop");
            assert!(token.cancelled.load(Ordering::SeqCst));
            assert_eq!(after.1, before.1, "the same pane process");
            assert!(
                after.0.contains("^["),
                "the Escape reached the pane: {after:?}"
            );
        }
    });
}

/// Stops the private server when the cells end, a failed assertion included.
struct PrivateServer(std::path::PathBuf);

impl Drop for PrivateServer {
    fn drop(&mut self) {
        let _ = real_tmux(&self.0, &["kill-server"]);
    }
}
