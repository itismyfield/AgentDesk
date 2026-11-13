use super::*;
use std::os::unix::fs::PermissionsExt;

pub(crate) struct Rig {
    pub(crate) dir: tempfile::TempDir,
    _path: crate::config::TestEnvVarGuard,
    _root: crate::config::TestEnvVarGuard,
}

impl Rig {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("tmux");
        let binary = crate::services::platform::binary_resolver::runtime_command("tmux").unwrap();
        let binary =
            crate::services::process::shell_escape(&binary.get_program().to_string_lossy());
        std::fs::write(
            &program,
            format!("#!/bin/sh\nexec {binary} -L i8119-s1 -f /dev/null \"$@\"\n"),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            dir.path(),
        );
        let path =
            crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(dir.path());
        Self {
            dir,
            _path: path,
            _root: root,
        }
    }

    pub(crate) fn raw(&self, args: &[&str]) -> Output {
        Command::new(self.dir.path().join("tmux"))
            .env_remove("AGENTDESK_IDENTITY")
            .args(args)
            .output()
            .unwrap()
    }

    fn snapshot(&self, name: &str) -> (Vec<u8>, Vec<u8>) {
        let target = exact_target(name);
        (
            self.raw(&["capture-pane", "-p", "-t", &target]).stdout,
            self.raw(&["display-message", "-p", "-t", &target, "#{pane_pid}"])
                .stdout,
        )
    }
}

#[test]
fn identity_gate_refuses_foreign_send_paste_and_kill_without_mutation() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rig = Rig::new();
    let name = format!("adk-cc-{}", uuid::Uuid::new_v4());
    assert!(
        rig.raw(&["new-session", "-d", "-s", &name, "cat"])
            .status
            .success()
    );
    let before = rig.snapshot(&name);
    assert!(
        load_buffer("s1-foreign", "unexpected")
            .unwrap()
            .status
            .success()
    );
    let refused = [
        send_keys(&name, &["unexpected"]),
        send_keys_timeout(&name, &["Enter"], Duration::from_secs(1)),
        send_literal(&name, "unexpected"),
        paste_buffer(&name, "s1-foreign", true),
    ];
    let kill = kill_session(&name, "isolated foreign gate test");
    let after = rig.snapshot(&name);
    let _ = rig.raw(&["kill-session", "-t", &exact_target(&name)]);
    assert!(
        refused.iter().all(Result::is_err),
        "all send/paste entrypoints must refuse"
    );
    assert!(!kill, "foreign session must survive");
    assert_eq!(before, after, "pane and PID must remain unchanged");
}

#[test]
fn identity_marker_is_present_at_creation_and_unset_is_refused() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rig = Rig::new();
    let name = format!("s1-managed-{}", uuid::Uuid::new_v4());
    assert!(create_session(&name, None, "cat").unwrap().status.success());
    let marker = rig.raw(&[
        "show-environment",
        "-t",
        &format!("={name}"),
        "AGENTDESK_IDENTITY",
    ]);
    let before_duplicate = rig.snapshot(&name);
    let mut other = crate::services::session_identity_metadata::runtime_identity(&name).unwrap();
    other.owner_scope = "e".repeat(64);
    let duplicate = create_session_with_identity(&name, None, "cat", &other).unwrap();
    let after_duplicate = rig.raw(&[
        "show-environment",
        "-t",
        &format!("={name}"),
        "AGENTDESK_IDENTITY",
    ]);
    assert!(
        !duplicate.status.success(),
        "duplicate creation must fail atomically"
    );
    assert_eq!(
        marker.stdout, after_duplicate.stdout,
        "an existing marker must never be adopted or overwritten"
    );
    assert_eq!(before_duplicate, rig.snapshot(&name));
    let first_send = send_keys(&name, &[""]);
    assert!(
        rig.raw(&[
            "set-environment",
            "-u",
            "-t",
            &format!("={name}"),
            "AGENTDESK_IDENTITY"
        ])
        .status
        .success()
    );
    let before = rig.snapshot(&name);
    let refused = send_literal(&name, "unexpected");
    let after = rig.snapshot(&name);
    let _ = rig.raw(&["kill-session", "-t", &exact_target(&name)]);
    assert!(marker.status.success(), "creation must attach the marker");
    assert!(String::from_utf8_lossy(&marker.stdout).starts_with("AGENTDESK_IDENTITY={"));
    assert!(first_send.unwrap().status.success());
    assert!(refused.is_err(), "unset marker is Unknown");
    assert_eq!(before, after);
}

/// Existing scripted panes explicitly carry management evidence at their process boundary.
pub(crate) fn managed_fixture_program(program: std::path::PathBuf) -> std::path::PathBuf {
    let program = crate::services::platform::binary_resolver::runtime_command(program)
        .unwrap()
        .get_program()
        .to_owned();
    let program = std::path::PathBuf::from(program);
    let identity =
        crate::services::session_identity_metadata::runtime_identity("scripted-fixture").unwrap();
    let assignment = identity.environment_assignment().unwrap();
    let wrapper = program.with_file_name(format!("marked-tmux-{}", uuid::Uuid::new_v4()));
    let quote = crate::services::process::shell_escape;
    std::fs::write(&wrapper, format!("#!/bin/sh\ncase \"$*\" in *show-environment*AGENTDESK_IDENTITY*) printf '%s\\n' {} ;; *) exec {} \"$@\" ;; esac\n", quote(&assignment), quote(&program.to_string_lossy()))).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    wrapper
}

/// Keeps a PATH-first stub's entry path while supplying its managed-session evidence.
pub(crate) fn mark_fixture_program(program: std::path::PathBuf) {
    let stub = program.with_file_name(format!("tmux-stub-{}", uuid::Uuid::new_v4()));
    std::fs::rename(&program, &stub).unwrap();
    let wrapper = managed_fixture_program(stub);
    std::fs::rename(wrapper, program).unwrap();
}

