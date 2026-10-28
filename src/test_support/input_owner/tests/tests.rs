use super::*;
use serde_json::json;

pub(crate) fn fixture(
    host: &str,
    physical: &str,
) -> (tempfile::TempDir, Attempt, StrictOwnerEffectGuard) {
    let root = tempfile::tempdir().unwrap();
    let target = Target {
        provider: "codex".into(),
        namespace: "s5-b0".into(),
        root: root.path().display().to_string(),
        host: host.into(),
        physical: physical.into(),
        execution: "execution-A".into(),
    };
    let attempt = Attempt {
        target: target.clone(),
        sources: vec![(100, 5), (101, 6)],
        primary: (100, 5),
        nonce: "attempt-A".into(),
        native_source: json!({"kind":"pre_source"}).to_string(),
    };
    let guard = StrictOwnerEffectGuard::new(target);
    guard.entry_reached();
    guard.admission(&attempt);
    let proof = json!({"version":2,"namespace":attempt.target.namespace,
        "head_injection":{"identity":{"primary":100,"queued_generation":5,"sources":[{"message_id":100,"queued_generation":5},
            {"message_id":101,"queued_generation":6}]}, "phase":"may_have_sent",
            "attempt":{"nonce":attempt.nonce,"provider":attempt.target.provider,
                "source":{"kind":"pre_source"},
                "target":{"host":attempt.target.host,"owner":attempt.target.physical,
                    "root":attempt.target.root,"execution_nonce":attempt.target.execution}},
            "payload":{"frame":"owner"},"payload_window":{"kind":"started","at_ms":1000}}});
    let path = root.path().join("proof.json");
    std::fs::write(&path, proof.to_string()).unwrap();
    guard.observe_durable(&attempt, &path).unwrap();
    (root, attempt, guard)
}

#[test]
fn s5_b0_rejects_missing_foreign_and_unscoped_even_after_owner_returns() {
    let (_root, attempt, guard) = fixture("fixture", &uuid::Uuid::new_v4().to_string());
    assert!(before("fixture", &attempt.target.physical, Op::Text, b"fallback").is_err());
    let mut missing = attempt.clone();
    missing.nonce = "missing-proof".into();
    {
        let _scope = guard.scope(Scope::owner(missing));
        assert!(before("fixture", &attempt.target.physical, Op::Text, b"missing").is_err());
    }
    for dimension in 0..9 {
        let mut foreign = attempt.clone();
        match dimension {
            0 => foreign.target.provider = "claude".into(),
            1 => foreign.target.namespace.push('x'),
            2 => foreign.target.root.push('x'),
            3 => foreign.target.physical.push('x'),
            4 => foreign.target.execution.push('x'),
            5 => foreign.sources[1].1 += 1,
            6 => foreign.nonce.push('x'),
            7 => foreign.native_source.push('x'),
            _ => foreign.primary.0 += 1,
        }
        let _scope = guard.scope(Scope::owner(foreign));
        assert!(before("fixture", &attempt.target.physical, Op::Text, b"foreign").is_err());
    }
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        before("fixture", &attempt.target.physical, Op::Text, b"owner")
            .unwrap()
            .written();
        std::thread::scope(|threads| {
            threads
                .spawn(|| {
                    assert!(
                        before("fixture", &attempt.target.physical, Op::Text, b"lost scope")
                            .is_err()
                    );
                })
                .join()
                .unwrap();
        });
    }
    assert!(
        before(
            "fixture",
            &attempt.target.physical,
            Op::Text,
            b"late fallback"
        )
        .is_err()
    );
    let counts = guard.counts();
    assert_eq!(counts.attempted_legacy, 13);
    assert_eq!(counts.owner_content.get(&attempt), Some(&1));
    assert_eq!(counts.events.iter().filter(|e| !e.accepted).count(), 13);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            guard.assert_owner_content(&attempt, 1);
        }))
        .is_err(),
        "positive owner content alone cannot hide a fallback attempt"
    );
}

