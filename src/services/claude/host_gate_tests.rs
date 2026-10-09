#![cfg(unix)]

use super::super::*;
use crate::services::provider_teardown::tests::test_support::{FakeTmux, cleared, refusals};
use crate::services::session_host::test_support::{InjectedLivenessGuard, InjectedPresenceGuard};
use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};

fn called(calls: &[String], command: &str) -> bool {
    calls.iter().any(|call| call.starts_with(command))
}

fn tui_turn(name: &str, clearance: Option<&TeardownClearance>) -> Result<(), String> {
    let (tx, _rx) = std::sync::mpsc::channel();
    let endpoint = "http://127.0.0.1:9".to_string();
    execute_streaming_local_tui_tmux(
        "hello", None, "/tmp", tx, None, name, clearance, None, None, None, None, endpoint,
    )
}

fn wrapper_turn(name: &str, clearance: Option<&TeardownClearance>) -> Result<(), String> {
    let (tx, _rx) = std::sync::mpsc::channel();
    execute_streaming_local_tmux(
        &[],
        "hello",
        None,
        "/tmp",
        tx,
        None,
        name,
        clearance,
        None,
        None,
        None,
        0,
    )
}

type Turn = fn(&str, Option<&TeardownClearance>) -> Result<(), String>;

const ENTRIES: [(&str, Turn); 2] = [("tui", tui_turn), ("wrapper", wrapper_turn)];

fn write_marker(name: &str, body: &str) -> String {
    let marker = crate::services::tmux_common::session_temp_path(name, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(&marker, body).unwrap();
    marker
}

// A turn on a session whose marker names another host or is unreadable ends before any
// tmux probe, kill or launch, and leaves the marker; an absent or tmux marker goes on.
#[test]
fn a_session_marked_for_another_host_is_never_probed_killed_or_relaunched() {
    const NAME: &str = "adk-p5c-claude-marked";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = FakeTmux::install(NAME);
    for (entry, turn) in ENTRIES {
        for body in ["herdr", "process", "zellij"] {
            let marker = write_marker(NAME, body);
            let error = turn(NAME, Some(&cleared(NAME))).expect_err(body);
            assert!(error.contains("host check kept"), "{entry} {body}: {error}");
            assert_eq!(tmux.take_calls(), Vec::<String>::new(), "{entry} {body}");
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), body, "{entry}");
        }
        let marker = write_marker(NAME, "");
        std::fs::remove_file(&marker).unwrap();
        std::fs::create_dir(&marker).unwrap();
        let error = turn(NAME, Some(&cleared(NAME))).expect_err("unreadable marker");
        assert!(error.contains("ReadFailed"), "{entry}: {error}");
        assert!(tmux.take_calls().is_empty(), "{entry}: unreadable marker");
        std::fs::remove_dir(&marker).unwrap();

        for body in [None, Some("tmux")] {
            if let Some(body) = body {
                write_marker(NAME, body);
            }
            let error = turn(NAME, Some(&cleared(NAME))).expect_err("the CLI never resolves");
            assert!(
                !error.contains("host check kept"),
                "{entry} {body:?}: {error}"
            );
            let calls = tmux.take_calls();
            assert!(
                called(&calls, "kill-session"),
                "{entry} {body:?}: {calls:?}"
            );
            assert!(!called(&calls, "new-session"), "{entry} {body:?}");
            crate::services::tmux_common::cleanup_session_temp_files(NAME);
        }
    }
}

