use super::*;

#[cfg(unix)]
#[test]
fn test_instance_tmux_entries_use_private_socket_and_refuse_foreign_targets() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    const NAME: &str = "services::platform::tmux::instance_scope::tests::test_instance_tmux_entries_use_private_socket_and_refuse_foreign_targets";
    if crate::config::test_instance::requested() {
        exercise_instance_entries();
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let (_temp, root) = crate::config::test_instance::tests::fixture();
    let fake = root.join("fixture-worktree/target/debug/tmux");
    std::fs::write(&fake, r##"#!/bin/sh
{
  printf 'CALL\n'
  for argument do printf 'ARG=%s\n' "$argument"; done
  printf 'TMUX=%s\nTMUX_PANE=%s\nTMUX_TMPDIR=%s\n' "${TMUX-unset}" "${TMUX_PANE-unset}" "$TMUX_TMPDIR"
} >> "$ADK_I2_TMUX_LOG"
case " $* " in
  *" attach-session "*) printf '%%begin 1 1 0\n%%end 1 1 0\n'; cat >/dev/null; exit 0;;
esac
while [ "$1" = '-S' ] || [ "$1" = '-u' ]; do
  if [ "$1" = '-S' ]; then shift 2; else shift; fi
done
case "$1 $*" in
  *show-environment*AGENTDESK_IDENTITY*) printf '%s\n' "$ADK_I2_TMUX_IDENTITY";;
  *pane_width*) printf '80 30\n';;
  *pane_id*) printf '%%7\n';;
  *pane_dead*|list-panes*) printf '0\n';;
  *pane_current_command*) printf '%s|claude|99\nAgentDesk-release|claude|100\n' "$ADK_I2_TMUX_SESSION";;
  *pane_pid*) printf '99\n';;
  *session_name*pid*) printf '%s|99\nAgentDesk-release|100\n' "$ADK_I2_TMUX_SESSION";;
  list-sessions*) printf '%s\nAgentDesk-release\n' "$ADK_I2_TMUX_SESSION";;
  capture-pane*) printf 'captured test pane\n';;
esac
"##).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let log = root.join("tmux-calls");
    let name = format!(
        "AdkTest-{}-claude-test",
        root.file_name().unwrap().to_str().unwrap()
    );
    let output = crate::config::test_instance::tests::command(&root)
        .args(["--exact", NAME, "--nocapture"])
        .env("AGENTDESK_TEST_MANIFEST", root.join("manifest.json"))
        .env("AGENTDESK_ROOT_DIR", &root)
        .env("AGENTDESK_CONFIG", root.join("config/agentdesk.yaml"))
        .env("ADK_I2_TMUX_LOG", &log)
        .env("ADK_I2_TMUX_SESSION", &name)
        .env("TMUX", "/synthetic/release/socket,42,0")
        .env("TMUX_PANE", "%99")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = std::fs::read_to_string(log).unwrap();
    let blocks: Vec<_> = calls
        .split("CALL\n")
        .filter(|block| !block.is_empty())
        .collect();
    assert!(!blocks.is_empty(), "missing real entry calls: {calls}");
    let attaches: Vec<_> = blocks
        .iter()
        .filter(|block| block.contains("ARG=attach-session\n"))
        .collect();
    assert_eq!(
        attaches.len(),
        1,
        "normal control attach did not spawn: {calls}"
    );
    assert!(attaches[0].contains(&format!("ARG=={name}\n")));
    for call in blocks {
        assert!(
            call.contains(&format!(
                "ARG=-S\nARG={}\n",
                root.join("runtime/tmux/socket").display()
            )),
            "{call}"
        );
        assert!(call.contains("TMUX=unset\nTMUX_PANE=unset\n"), "{call}");
        assert!(
            call.contains(&format!(
                "TMUX_TMPDIR={}\n",
                root.join("runtime/tmux").display()
            )),
            "{call}"
        );
        assert!(
            !call.contains("ARG=AgentDesk-release\n")
                && !call.contains("ARG==AgentDesk-release:\n"),
            "{call}"
        );
    }
}