#[test]
fn s5_b0_same_tui_preserved_4h_never_counts_as_owner_success() {
    let (_root, attempt, guard) = fixture("fixture", &uuid::Uuid::new_v4().to_string());
    guard.preserve_4h("legacy-http:/turn/deliver:origin-1", b"obligated delivery");
    {
        let _scope = guard.scope(Scope::Preserved4h(
            "legacy-http:/turn/deliver:origin-1".into(),
        ));
        before(
            "fixture",
            &attempt.target.physical,
            Op::Text,
            b"obligated delivery",
        )
        .unwrap()
        .written();
    }
    let counts = guard.counts();
    assert_eq!(counts.preserved_4h_content.values().sum::<usize>(), 1);
    assert!(counts.owner_content.is_empty());
    assert_eq!(counts.attempted_legacy, 0);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            guard.assert_owner_content(&attempt, 1);
        }))
        .is_err()
    );
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        before(
            "fixture",
            &attempt.target.physical,
            Op::Text,
            b"target inbound",
        )
        .unwrap()
        .written();
    }
    guard.assert_owner_content(&attempt, 1);
    let _scope = guard.scope(Scope::Preserved4h(
        "legacy-http:/turn/deliver:origin-1".into(),
    ));
    assert!(
        before(
            "fixture",
            &attempt.target.physical,
            Op::Text,
            b"target inbound"
        )
        .is_err()
    );
}

#[test]
fn s5_b0_cross_thread_barrier_and_distinct_targets_keep_writer_intervals() {
    let (_a, attempt, guard) = fixture("fixture", &uuid::Uuid::new_v4().to_string());
    let (_b, other, second) = fixture("fixture", &uuid::Uuid::new_v4().to_string());
    let _scope = guard.scope(Scope::owner(attempt.clone()));
    let flight = before("fixture", &attempt.target.physical, Op::Paste, b"A").unwrap();
    flight.written();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|threads| {
        threads.spawn(|| {
            let _scope = guard.scope(Scope::owner(attempt.clone()));
            barrier.wait();
            assert!(before("fixture", &attempt.target.physical, Op::Text, b"overlap").is_err());
            let _other = second.scope(Scope::owner(other.clone()));
            before("fixture", &other.target.physical, Op::Text, b"B")
                .unwrap()
                .written();
        });
        barrier.wait();
    });
    drop(flight);
    assert_eq!(guard.counts().max_writers, 1);
    assert_eq!(guard.counts().attempted_legacy, 1);
    second.assert_owner_content(&other, 1);
}

#[test]
#[cfg(unix)]
fn s5_b0_real_tmux_leaves_reject_before_transport_and_trace_owner_commands() {
    use crate::services::platform::tmux;
    use crate::services::session_host::{
        HostKind, HostSessionRef, InteractiveSessionHost, TmuxHost,
    };
    let _env = crate::config::TestRuntimeRootGuard::new();
    let (root, attempt, guard) = fixture("tmux", "s5-b0-tmux");
    let _fake = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux(root.path());
    assert!(tmux::send_literal("s5-b0-tmux", "fallback").is_err());
    assert!(tmux::paste_buffer("s5-b0-tmux", "s5-buffer", true).is_err());
    assert!(
        tmux::send_keys_timeout(
            "s5-b0-tmux",
            &["fallback"],
            std::time::Duration::from_secs(1)
        )
        .is_err()
    );
    // The stub writes under the isolated runtime root, so rejected calls leave no command log.
    let runtime = std::path::PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    assert!(!runtime.join("tmux.calls").exists());
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        tmux::send_literal("s5-b0-tmux", "owner").unwrap();
        TmuxHost
            .send_text(
                HostSessionRef {
                    kind: HostKind::Tmux,
                    name: "s5-b0-tmux",
                },
                "generic owner",
            )
            .unwrap();
        tmux::load_buffer("s5-buffer", "paste owner").unwrap();
        tmux::paste_buffer("s5-b0-tmux", "s5-buffer", true).unwrap();
        tmux::send_keys("s5-b0-tmux", &["Enter"]).unwrap();
        tmux::send_keys("s5-b0-tmux", &["C-s"]).unwrap();
        tmux::capture_pane("s5-b0-tmux", -10).unwrap();
    }
    let counts = guard.counts();
    assert_eq!(counts.owner_content.get(&attempt), Some(&3));
    assert_eq!(counts.attempted_legacy, 3);
    assert!(
        counts
            .events
            .iter()
            .any(|e| e.op == Op::Capture && e.accepted)
    );
    assert!(
        counts
            .events
            .iter()
            .any(|e| e.op == Op::Enter && e.accepted)
    );
    assert!(
        counts
            .events
            .iter()
            .any(|e| e.op == Op::Control && e.accepted)
    );
    assert!(
        counts
            .events
            .iter()
            .any(|e| e.bytes == 5 && e.scope == Some(Scope::owner(attempt.clone())))
    );
    assert_eq!(
        std::fs::read_to_string(runtime.join("tmux.calls"))
            .unwrap()
            .lines()
            .count(),
        7
    );
}

