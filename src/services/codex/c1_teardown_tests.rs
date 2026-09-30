#![cfg(unix)]

use super::*;
use crate::services::provider_teardown::tests::test_support::{
    FakeTmux, cleared, refusals, take_exit_reason,
};

fn called(calls: &[String], command: &str) -> bool {
    calls.iter().any(|call| call.starts_with(command))
}

/// Puts a tmux in front of `FakeTmux` whose `new-session` succeeds into the same log, and
/// points `cli_env` at a CLI that resolves, so a relaunch runs to its end.
fn relaunching(cli_env: &'static str) -> (tempfile::TempDir, Vec<crate::config::TestEnvVarGuard>) {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let fake = std::env::split_paths(&path).next().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, body: String| {
        let file = dir.path().join(name);
        std::fs::write(&file, body).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        file
    };
    let fake = fake.display();
    write(
        "tmux",
        format!(
            "#!/bin/sh\nif [ \"$2\" = new-session ]; then shift; echo \"$*\" >> '{fake}/calls'; \
             exit 0; fi\nexec '{fake}/tmux' \"$@\"\n"
        ),
    );
    let cli = write("cli", "#!/bin/sh\necho 2.1.0\n".to_string());
    let mut paths = vec![dir.path().to_path_buf()];
    paths.extend(std::env::split_paths(&path));
    let paths = std::env::join_paths(paths).unwrap();
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let env = vec![
        set("PATH", std::path::Path::new(&paths)),
        set(cli_env, &cli),
    ];
    (dir, env)
}

/// The audit, kill and launch calls in the order they ran.
fn teardown_then_launch(calls: &[String]) -> Vec<&'static str> {
    calls
        .iter()
        .filter_map(|call| {
            ["capture-pane", "kill-session", "new-session"]
                .into_iter()
                .find(|command| call.starts_with(command))
        })
        .collect()
}

// An existing wrapper session is killed, its files swept and a new one launched only under
// its own clearance; a refusal ends the turn with no audit, kill, sweep or relaunch.
#[test]
fn an_existing_session_is_recreated_only_under_its_clearance() {
    const NAME: &str = "adk-w2a-codex-existing";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = FakeTmux::install(NAME);
    let leftover = crate::services::tmux_common::session_temp_path(NAME, "prompt");
    let cases = std::iter::once(("cleared", Some(cleared(NAME)), true));
    let cases = cases.chain(
        refusals(NAME)
            .into_iter()
            .map(|(label, c)| (label, c, false)),
    );
    for (label, clearance, recreated) in cases {
        crate::services::tmux_common::cleanup_session_temp_files(NAME);
        std::fs::create_dir_all(std::path::Path::new(&leftover).parent().unwrap()).unwrap();
        std::fs::write(&leftover, "stale").unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let result = execute_streaming_local_tmux(
            "hello",
            None,
            None,
            None,
            None,
            "/tmp",
            tx,
            None,
            NAME,
            clearance.as_ref(),
            None,
            None,
            None,
            None,
            true,
        );
        let error = result.expect_err(label);
        assert_eq!(
            error.contains("host guard kept"),
            !recreated,
            "{label}: {error}"
        );
        let calls = tmux.take_calls();
        assert_eq!(
            called(&calls, "capture-pane"),
            recreated,
            "{label}: {calls:?}"
        );
        assert_eq!(called(&calls, "kill-session"), recreated, "{label}");
        assert!(
            !called(&calls, "new-session"),
            "{label}: the CLI never resolves"
        );
        assert_eq!(called(&calls, "reason-before-kill"), recreated, "{label}");
        assert!(recreated || !take_exit_reason(NAME), "{label}");
        let kept = std::fs::read_to_string(&leftover).is_ok_and(|text| text == "stale");
        assert_eq!(kept, !recreated, "{label}: the stale prompt is swept");
    }
}

// An existing session cleared for this very turn is audited, killed and relaunched exactly
// once, by a launch script that runs the resolved CLI.
#[test]
fn a_cleared_existing_session_is_relaunched_exactly_once() {
    const NAME: &str = "adk-w2b-codex-relaunch";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = FakeTmux::install(NAME);
    let (launch, _env) = relaunching("AGENTDESK_CODEX_PATH");
    let (tx, _rx) = std::sync::mpsc::channel();
    // A cancelled turn ends the output wait right after the launch.
    let cancel = std::sync::Arc::new(CancelToken::new());
    cancel
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let clearance = cleared(NAME);
    let result = execute_streaming_local_tmux(
        "hello",
        None,
        None,
        None,
        None,
        "/tmp",
        tx,
        Some(cancel),
        NAME,
        Some(&clearance),
        None,
        None,
        None,
        None,
        true,
    );
    assert_eq!(result, Ok(()));
    let calls = tmux.take_calls();
    let order = teardown_then_launch(&calls);
    assert_eq!(
        order,
        ["capture-pane", "kill-session", "new-session"],
        "{calls:?}"
    );
    assert!(called(&calls, "reason-before-kill"), "{calls:?}");
    let script = crate::services::tmux_common::session_temp_path(NAME, "sh");
    let created = calls
        .iter()
        .find(|call| call.starts_with("new-session"))
        .unwrap();
    assert!(
        created.starts_with(&format!("new-session -d -s {NAME} -c /tmp bash "))
            && created.contains(&script),
        "{created}"
    );
    let body = std::fs::read_to_string(&script).unwrap();
    let cli = launch.path().join("cli").display().to_string();
    assert!(body.contains(&cli), "{body}");
}
