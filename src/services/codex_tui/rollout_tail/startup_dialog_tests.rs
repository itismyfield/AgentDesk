use super::*;
use crate::config::TestEnvVarGuard as Guard;
use std::os::unix::fs::PermissionsExt;

const TRUST: &str =
    "Do you trust this folder?\n› 1. Yes, proceed\n  2. No, exit\nPress enter to continue";

fn fixture() -> (tempfile::TempDir, Vec<Guard>) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("home/sessions")).unwrap();
    let tmux = root.path().join("tmux");
    std::fs::write(
        &tmux,
        "#!/bin/sh\ncase \"$*\" in *capture-pane*) cat \"$I8165_PANE\";; *) exit 64;; esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
    let guards = vec![
        Guard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path()),
        Guard::prepend_path_after_shared_test_env_lock(root.path()),
        Guard::set_path_after_shared_test_env_lock("CODEX_HOME", &root.path().join("home")),
        Guard::set_path_after_shared_test_env_lock("I8165_PANE", &root.path().join("pane")),
    ];
    (root, guards)
}

fn wait(
    root: &Path,
    resumed: bool,
    cancel: Option<Arc<CancelToken>>,
) -> Result<CodexTuiTailResult, String> {
    let (tx, _rx) = std::sync::mpsc::channel();
    let options = RolloutTailOptions {
        wait_for_rollout: Duration::from_secs(2),
        tmux_session_name: Some("test-i8165-startup".into()),
        ..Default::default()
    };
    if resumed {
        tail_resumed_rollout_for_session_with_handoff_options(
            root,
            "session",
            &root.join("old.jsonl"),
            0,
            SystemTime::UNIX_EPOCH,
            &root.join("home/sessions"),
            tx,
            cancel,
            || true,
            options,
        )
    } else {
        tail_latest_rollout_for_cwd_with_handoff_options(
            root,
            SystemTime::UNIX_EPOCH,
            tx,
            cancel,
            || true,
            options,
        )
    }
}

#[test]
fn startup_dialog_fails_fresh_and_resumed_with_reason_and_cwd() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _guards) = fixture();
    for (pane, reason) in [
        (TRUST.to_string(), "trust"),
        ("Trust this folder? Codex can read, edit, and run files here.\n› 1. Trust and continue\n  2. Quit\nenter continue · esc quit".into(), "trust"),
        ("Welcome to Codex\n› 1. Sign in with ChatGPT\n  2. Use an API key\nPress Enter to continue".into(), "sign-in"),
        ("Update available\n› 1. Update now (runs npm install)\n  2. Skip until next version\nPress enter to continue".into(), "update"),
        (TRUST.replace("›", "\x1b[32m›\x1b[0m"), "trust"),
    ] {
        std::fs::write(root.path().join("pane"), pane).unwrap();
        for resumed in [false, true] {
            let error = wait(root.path(), resumed, None).unwrap_err();
            assert!(error.contains(reason) && error.contains(root.path().to_str().unwrap()), "{error}");
            assert!(!error.contains("Timeout"), "{error}");
        }
    }
}

#[test]
fn startup_dialog_ignores_prose_composer_and_capture_failure() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _guards) = fixture();
    for pane in [
        "The docs say: Do you trust this folder?",
        "› explain sign in\n  gpt-5.4 100% left",
        "Do you trust this folder?\n1. Yes, proceed\n2. No, exit",
        "Update available in the docs\n› summarize\n? for shortcuts",
    ] {
        std::fs::write(root.path().join("pane"), pane).unwrap();
        assert!(
            wait(root.path(), false, None)
                .unwrap_err()
                .contains("Timeout")
        );
    }
    std::fs::remove_file(root.path().join("pane")).unwrap();
    assert!(
        wait(root.path(), false, None)
            .unwrap_err()
            .contains("Timeout")
    );
}

#[test]
fn startup_dialog_preserves_cancellation() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _guards) = fixture();
    std::fs::write(root.path().join("pane"), TRUST).unwrap();
    let cancel = Arc::new(CancelToken::new());
    cancel
        .cancelled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    for resumed in [false, true] {
        assert!(
            wait(root.path(), resumed, Some(cancel.clone()))
                .unwrap_err()
                .starts_with("cancelled")
        );
    }
}