#[cfg(unix)]
fn exercise_instance_entries() {
    use crate::services::platform::tmux;
    use crate::services::tui_input::actor::pane::{Pane as _, TmuxPane};
    use crate::services::tui_input::submission_tmux::SubmissionTmux;
    crate::config::test_instance::initialize().unwrap();
    let instance = crate::config::test_instance::current().unwrap().unwrap();
    let name = std::env::var("ADK_I2_TMUX_SESSION").unwrap();
    let assignment = crate::services::session_identity_metadata::runtime_identity(&name)
        .unwrap()
        .environment_assignment()
        .unwrap();
    let _identity = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_I2_TMUX_IDENTITY",
        std::ffi::OsStr::new(&assignment),
    );
    let fake = instance.root.join("fixture-worktree/target/debug/tmux");
    assert!(
        tmux::create_session(&name, None, "true")
            .unwrap()
            .status
            .success()
    );
    assert!(
        tmux::respawn_dead_pane(&name, None, "true")
            .unwrap()
            .status
            .success()
    );
    assert!(tmux::send_keys(&name, &["Enter"]).unwrap().status.success());
    assert_eq!(
        tmux::capture_pane(&name, -10).as_deref(),
        Some("captured test pane\n")
    );
    assert_eq!(tmux::list_session_names().unwrap(), [name.clone()]);
    assert_eq!(
        tmux::list_sessions_with_pane_command().unwrap()[0].session_name,
        name
    );
    assert_eq!(tmux::list_sessions_with_server_pid().unwrap().len(), 1);
    assert_eq!(tmux::pane_liveness(&name), tmux::PaneLiveness::Live);
    let mut prepared = super::super::liveness::prepared_tmux_command().unwrap();
    assert!(prepared.arg("-V").output().unwrap().status.success());
    let mut health = tmux::health_probe::health_tmux_command(
        tmux::health_probe::HealthTmuxQuery::PanePid,
        &name,
    )
    .unwrap();
    assert!(health.output().unwrap().status.success());
    let busy = crate::services::claude_tui::busy_inject::Pane::with_program(&name, fake.clone());
    assert!(busy.capture().is_some());
    assert!(
        TmuxPane::with_program(&name, fake.clone(), std::time::Duration::from_secs(2))
            .capture()
            .is_ok()
    );
    assert_eq!(SubmissionTmux.pane_size(&name), Some((80, 30)));
    let hook = super::super::dead_marker_shell_command(&name, Some(12));
    assert!(
        Command::new("/bin/sh")
            .args(["-c", &hook])
            .status()
            .unwrap()
            .success()
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(
            crate::services::claude::session_handoff::pinned_tmux::scoped_control_attach_for_test(
                &fake, &name,
            ),
        )
        .unwrap();
    let log = std::env::var_os("ADK_I2_TMUX_LOG").unwrap();
    let before = std::fs::read(&log).unwrap();
    let refusal = runtime
        .block_on(
            crate::services::claude::session_handoff::pinned_tmux::scoped_control_attach_for_test(
                &instance.root.join("absent-control-program"),
                "AgentDesk-release",
            ),
        )
        .unwrap_err();
    assert!(
        refusal.contains("test instance host declaration rejected"),
        "{refusal}"
    );
    for foreign in [
        "AgentDesk-release".to_string(),
        "%7".into(),
        format!("{}-x:1.0", instance.tmux.prefix),
        format!("{}foreign", instance.tmux.prefix),
        format!("{}-x;kill-server", instance.tmux.prefix),
        format!("{}-x\nsend-keys -t AgentDesk-release", instance.tmux.prefix),
    ] {
        assert!(tmux::create_session(&foreign, None, "true").is_err());
        assert!(tmux::respawn_dead_pane(&foreign, None, "true").is_err());
        assert!(tmux::send_keys(&foreign, &["Enter"]).is_err());
        assert!(tmux::capture_pane(&foreign, -1).is_none());
        assert!(!tmux::kill_session(&foreign, "isolated test"));
        assert_eq!(
            tmux::pane_liveness(&foreign),
            tmux::PaneLiveness::ProbeError
        );
        assert!(
            tmux::health_probe::health_tmux_command(
                tmux::health_probe::HealthTmuxQuery::PanePid,
                &foreign
            )
            .is_err()
        );
        assert!(
            crate::services::claude_tui::busy_inject::Pane::with_program(&foreign, fake.clone())
                .capture()
                .is_none()
        );
        assert!(
            TmuxPane::with_program(&foreign, fake.clone(), std::time::Duration::from_secs(2))
                .capture()
                .is_err()
        );
        assert!(SubmissionTmux.pane_size(&foreign).is_none());
    }
    for args in [
        vec!["send-keys", "-tAgentDesk-release", "Enter"],
        vec!["new-session", "-sAgentDesk-release"],
        vec!["-S", "/synthetic/release/socket", "list-sessions"],
        vec!["-Lforeign", "list-sessions"],
        vec!["kill-server"],
    ] {
        assert!(input_command(&mut Command::new(&fake), &args).is_err());
    }
    assert_eq!(
        std::fs::read(log).unwrap(),
        before,
        "foreign target spawned tmux"
    );
}