// A test turn on the host's tmux makes no tmux call unless its socket is isolated, so a turn
// that outlives its fixture never starts a session on the user's server.
#[test]
fn a_test_turn_reaches_the_host_tmux_only_on_an_isolated_socket() {
    const NAME: &str = "adk-i6719-claude-host-tmux";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = FakeTmux::install(NAME);
    // With the temp dir moved elsewhere the fake reads as the host's own binary.
    let elsewhere = std::env::temp_dir().join("adk-i6719-elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _elsewhere = set("TMPDIR", &elsewhere);
    let capture = crate::config::TestEnvVarGuard::capture_after_shared_test_env_lock;
    let _env = [capture("TMUX"), capture("TMUX_TMPDIR")];
    let attached = Some("/tmp/tmux-0/default,1,0");
    let cases = [
        (None, None, false),
        (Some(&elsewhere), attached, false),
        (Some(&elsewhere), None, true),
    ];
    for (entry, turn) in ENTRIES {
        for (sockets, tmux_env, admitted) in cases {
            let label = format!("{entry} TMUX_TMPDIR={sockets:?} TMUX={tmux_env:?}");
            unsafe {
                match sockets {
                    Some(dir) => std::env::set_var("TMUX_TMPDIR", dir),
                    None => std::env::remove_var("TMUX_TMPDIR"),
                }
                match tmux_env {
                    Some(value) => std::env::set_var("TMUX", value),
                    None => std::env::remove_var("TMUX"),
                }
            }
            let error = turn(NAME, Some(&cleared(NAME))).expect_err(&label);
            assert_eq!(error.contains("refused"), !admitted, "{label}: {error}");
            let calls = tmux.take_calls();
            assert_eq!(
                called(&calls, "kill-session"),
                admitted,
                "{label}: {calls:?}"
            );
            assert!(admitted || calls.is_empty(), "{label}: {calls:?}");
            crate::services::tmux_common::cleanup_session_temp_files(NAME);
        }
    }
}

// A stale TUI session is audited and killed only under the turn's own clearance or with
// no key; any refusal ends the turn with no audit, kill or relaunch.
#[test]
fn a_stale_tui_session_is_cleaned_only_under_its_clearance() {
    const NAME: &str = "adk-p5c-claude-tui-stale";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = FakeTmux::install(NAME);
    let admitted = [
        ("cleared", Some(cleared(NAME)), true),
        ("unkeyed", Some(TeardownClearance::Unkeyed), true),
    ];
    let refused = refusals(NAME)
        .into_iter()
        .map(|(label, c)| (label, c, false));
    for (label, clearance, torn_down) in admitted.into_iter().chain(refused) {
        let error = tui_turn(NAME, clearance.as_ref()).expect_err(label);
        assert_eq!(
            error.contains("host guard kept"),
            !torn_down,
            "{label}: {error}"
        );
        let calls = tmux.take_calls();
        assert_eq!(
            called(&calls, "capture-pane"),
            torn_down,
            "{label}: {calls:?}"
        );
        assert_eq!(
            called(&calls, "kill-session"),
            torn_down,
            "{label}: {calls:?}"
        );
        assert!(!called(&calls, "new-session"), "{label}: {calls:?}");
        crate::services::tmux_common::cleanup_session_temp_files(NAME);
    }
}

/// A PATH-first tmux whose sessions exist but whose `list-panes` fails, logging each call.
fn pane_probe_failing_tmux() -> (tempfile::TempDir, Vec<crate::config::TestEnvVarGuard>) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("tmux");
    let body = "#!/bin/sh\n[ \"$1\" = -u ] && shift\necho \"$*\" >> \"$(dirname \"$0\")/calls\"\n\
                case \"$1\" in has-session|kill-session) exit 0 ;; capture-pane) echo pane ;; \
                *) exit 1 ;; esac\n";
    std::fs::write(&binary, body).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![dir.path().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths).unwrap();
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let absent = dir.path().join("absent-cli");
    let env = vec![
        set("PATH", std::path::Path::new(&path)),
        set("AGENTDESK_CLAUDE_PATH", &absent),
    ];
    (dir, env)
}

fn take_calls(dir: &tempfile::TempDir) -> Vec<String> {
    let log = dir.path().join("calls");
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(log);
    calls.lines().map(str::to_string).collect()
}

// A present session whose pane probe fails is not a stale session: both entries end the
// turn with no kill or relaunch, while a confirmed dead pane is still recreated.
#[test]
fn a_failed_pane_probe_never_reads_as_a_stale_session() {
    const NAME: &str = "adk-p5c-claude-pane-unobserved";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (tmux, _env) = pane_probe_failing_tmux();
    let session = HostSessionRef::tmux(NAME);
    let _presence = InjectedPresenceGuard::set(session, HostPresence::Present);
    for (entry, turn) in ENTRIES {
        for (pane, recreated) in [
            (HostLiveness::ProbeError, false),
            (HostLiveness::DeadOrAbsent, true),
        ] {
            let _pane = InjectedLivenessGuard::set(session, pane);
            let error = turn(NAME, Some(&cleared(NAME))).expect_err(entry);
            assert_eq!(
                error.contains("unobserved"),
                !recreated,
                "{entry} {pane:?}: {error}"
            );
            let calls = take_calls(&tmux);
            assert_eq!(
                called(&calls, "kill-session"),
                recreated,
                "{entry} {pane:?}: {calls:?}"
            );
            assert!(!called(&calls, "new-session"), "{entry} {pane:?}");
            crate::services::tmux_common::cleanup_session_temp_files(NAME);
        }
    }
}

