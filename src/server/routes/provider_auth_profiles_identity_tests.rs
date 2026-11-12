use super::*;

#[test]
fn identity_login_retry_replaces_only_a_marked_auth_session() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rig = crate::services::platform::tmux::session_identity_tests::Rig::new();
    let session = format!("adk-login-s1-{}", uuid::Uuid::new_v4());
    let script = rig.dir.path().join("login.sh");
    std::fs::write(&script, "#!/bin/sh\nexec cat\n").unwrap();
    spawn_login_tmux(&session, rig.dir.path(), &script, "claude").unwrap();
    let marker = rig.raw(&[
        "show-environment",
        "-t",
        &format!("={session}"),
        "AGENTDESK_IDENTITY",
    ]);
    let retry = spawn_login_tmux(&session, rig.dir.path(), &script, "claude");
    let pid = |rig: &crate::services::platform::tmux::session_identity_tests::Rig| {
        rig.raw(&[
            "display-message",
            "-p",
            "-t",
            &format!("={session}:"),
            "#{pane_pid}",
        ])
        .stdout
    };
    let before = pid(&rig);
    assert!(
        rig.raw(&[
            "set-environment",
            "-u",
            "-t",
            &format!("={session}"),
            "AGENTDESK_IDENTITY"
        ])
        .status
        .success()
    );
    let unmarked_retry = spawn_login_tmux(&session, rig.dir.path(), &script, "claude");
    let after = pid(&rig);
    let _ = rig.raw(&["kill-session", "-t", &format!("={session}")]);
    assert!(
        marker.status.success(),
        "auth producer must attach identity at create"
    );
    let identity = crate::domain::session_identity::managed::ManagedIdentity::decode_environment(
        std::str::from_utf8(&marker.stdout).unwrap(),
    )
    .unwrap();
    assert_eq!(identity.name_kind, "provider_auth");
    assert_eq!(identity.provider.as_deref(), Some("claude"));
    assert!(
        retry.is_ok(),
        "leftover login session must permit retry: {retry:?}"
    );
    assert!(
        unmarked_retry.is_err(),
        "removing ownership must refuse retry before replacement"
    );
    assert_eq!(before, after, "unmarked login process must survive");
}