#[test]
fn s5_b0_tmux_argv_and_key_only_classification() {
    let (_root, attempt, guard) = fixture("tmux", &uuid::Uuid::new_v4().to_string());
    let mut command = std::process::Command::new("fixture-tmux");
    command.args([
        "new-session",
        "-s",
        &attempt.target.physical,
        "codex -- fixture-prompt",
    ]);
    guard.expect_argv("codex -- fixture-prompt", b"fixture-prompt");
    assert!(tmux(&command).is_err());
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        tmux(&command).unwrap().written();
    }
    assert_eq!(guard.counts().owner_content.get(&attempt), Some(&1));
    assert_eq!(guard.counts().events.last().unwrap().op, Op::Argv);
}

#[test]
fn s5_b0_admitted_command_without_transport_write_cannot_pass() {
    let (_root, attempt, guard) = fixture("fixture", &uuid::Uuid::new_v4().to_string());
    let _scope = guard.scope(Scope::owner(attempt.clone()));
    drop(
        before(
            "fixture",
            &attempt.target.physical,
            Op::Text,
            b"failed write",
        )
        .unwrap(),
    );
    assert!(guard.counts().owner_content.is_empty());
    assert!(!guard.counts().events[0].written);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            guard.assert_owner_content(&attempt, 1);
        }))
        .is_err()
    );
    before("fixture", &attempt.target.physical, Op::Text, b"written")
        .unwrap()
        .written();
    guard.assert_owner_content(&attempt, 1);
}

#[test]
#[cfg(unix)]
fn s5_b0_real_cold_argv_create_is_content_once_and_has_no_paste() {
    use crate::services::platform::tmux;
    let _env = crate::config::TestRuntimeRootGuard::new();
    let (root, attempt, guard) = fixture("tmux", "s5-b0-argv");
    let _fake = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux(root.path());
    let shell = "bash /fixture/prepared-script.sh";
    guard.expect_argv(shell, b"actual argv prompt");
    assert!(tmux::create_session("s5-b0-argv", None, shell).is_err());
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        tmux::create_session("s5-b0-argv", None, shell).unwrap();
    }
    let counts = guard.counts();
    assert_eq!(counts.owner_content.get(&attempt), Some(&1));
    assert_eq!(
        counts
            .events
            .iter()
            .filter(|event| event.op == Op::Argv && event.written)
            .count(),
        1
    );
    assert!(!counts.events.iter().any(|event| event.op == Op::Paste));
    assert_eq!(counts.attempted_legacy, 1);
}

#[test]
#[cfg(unix)]
fn s5_b0_real_same_tui_4h_and_owner_effects_have_separate_positive_counts() {
    use crate::services::platform::tmux;
    let _env = crate::config::TestRuntimeRootGuard::new();
    let (root, attempt, guard) = fixture("tmux", "s5-b0-coexist");
    let _fake = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux(root.path());
    guard.preserve_4h("legacy-http:/turn/deliver:stable-origin", b"4h delivery");
    {
        let _scope = guard.scope(Scope::Preserved4h(
            "legacy-http:/turn/deliver:stable-origin".into(),
        ));
        tmux::send_literal("s5-b0-coexist", "4h delivery").unwrap();
    }
    assert!(guard.counts().owner_content.is_empty());
    assert_eq!(
        guard.counts().preserved_4h_content.values().sum::<usize>(),
        1
    );
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        tmux::send_literal("s5-b0-coexist", "owner inbound").unwrap();
    }
    guard.assert_owner_content(&attempt, 1);
    assert_eq!(
        guard.counts().preserved_4h_content.values().sum::<usize>(),
        1
    );
}

#[test]
fn s5_b0_auxiliary_underscore_cannot_substitute_for_the_actual_payload() {
    let (_root, attempt, guard) = fixture("fixture", &uuid::Uuid::new_v4().to_string());
    let _scope = guard.scope(Scope::owner(attempt.clone()));
    before("fixture", &attempt.target.physical, Op::Literal, b"_")
        .unwrap()
        .written();
    assert_eq!(guard.counts().owner_content.get(&attempt), Some(&1));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            guard.assert_owner_payload(&attempt, b"actual prepared frame", 1);
        }))
        .is_err()
    );
    guard.assert_owner_payload(&attempt, b"_", 1);
}