// A wrapper follow-up's poll reads its session dead only on a confirmed tmux death, so a
// failed pane probe never turns into a recreate request.
#[test]
fn the_wrapper_follow_up_poll_reads_dead_only_on_a_confirmed_death() {
    const NAME: &str = "adk-p5c-claude-wrapper-poll";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let session = HostSessionRef::tmux(NAME);
    for (pane, alive) in [
        (HostLiveness::Live, true),
        (HostLiveness::ProbeError, true),
        (HostLiveness::DeadOrAbsent, false),
    ] {
        let _pane = InjectedLivenessGuard::set(session, pane);
        let probe = super::tmux_wrapper_poll_probe(NAME);
        assert_eq!((probe.is_alive)(), alive, "{pane:?}");
    }
}

// A failed presence probe while tmux has a server, or its socket cannot be read, keeps every
// runtime file and starts nothing; with no server socket the fresh path runs as before.
#[test]
fn a_failed_presence_probe_never_prepares_a_fresh_session() {
    const NAME: &str = "adk-p5c-claude-presence-unobserved";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = FakeTmux::install(NAME);
    let sockets = tempfile::tempdir().unwrap();
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _tmpdir = set("TMUX_TMPDIR", sockets.path());
    // A resolvable CLI the fake tmux never runs, so the fresh path reaches its preparation.
    let cli = sockets.path().join("claude");
    std::fs::write(&cli, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _cli = set("AGENTDESK_CLAUDE_PATH", &cli);
    let _attached = crate::config::TestEnvVarGuard::capture_after_shared_test_env_lock("TMUX");
    unsafe { std::env::remove_var("TMUX") };
    let uid = unsafe { libc::getuid() };
    let socket_dir = sockets.path().join(format!("tmux-{uid}"));
    let socket = socket_dir.join("default");
    let session = HostSessionRef::tmux(NAME);
    let _presence = InjectedPresenceGuard::set(session, HostPresence::ProbeFailed);
    let _pane = InjectedLivenessGuard::set(session, HostLiveness::Live);
    let files = ["jsonl", "prompt", "generation"]
        .map(|ext| crate::services::tmux_common::session_temp_path(NAME, ext));
    for (entry, turn) in ENTRIES {
        // A file in place of the socket directory makes the socket lookup itself fail.
        for (socket_state, server) in [("present", true), ("unreadable", true), ("absent", false)] {
            let _ = std::fs::remove_dir_all(&socket_dir);
            let _ = std::fs::remove_file(&socket_dir);
            match socket_state {
                "present" => {
                    std::fs::create_dir_all(&socket_dir).unwrap();
                    std::fs::write(&socket, "").unwrap();
                }
                "unreadable" => std::fs::write(&socket_dir, "").unwrap(),
                _ => {}
            }
            for file in &files {
                std::fs::create_dir_all(std::path::Path::new(file).parent().unwrap()).unwrap();
                std::fs::write(file, "sentinel").unwrap();
            }
            let result = turn(NAME, Some(&cleared(NAME)));
            let kept = files
                .iter()
                .all(|file| std::fs::read_to_string(file).is_ok_and(|body| body == "sentinel"));
            let calls = tmux.take_calls();
            if server {
                let label = format!("{entry} socket {socket_state}");
                assert!(kept, "{label}: runtime files kept, {result:?}");
                assert_eq!(calls, Vec::<String>::new(), "{label}");
                let error = result.expect_err(entry);
                assert!(error.contains("unobserved"), "{label}: {error}");
            } else {
                assert!(
                    !kept,
                    "{entry}: no server, fresh path as before: {result:?}"
                );
            }
            crate::services::tmux_common::cleanup_session_temp_files(NAME);
        }
    }
}