#[test]
fn identity_parser_preserves_equals_unicode_and_rejects_unknown_records() {
    use crate::domain::session_identity::managed::ManagedIdentity;
    let mut identity =
        crate::services::session_identity_metadata::runtime_identity("스마트홈-cc").unwrap();
    identity.logical_key = Some("claude/token/host:label=unchanged".into());
    let assignment = identity.environment_assignment().unwrap();
    assert_eq!(
        ManagedIdentity::decode_environment(&(assignment.clone() + "\n")),
        Some(identity)
    );
    for output in [
        "",
        "-AGENTDESK_IDENTITY",
        "OTHER={}",
        "AGENTDESK_IDENTITY={}",
        "AGENTDESK_IDENTITY=bad",
        &(assignment.clone() + "\nOTHER=bad"),
        &assignment.replace("\"v\":1", "\"v\":2"),
    ] {
        assert!(
            ManagedIdentity::decode_environment(output).is_none(),
            "must remain Unknown: {output}"
        );
    }
}

#[test]
fn identity_gate_refuses_malformed_foreign_and_failed_lookup() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rig = Rig::new();
    let name = format!("s1-unknown-{}", uuid::Uuid::new_v4());
    assert!(
        rig.raw(&["new-session", "-d", "-s", &name, "cat"])
            .status
            .success()
    );
    let mut foreign = crate::services::session_identity_metadata::runtime_identity(&name).unwrap();
    foreign.owner_scope = "f".repeat(64);
    let foreign = serde_json::to_string(&foreign).unwrap();
    for marker in ["{}", "bad=json", &foreign] {
        assert!(
            rig.raw(&[
                "set-environment",
                "-t",
                &format!("={name}"),
                "AGENTDESK_IDENTITY",
                marker
            ])
            .status
            .success()
        );
        let before = rig.snapshot(&name);
        let send = send_literal(&name, "unexpected");
        let kill = kill_session_output_timeout(&name, "unknown fixture", Duration::from_secs(1));
        assert!(send.is_err());
        assert!(kill.is_err());
        assert_eq!(before, rig.snapshot(&name));
    }
    assert!(
        rig.raw(&["kill-session", "-t", &exact_target(&name)])
            .status
            .success()
    );
    assert!(
        send_literal(&name, "unexpected").is_err(),
        "missing session is not ownership evidence"
    );
}

#[test]
fn identity_checked_panes_refuse_unmarked_submission_target() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rig = Rig::new();
    let name = format!("adk-cc-pane-{}", uuid::Uuid::new_v4());
    let composer = "printf '────────────────────\\n❯ \\n────────────────────\\n'; exec cat";
    assert!(
        rig.raw(&["new-session", "-d", "-s", &name, composer])
            .status
            .success()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !String::from_utf8_lossy(&rig.snapshot(&name).0).contains('❯') {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture composer ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Ready input evidence must not let an unmarked pane cross the identity gate.
    let transcript = rig.dir.path().join("idle.jsonl");
    std::fs::write(
        &transcript,
        "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
    )
    .unwrap();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        &name,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: transcript.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    crate::services::claude_tui::input::wait_for_prompt_ready(
        &name,
        crate::services::claude_tui::input::PromptReadinessKind::ProvenWarmFollowup,
        None,
    )
    .expect("unmarked fixture independently proves readiness");
    let before = rig.snapshot(&name);
    let program = rig.dir.path().join("tmux");
    let busy = crate::services::claude_tui::busy_inject::Pane::with_program(&name, program.clone());
    let operation = std::panic::catch_unwind(|| {
        crate::services::claude_tui::host_input::BusyOperation::for_session(
            &name,
            None,
            program.clone(),
            &crate::services::claude_tui::busy_inject::TIMING,
            false,
            std::sync::Arc::new(crate::services::provider::CancelToken::new()),
        )
    });
    let actor = crate::services::tui_input::actor::pane::TmuxPane::with_program(
        &name,
        program,
        Duration::from_secs(1),
    );
    use crate::services::claude_tui::host_input::InputTransport;
    let submit = crate::services::tui_input::submission_tmux::SubmissionTmux
        .send_literal(&name, "unexpected");
    let submitted = crate::services::claude_tui::input::send_prompt_with_readiness(
        &name,
        "unexpected",
        crate::services::claude_tui::input::PromptReadinessKind::ProvenWarmFollowup,
        None,
    );
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&name);
    let after = rig.snapshot(&name);
    let _ = rig.raw(&["kill-session", "-t", &exact_target(&name)]);
    assert!(
        operation
            .expect("an unmarked target must be refused without panicking")
            .is_err(),
        "the common operation must propagate the Pane ownership refusal"
    );
    use crate::services::claude_tui::guarded_outcome::SubmissionError;
    use crate::services::claude_tui::host_input::{InputRefusal, InputRun};
    assert_eq!(
        submitted,
        Err(SubmissionError::Guarded(InputRun::Refused(
            InputRefusal::IdentityMismatch
        )))
    );
    assert_eq!(
        (busy.is_err(), actor.is_err()),
        (true, true),
        "both direct Pane constructors must check ownership"
    );
    assert!(
        submit.is_err(),
        "submission transport must refuse before a key"
    );
    assert!(submit.unwrap_err().contains("NotSent"));
    assert_eq!(before, after);
}

#[test]
fn identity_legacy_gate_keeps_existing_prefix_compatibility_without_tmux_lookup() {
    assert!(
        session_identity::require_managed_with_program(
            "AgentDesk-legacy",
            std::path::Path::new("/nonexistent/s1-tmux"),
            Duration::ZERO
        )
        .is_ok()
    );
}