#[test]
fn startup_dialog_fails_verified_binding_wait_without_weakening_proof() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _guards) = fixture();
    std::fs::write(root.path().join("pane"), TRUST).unwrap();
    let error = crate::services::codex_tui::verified_tail::wait_for_binding(
        "test-i8165-no-verified-source",
        None,
        root.path(),
    )
    .unwrap_err();
    assert!(error.contains("folder trust") && error.contains(root.path().to_str().unwrap()));
}

#[test]
fn startup_dialog_does_not_mistake_quoted_menu_above_composer() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _guards) = fixture();
    std::fs::write(
        root.path().join("pane"),
        format!("{TRUST}\n› explain this menu\n  gpt-5.4 100% left"),
    )
    .unwrap();
    assert!(
        wait(root.path(), false, None)
            .unwrap_err()
            .contains("Timeout")
    );
}

#[test]
fn startup_dialog_capture_timeout_keeps_cancel_responsive() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _guards) = fixture();
    std::fs::write(root.path().join("tmux"), "#!/bin/sh\nsleep 5\n").unwrap();
    let cancel = Arc::new(CancelToken::new());
    let delayed = cancel.clone();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        delayed
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    });
    let start = Instant::now();
    let result = wait(root.path(), false, Some(cancel));
    crate::test_support::wait::join_within(release, Duration::from_secs(2));
    assert!(result.unwrap_err().starts_with("cancelled"));
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn startup_dialog_does_not_override_an_available_rollout() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _guards) = fixture();
    std::fs::write(root.path().join("pane"), TRUST).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let metadata = serde_json::json!({"type":"session_meta", "payload":{"id":id,"cwd":root.path(),"source":"cli"}});
    let body = concat!(
        r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"hello"}}"#,
        "\n",
    );
    std::fs::write(
        root.path()
            .join("home/sessions")
            .join(format!("rollout-{id}.jsonl")),
        format!("{metadata}\n{body}"),
    )
    .unwrap();
    let result = wait(root.path(), false, None).unwrap();
    assert!(matches!(
        result.read_result,
        ReadOutputResult::Completed { .. }
    ));
}

#[test]
fn startup_dialog_rechecks_rollout_created_during_capture() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _guards) = fixture();
    std::fs::write(root.path().join("pane"), TRUST).unwrap();
    let pending = root.path().join("pending");
    std::fs::create_dir(&pending).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let metadata = serde_json::json!({"type":"session_meta", "payload":{"id":id,"cwd":root.path(),"source":"cli"}});
    let answer = serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}});
    let end = serde_json::json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"hello"}});
    std::fs::write(
        pending.join(format!("rollout-{id}.jsonl")),
        format!("{metadata}\n{answer}\n{end}\n"),
    )
    .unwrap();
    std::fs::write(root.path().join("tmux"), "#!/bin/sh\ncase \"$*\" in *capture-pane*) root=${I8165_PANE%/*}; cp \"$root\"/pending/* \"$root/home/sessions/\"; cat \"$I8165_PANE\";; *) exit 64;; esac\n").unwrap();
    let result = wait(root.path(), false, None).unwrap();
    assert!(matches!(
        result.read_result,
        ReadOutputResult::Completed { .. }
    ));
}

