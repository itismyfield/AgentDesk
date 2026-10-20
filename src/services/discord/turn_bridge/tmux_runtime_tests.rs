use super::*;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

const RAW_SUBAGENT: &str = r#"<subagent_notification>
{"agent_path":"/tmp/private-agent","status":{"completed":"Read-only review complete.\n\n1. Check relay path."}}
</subagent_notification>"#;
const CHROME_RAW_SUBAGENT: &str = "No response requested.\n<subagent_notification>{\"agent_path\":\"/tmp/private-agent\",\"status\":{\"completed\":\"Read-only review complete.\"}}</subagent_notification>";
pub(super) static SIGINT_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[test]
fn stale_inflight_message_hides_raw_subagent_notification() {
    let message = stale_inflight_message(RAW_SUBAGENT);

    assert!(message.contains("Subagent completed"));
    assert!(message.contains("Read-only review complete."));
    assert!(message.contains("[Interrupted by restart]"));
    assert!(!message.contains("<subagent_notification>"));
    assert!(!message.contains("agent_path"));
    assert!(!message.contains("/tmp/private-agent"));
    assert!(!message.contains("{\""));
}

#[test]
fn handoff_interrupted_message_hides_raw_subagent_notification() {
    let message = handoff_interrupted_message(InflightRestartMode::DrainRestart, RAW_SUBAGENT);

    assert!(message.contains("Subagent completed"));
    assert!(message.contains("Read-only review complete."));
    assert!(message.contains("[Restart handoff incomplete]"));
    assert!(!message.contains("<subagent_notification>"));
    assert!(!message.contains("agent_path"));
    assert!(!message.contains("/tmp/private-agent"));
    assert!(!message.contains("{\""));
}

#[test]
fn restart_messages_sanitize_subagent_after_tui_chrome_strip() {
    let stale = stale_inflight_message(CHROME_RAW_SUBAGENT);
    assert!(stale.contains("Subagent completed"));
    assert!(stale.contains("Read-only review complete."));
    assert!(!stale.contains("No response requested."));
    assert!(!stale.contains("<subagent_notification>"));
    assert!(!stale.contains("agent_path"));

    let handoff =
        handoff_interrupted_message(InflightRestartMode::DrainRestart, CHROME_RAW_SUBAGENT);
    assert!(handoff.contains("Subagent completed"));
    assert!(!handoff.contains("<subagent_notification>"));
    assert!(!handoff.contains("agent_path"));
}

#[cfg(unix)]
#[test]
fn issue_4112_pipe_interrupt_sends_sigint_and_marks_stopped() {
    let _guard = SIGINT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    super::stop_host::tests::run(async {
        let _ = process_table::take_sigint_test_events();
        let session_name = format!("pipe-cancel-{}", uuid::Uuid::new_v4());
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        crate::services::session_backend::insert_process_session(
            session_name.clone(),
            crate::services::session_backend::SessionHandle::TestProcess { pid: 4112, alive },
        );
        let token = std::sync::Arc::new(CancelToken::new());
        token.store_child_pid(4112);

        let outcome =
            interrupt_provider_cli_turn(&ProviderKind::Claude, &token, "explicit_stop").await;

        assert_eq!(process_table::take_sigint_test_events(), vec![4112]);
        assert_eq!(outcome.tmux_session, None);
        assert_eq!(outcome.fallback_sigint_pid, Some(4112));
        assert!(outcome.missing_tmux_session);
        assert!(!outcome.sigint_target_missing);
        assert!(crate::services::session_backend::process_session_was_stopped(&session_name));
        assert!(!crate::services::session_backend::process_session_is_alive(
            &session_name
        ));
    });
}

#[cfg(unix)]
#[test]
fn issue_3169_process_backend_anonymous_teardown_is_noop() {
    let _guard = SIGINT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    super::stop_host::tests::run(async {
        let _ = process_table::take_sigint_test_events();
        let session_name = format!("pipe-anonymous-teardown-{}", uuid::Uuid::new_v4());
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        crate::services::session_backend::insert_process_session(
            session_name.clone(),
            crate::services::session_backend::SessionHandle::TestProcess {
                pid: 43169,
                alive: alive.clone(),
            },
        );
        let token = std::sync::Arc::new(CancelToken::new());
        token.store_child_pid(43169);

        let outcome = interrupt_provider_cli_turn(
            &ProviderKind::Claude,
            &token,
            ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON,
        )
        .await;

        assert!(process_table::take_sigint_test_events().is_empty());
        assert_eq!(outcome.tmux_session, None);
        assert_eq!(outcome.fallback_sigint_pid, None);
        assert!(!outcome.sigint_target_missing);
        assert!(!crate::services::session_backend::process_session_was_stopped(&session_name));
        assert!(crate::services::session_backend::process_session_is_alive(
            &session_name
        ));

        if let Some(handle) =
            crate::services::session_backend::remove_process_session(&session_name)
        {
            crate::services::session_backend::terminate_process_handle(handle);
        }
    });
}

