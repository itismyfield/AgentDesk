//! Moved from src/services/claude/tui_session_launch.rs: names the tmux session via the root crate.

#[cfg(unix)]
#[test]
fn claude_launch_marks_its_tmux_host_where_session_cleanup_looks_and_a_failed_mark_still_launches()
{
    use crate::config::TestEnvVarGuard as Guard;
    use crate::services::claude::tui_session_launch::prepare_and_create_claude_tui_session_for_test as launch;
    use crate::services::discord::session_identity::tmux_name_from_session_key;
    use crate::services::session_host::HostKind;
    use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::tests};
    use std::os::unix::fs::PermissionsExt;
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _env) = tests::fixture_after_shared_test_env_lock();
    // Prepend the fake tmux: concurrent tests must still find system binaries.
    let stub = "#!/bin/bash\nprintf '%s\\n' \"$*\" >> \"$AGENTDESK_ROOT_DIR/tmux.calls\"\n";
    let fake_tmux = root.path().join("tmux");
    std::fs::write(&fake_tmux, stub).unwrap();
    std::fs::set_permissions(&fake_tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!(
        "{}:{}",
        root.path().display(),
        std::env::var("PATH").unwrap()
    );
    let _path = Guard::set_value_after_shared_test_env_lock("PATH", path.as_ref());
    let claude = root.path().join("claude");
    std::fs::write(&claude, "#!/bin/bash\necho '2.1.0 (Claude Code)'\n").unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _bin = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CLAUDE_PATH", &claude);
    let dir = root.path().to_str().unwrap();
    let id = "11111111-1111-4111-8111-111111111111";
    let calls = root.path().join("tmux.calls");

    let tmux = "AgentDesk-claude-host-marker-launch";
    let session_key = format!("claude/token-hash/mac-mini:{tmux}");
    let witness_name = tmux_name_from_session_key(&session_key).unwrap();
    assert_eq!(read_host_kind_marker(&witness_name), HostKindMarker::Absent);
    launch(tmux, dir, root.path(), id, 42).unwrap();
    assert!(std::fs::read_to_string(&calls).unwrap().contains(tmux));
    assert_eq!(
        read_host_kind_marker(&witness_name),
        HostKindMarker::Known(HostKind::Tmux),
        "cleanup reads the marker by the session key's tmux name"
    );

    let blocked = "AgentDesk-claude-host-marker-blocked";
    let marker = crate::services::tmux_common::session_temp_path(blocked, "host_kind");
    std::fs::create_dir(&marker).unwrap();
    launch(blocked, dir, root.path(), id, 43)
        .err()
        .expect("unreadable host evidence cannot authorize a publisher");
    assert!(!std::fs::read_to_string(&calls).unwrap().contains(blocked));
    assert!(std::path::Path::new(&marker).is_dir());
    assert!(
        !std::path::Path::new(&crate::services::tmux_common::tmux_owner_path(blocked)).exists()
    );
    assert!(matches!(
        read_host_kind_marker(blocked),
        HostKindMarker::ReadFailed(_)
    ));
}