// The admitted launch's startup check sees its trust menu first: it repairs it with the
// admission's one repair instead of failing, and the rollout the Enter lets through completes.
// A refused option keeps the startup failure and sends nothing.
#[test]
fn an_admitted_launch_repairs_its_trust_menu_before_the_startup_check_fails_it() {
    use crate::services::tui_o::channel_policy::{Adoption, BootChannels, first_prompt_digest};
    use crate::services::tui_prompt_dedupe::{
        self as dedupe, binding_context::PreparedIncarnation,
    };
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _guards) = fixture();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let negative = "Do you trust this folder?\n  1. Yes, proceed\n› 2. No, exit";
    for (channel, pane) in [(8_165_311u64, TRUST), (8_165_312, negative)] {
        dedupe::reset_state_for_tests();
        let cwd = root.path().join(format!("workspaces/project-{channel}"));
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let tmux = "test-i8165-startup";
        let sessions = root.path().join("home/sessions").canonicalize().unwrap();
        let config = serde_json::from_value(serde_json::json!({"server":{}, "agents":[{"id":"first", "name":"first", "channels":{"codex":{"id":channel.to_string(), "runtime":"tui"}}}], "tui_o":{"writer":{"channels":[channel], "late_adopt":true}}})).unwrap();
        let pending = |_: &_, _| {
            Ok(std::collections::BTreeMap::from([(
                channel,
                Adoption::Pending,
            )]))
        };
        let boot = BootChannels::validate(&config)
            .unwrap()
            .seeded(&config, pending)
            .unwrap();
        let candidate = boot.candidate(channel).unwrap().clone();
        let _boot = crate::services::tui_o::cutover::test_override::force_boot(boot);
        let token = CancelToken::new();
        let nonce = token.turn_nonce().unwrap();
        let admission = candidate
            .admit_first(channel, nonce, "hi", tmux, || Ok(0))
            .unwrap();
        let digests = (Some(first_prompt_digest("hi")), Some("legacy".into()));
        let prepared = PreparedIncarnation::prepare_pinned(
            "codex",
            tmux,
            Some(channel),
            None,
            false,
            Some(sessions.clone()),
            digests,
        )
        .unwrap();
        candidate.attach_first_execution(&prepared.context, "hi", Some(nonce));
        let marker = crate::services::tmux_common::session_temp_path(tmux, "spawn_nonce");
        std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(&marker, &prepared.context.execution_nonce).unwrap();
        dedupe::register_tmux_channel(tmux, channel);
        // The Enter lets Codex start: its rollout appears and the pane leaves the menu.
        let staged = root.path().join(format!("staged-{channel}"));
        std::fs::create_dir_all(&staged).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let meta =
            serde_json::json!({"type":"session_meta","payload":{"id":id,"cwd":cwd,"source":"cli"}});
        let answer = serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}});
        let end = serde_json::json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","last_agent_message":"hello"}});
        let rollout = staged.join(format!("rollout-{id}.jsonl"));
        std::fs::write(&rollout, format!("{meta}\n{answer}\n{end}\n")).unwrap();
        std::fs::write(root.path().join("pane"), pane).unwrap();
        let keys = root.path().join(format!("keys-{channel}"));
        let identity = crate::services::session_identity_metadata::runtime_identity("startup")
            .unwrap()
            .environment_assignment()
            .unwrap();
        let identity = crate::services::process::shell_escape(&identity);
        let script = format!(
            "#!/bin/sh\ncase \" $* \" in\n*show-environment*) printf '%s\\n' {identity};;\n*display-message*) printf '0\\n{}\\n';;\n*send-keys*) echo \"$*\" >> '{}'; cp '{}' '{}/';;\n*capture-pane*) if [ -f '{}' ]; then echo ready; else cat \"$I8165_PANE\"; fi;;\n*) exit 64;;\nesac\n",
            cwd.display(),
            keys.display(),
            rollout.display(),
            sessions.display(),
            keys.display(),
        );
        std::fs::write(root.path().join("tmux"), script).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let options = RolloutTailOptions {
            wait_for_rollout: Duration::from_secs(2),
            tmux_session_name: Some(tmux.into()),
            ..Default::default()
        };
        let result = tail_latest_rollout_for_cwd_with_handoff_options(
            &cwd,
            SystemTime::UNIX_EPOCH,
            tx,
            None,
            || true,
            options,
        );
        let enters = std::fs::read_to_string(&keys).map_or(0, |log| log.lines().count());
        let spent = candidate
            .first_trust_launch(tmux)
            .map(|(_, _, spent)| spent);
        if pane == TRUST {
            let result = result.unwrap();
            assert!(matches!(
                result.read_result,
                ReadOutputResult::Completed { .. }
            ));
            assert_eq!(
                enters, 1,
                "one Enter on the admitted launch's positive menu"
            );
            assert_eq!(spent, Some(true), "the admission's one repair is spent");
        } else {
            let error = result.unwrap_err();
            assert!(error.contains("trust"), "{error}");
            assert_eq!(enters, 0, "a refused option gets no Enter");
        }
        assert_eq!(
            candidate.peek(),
            Adoption::Deferred,
            "the menu action adopts nothing"
        );
        drop(admission);
    }
}