#[cfg(unix)]
#[test]
fn issue_4112_process_backend_registry_miss_skips_sigint() {
    let _guard = SIGINT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    super::stop_host::tests::run(async {
        let _ = process_table::take_sigint_test_events();
        let token = std::sync::Arc::new(CancelToken::new());
        token.store_child_pid(499_112);

        let outcome =
            interrupt_provider_cli_turn(&ProviderKind::Claude, &token, "explicit_stop").await;

        assert!(process_table::take_sigint_test_events().is_empty());
        assert_eq!(outcome.fallback_sigint_pid, None);
        assert!(outcome.sigint_target_missing);
    });
}

#[test]
fn issue_4112_preserve_session_cancel_does_not_mark_process_backend_stopped() {
    let session_name = format!("pipe-preserve-mark-only-{}", uuid::Uuid::new_v4());
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    crate::services::session_backend::insert_process_session(
        session_name.clone(),
        crate::services::session_backend::SessionHandle::TestProcess {
            pid: 44_112,
            alive: alive.clone(),
        },
    );
    let token = std::sync::Arc::new(CancelToken::new());
    token.store_child_pid(44_112);

    let termination_recorded =
        cancel_active_token(&token, TmuxCleanupPolicy::PreserveSession, "auto_heal");

    assert!(!termination_recorded);
    assert!(token.cancelled.load(Ordering::Relaxed));
    assert!(!crate::services::session_backend::process_session_was_stopped(&session_name));
    assert!(crate::services::session_backend::process_session_is_alive(
        &session_name
    ));

    if let Some(handle) = crate::services::session_backend::remove_process_session(&session_name) {
        crate::services::session_backend::terminate_process_handle(handle);
    }
}

#[cfg(unix)]
#[test]
fn issue_4112_process_backend_stop_hard_stops_sigint_ignoring_provider() {
    let _guard = SIGINT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    super::stop_host::tests::run(async {
        let _ = process_table::take_sigint_test_events();
        let session_name = format!("pipe-hard-stop-{}", uuid::Uuid::new_v4());
        let pid_path =
            std::env::temp_dir().join(format!("agentdesk-provider-pid-{}", uuid::Uuid::new_v4()));
        let pid_path_arg = crate::services::process::shell_escape(&pid_path.display().to_string());
        let wrapper_script = format!(
            "bash -c 'trap \"\" INT; while :; do read -t 1 _ || true; done' & echo $! > {pid_path_arg}; wait $!"
        );
        let mut child = std::process::Command::new("bash")
            .arg("-c")
            .arg(wrapper_script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn process-backend wrapper fixture");
        let wrapper_pid = child.id();
        let child_stdin = child.stdin.take().expect("capture wrapper stdin");

        let mut provider_pid = None;
        for _ in 0..50 {
            if let Ok(raw) = std::fs::read_to_string(&pid_path)
                && let Ok(pid) = raw.trim().parse::<u32>()
            {
                provider_pid = Some(pid);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let provider_pid = provider_pid.expect("provider fixture pid should be visible");
        process_table::set_process_backend_provider_pid_for_test(wrapper_pid, provider_pid);

        crate::services::session_backend::insert_process_session(
            session_name.clone(),
            crate::services::session_backend::SessionHandle::Process {
                child_stdin: std::sync::Arc::new(std::sync::Mutex::new(Some(child_stdin))),
                child: std::sync::Arc::new(std::sync::Mutex::new(Some(child))),
                pid: wrapper_pid,
                output: std::sync::Arc::new(std::sync::Mutex::new(tempfile::tempfile().unwrap())),
            },
        );
        let token = std::sync::Arc::new(CancelToken::new());
        token.store_child_pid(wrapper_pid);

        let termination_recorded = stop_active_turn(
            &ProviderKind::Claude,
            &token,
            TmuxCleanupPolicy::PreserveSession,
            "explicit_stop",
        )
        .await;

        assert!(!termination_recorded);
        assert_eq!(process_table::take_sigint_test_events(), vec![provider_pid]);
        assert!(crate::services::session_backend::process_session_was_stopped(&session_name));
        assert!(wait_for_pid_exit(provider_pid, Duration::from_secs(2)).await);
        assert!(wait_for_pid_exit(wrapper_pid, Duration::from_secs(2)).await);
        process_table::clear_process_backend_provider_pid_for_test(wrapper_pid);
        let _ = std::fs::remove_file(pid_path);
    });
}

#[test]
fn issue_4112_tmux_hard_stop_policy_preserves_existing_provider_behavior() {
    assert_eq!(
        hard_stop_pid_for_unresponsive_provider(
            &ProviderKind::Codex,
            TmuxCleanupPolicy::PreserveSession,
            true,
            false,
            Some(41),
            None,
            Some(7),
        ),
        Some(41),
        "non-pane provider PID remains the tmux hard-stop candidate"
    );
    assert_eq!(
        hard_stop_pid_for_unresponsive_provider(
            &ProviderKind::Codex,
            TmuxCleanupPolicy::PreserveSession,
            true,
            true,
            Some(41),
            None,
            Some(7),
        ),
        None,
        "ready tmux panes are still not hard-killed"
    );
    assert_eq!(
        hard_stop_pid_for_unresponsive_provider(
            &ProviderKind::Claude,
            TmuxCleanupPolicy::PreserveSession,
            true,
            false,
            Some(41),
            None,
            Some(7),
        ),
        None,
        "claude preserve-session tmux turns still avoid hard-killing the CLI"
    );
}
