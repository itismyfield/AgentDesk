#![cfg(unix)]

use super::*;
use crate::services::claude_tui::hook_server::{
    adoption_retry::DurableKind,
    observation_ingress::{IngressOutcome, NotApplicableReason, observe_binding_hook},
};
use crate::services::tui_prompt_dedupe::binding_context::{
    BINDING_HEADER, CapturedContext, HookBindingEnvelope, ObservedHookProcess, PreparedIncarnation,
};
use serde_json::json;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn legacy_canonical_read_failure_completes_ingress_and_another_getter() {
    run_legacy_ingress(false);
}

#[test]
fn legacy_channel_registration_serializes_hook_disk_and_memory_publication() {
    run_legacy_ingress(true);
}

fn run_legacy_ingress(race_channel_registration: bool) {
    const CHILD_ENV: &str = "ADK_CODEX_LEGACY_LOCK_CHILD";
    const CHILD_READY: &str = "ADK_CODEX_LEGACY_LOCK_READY";
    if std::env::var_os(CHILD_ENV).is_none() {
        let name = if race_channel_registration {
            "legacy_channel_registration_serializes_hook_disk_and_memory_publication"
        } else {
            "legacy_canonical_read_failure_completes_ingress_and_another_getter"
        };
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!(
                    "services::tui_prompt_dedupe::runtime_binding::codex_legacy_lock_tests::{name}"
                ),
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let output = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                if line == CHILD_READY {
                    let _ = ready_tx.send(Instant::now());
                }
                println!("{line}");
            }
        });
        // Allow slow child startup and fixtures without extending the STATE hang check.
        let ready_at = match ready_rx.recv_timeout(Duration::from_secs(180)) {
            Ok(ready_at) => ready_at,
            Err(error) => {
                let _ = child.kill();
                let status = child.wait().unwrap();
                output.join().unwrap();
                panic!("legacy ingress child did not become ready: {error} ({status})");
            }
        };
        let deadline = ready_at + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                output.join().unwrap();
                assert!(status.success(), "legacy ingress child failed: {status}");
                return;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                output.join().unwrap();
                panic!("legacy ingress held STATE after canonical context read failure");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe_lock = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _env) = super::super::binding_context::tests::fixture_after_shared_test_env_lock();
    super::super::reset_state_for_tests();
    binding_events::set_test_root(Some(root.path()));
    let sessions = root.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    let tmux = format!("legacy-lock-{}", uuid::Uuid::new_v4().simple());
    let command = "019e660d-4859-7522-9cee-8ba7c4e7c743";
    let incoming = "019e660d-4859-7522-9cee-8ba7c4e7c744";
    let prepared = PreparedIncarnation::prepare_at(
        "codex",
        &tmux,
        Some(584_509),
        Some(command),
        false,
        Some(sessions.clone()),
    )
    .unwrap();
    let context = prepared.context.clone();
    let spawn_nonce = crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce");
    fs::create_dir_all(Path::new(&spawn_nonce).parent().unwrap()).unwrap();
    fs::write(spawn_nonce, &context.execution_nonce).unwrap();
    let write_native = |id: &str| {
        let path = sessions.join(format!("rollout-{id}.jsonl"));
        let header = json!({"type":"session_meta", "payload":{
            "id":id, "source":"cli", "cwd":root.path(), "originator":"codex_cli_rs"
        }});
        fs::write(&path, format!("{header}\n")).unwrap();
        path
    };
    let old = write_native(command);
    let native = write_native(incoming);
    register_provider_session("codex", command, &tmux);
    register_tmux_channel(&tmux, 584_509);
    crate::services::codex_tui::session::install_codex_tui_runtime_binding(
        &tmux,
        Some(19),
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: old.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(command.into()),
            last_offset: 19,
            relay_last_offset: Some(19),
        },
    );
    fs::remove_file(&prepared.path).unwrap();
    assert!(
        super::super::binding_context::execution_context("codex", &context.execution_nonce)
            .is_err()
    );
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(context),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let registration = race_channel_registration.then(|| {
        let (start_tx, start_rx) = std::sync::mpsc::channel();
        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let pane = tmux.clone();
        let registration = std::thread::spawn(move || {
            start_rx.recv().unwrap();
            attempt_tx.send(()).unwrap();
            register_tmux_channel(&pane, 584_510);
            let _ = done_tx.send(());
        });
        codex_hook::AFTER_LEGACY_SNAPSHOT.with_borrow_mut(|slot| {
            *slot = Some(Box::new(move || {
                start_tx.send(()).unwrap();
                attempt_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                assert!(
                    matches!(
                        done_rx.recv_timeout(Duration::from_millis(200)),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    ),
                    "channel registration crossed the hook's source authority"
                );
            }));
        });
        registration
    });
    println!("{CHILD_READY}");
    std::io::stdout().flush().unwrap();
    let outcome = observe_binding_hook(
        "codex",
        "SessionStart",
        Some(command),
        Some(incoming),
        &json!({"session_id":incoming,"transcript_path":native,"source":"clear"}),
        &headers,
    );
    assert!(
        matches!(outcome, IngressOutcome::Durable(DurableKind::Adopted)),
        "{outcome:?}"
    );
    if let Some(registration) = registration {
        registration.join().unwrap();
        assert_eq!(owner_channel_for_tmux_session(&tmux), Some(584_510));
    }
    let restored = runtime_binding_for_tmux_session(&tmux).unwrap();
    assert_eq!(
        restored.output_path,
        native.canonicalize().unwrap().display().to_string()
    );
    assert_eq!(restored.session_id.as_deref(), Some(incoming));
    assert!(runtime_binding_for_tmux_session("another-legacy-lock-pane").is_none());
    let marker = crate::services::codex_tui::session::read_codex_tui_rollout_marker(&tmux).unwrap();
    assert_eq!(marker.rollout_path, native.canonicalize().unwrap());
    assert_eq!(marker.session_id.as_deref(), Some(incoming));
    if race_channel_registration {
        let before = fs::read(crate::services::tmux_common::session_temp_path(
            &tmux,
            crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        ))
        .ok();
        let rejected = observe_binding_hook(
            "codex",
            "SessionStart",
            Some(command),
            Some(command),
            &json!({"session_id":command,"transcript_path":old,"source":"clear"}),
            &headers,
        );
        assert!(matches!(
            rejected,
            IngressOutcome::NotApplicable(NotApplicableReason::CodexContextUnavailable)
        ));
        let marker =
            crate::services::codex_tui::session::read_codex_tui_rollout_marker(&tmux).unwrap();
        assert_eq!(marker.rollout_path, native.canonicalize().unwrap());
        assert_eq!(
            fs::read(crate::services::tmux_common::session_temp_path(
                &tmux,
                crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT
            ))
            .ok(),
            before
        );
        assert_eq!(runtime_binding_for_tmux_session(&tmux).unwrap(), restored);
        assert!(runtime_binding_for_tmux_session("another-legacy-lock-pane").is_none());
        register_tmux_channel(&tmux, 584_509);
    }
    let wrong = "019e660d-4859-7522-9cee-8ba7c4e7c745";
    let malformed = write_native(wrong);
    fs::write(
        &malformed,
        format!(
            "{}\n",
            json!({"type":"session_meta","payload":{
                "id":incoming,"source":"cli","cwd":root.path()
            }})
        ),
    )
    .unwrap();
    let rejected = observe_binding_hook(
        "codex",
        "SessionStart",
        Some(command),
        Some(wrong),
        &json!({"session_id":wrong,"transcript_path":malformed,"source":"clear"}),
        &headers,
    );
    assert!(matches!(
        rejected,
        IngressOutcome::NotApplicable(NotApplicableReason::CodexSourceRejected)
    ));
    assert_eq!(runtime_binding_for_tmux_session(&tmux).unwrap(), restored);
    binding_events::forget_channel_for_tests(584_509);
    binding_events::set_test_root(None);
    super::super::reset_state_for_tests();
}

#[test]
fn marker_io_guard_rejects_only_this_threads_binding_publication() {
    use crate::services::cluster::stream_relay::source_read_proof::tests::hold_revisions_for_tests;
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _state = crate::services::tui_prompt_dedupe::hold_binding_peek_lock_for_tests();
        let _revisions = hold_revisions_for_tests();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv();
    });
    held_rx.recv().unwrap();
    // A concurrent test thread holding either mutex is not this thread's marker IO.
    assert_marker_publication_unlocked_for_tests();
    release_tx.send(()).unwrap();
    holder.join().unwrap();

    let panic = std::panic::catch_unwind(|| {
        with_runtime_binding_authority("marker-guard-same-thread", |_| {
            assert_marker_publication_unlocked_for_tests();
        })
    })
    .expect_err("marker IO inside this thread's binding publication must be rejected");
    STATE.clear_poison();
    let message = panic
        .downcast_ref::<&str>()
        .map(|message| message.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned());
    assert_eq!(
        message.as_deref(),
        Some("binding cache mutex must not cover marker IO")
    );
}
