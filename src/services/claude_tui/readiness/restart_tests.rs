//! A pane that outlived the daemon restart, a turn's trailing session metadata, and a resumed run
//! proving its own turn.
use super::*;

fn finished(r: &Rig) {
    r.append(json!({"type":"user","timestamp":Utc::now(),
        "message":{"role":"user","content":"earlier prompt"}}));
    r.assistant("earlier answer");
    r.append(json!({"type":"system","subtype":"stop_hook_summary"}));
    r.terminal();
}
fn old_launch() -> Rig {
    initialize();
    Rig::launched(None, *STARTED - chrono::Duration::hours(1))
}
fn restored(r: &Rig) -> Option<bool> {
    observe_restored(&r.session, Some(&r.path)).map(|proof| proof.restart())
}

#[test]
fn a_launch_older_than_the_restart_is_ready_on_its_last_hard_terminal() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = old_launch();
    finished(&r);
    daemon_restart(&r.session);
    assert!(!r.ready(), "the shared observe keeps the restart floor");
    assert_eq!(restored(&r), Some(true));
    let reservation = reserve(observe_restored(&r.session, Some(&r.path)).unwrap()).unwrap();
    assert!(reservation.current());
    assert_eq!(restored(&r), None, "one reservation at a time");
    reservation.not_sent();
    assert_eq!(
        restored(&r),
        Some(true),
        "a known pre-key refusal releases it"
    );
    let reservation = reserve(observe_restored(&r.session, Some(&r.path)).unwrap()).unwrap();
    reservation.sent();
    r.append(json!({"type":"user","timestamp":Utc::now(),
        "message":{"role":"user","content":"one original"}}));
    assert_eq!(restored(&r), None, "our turn runs");
    r.assistant("its answer");
    r.terminal();
    assert!(r.ready(), "our turn's terminal passes the floor");
}

#[test]
fn a_new_incarnation_or_a_moved_source_never_rests_on_the_restart_floor() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    initialize();
    let fresh = Rig::launched(None, Utc::now());
    finished(&fresh);
    daemon_restart(&fresh.session);
    assert_eq!(
        restored(&fresh),
        None,
        "launched after this run: its Start decides"
    );

    // A same-length replacement shows only in the file identity, which only a unix stamp carries.
    #[cfg(unix)]
    {
        let r = old_launch();
        finished(&r);
        daemon_restart(&r.session);
        assert_eq!(restored(&r), Some(true));
        let copy = r.path.with_extension("copy");
        fs::copy(&r.path, &copy).unwrap();
        fs::rename(&copy, &r.path).unwrap();
        assert_eq!(
            restored(&r),
            None,
            "a replaced file's floor is no restart floor"
        );
    }

    let r = old_launch();
    finished(&r);
    daemon_restart(&r.session);
    assert_eq!(
        observe_restored(&r.session, Some(&r.root.join("other.jsonl"))).map(|p| p.restart()),
        None,
        "another source"
    );
}

type After = fn(&Rig);

#[test]
fn anything_after_the_terminal_or_an_open_fence_keeps_the_floor() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let cases: [(&str, After); 5] = [
        ("an answer streaming", |r| r.assistant("still running")),
        ("a prompt", |r| {
            r.append(json!({"type":"user","timestamp":Utc::now(),
                "message":{"role":"user","content":"typed"}}))
        }),
        ("a summary only", |r| {
            r.assistant("answer");
            r.append(json!({"type":"system","subtype":"stop_hook_summary"}));
        }),
        ("a permission marker after the restart", |r| {
            r.append(json!({"type":"permission-mode","permissionMode":"default"}))
        }),
        ("a continuing Stop", |r| {
            note_hook(
                "claude",
                &HookEventKind::Stop,
                &r.session,
                &json!({"session_id":r.ctx.expected_native_session_id,"transcript_path":r.path,
                    "last_assistant_message":"earlier answer"}),
                Some(&r.ctx),
                Some(Utc::now()),
                true,
            )
        }),
    ];
    for (name, after) in cases {
        let r = old_launch();
        finished(&r);
        daemon_restart(&r.session);
        assert_eq!(restored(&r), Some(true), "{name}: the floor alone admits");
        after(&r);
        assert_eq!(restored(&r), None, "{name}");
    }
}

/// A turn's session-state block as Claude writes it after the turn's `turn_duration`.
const ADK_CC_TAIL: [&str; 9] = [
    "last-prompt",
    "ai-title",
    "mode",
    "permission-mode",
    "session-owner",
    "atis-latch",
    "repl-mode",
    "pr-link",
    "frame-link",
];
fn metadata(r: &Rig, kinds: &[&str]) {
    for kind in kinds {
        r.append(match *kind {
            "pr-link" | "frame-link" => json!({"type":kind,"timestamp":Utc::now()}),
            "permission-mode" => json!({"type":kind,"permissionMode":"bypassPermissions"}),
            _ => json!({"type":kind}),
        });
    }
}

#[test]
fn a_turns_trailing_session_metadata_keeps_its_hard_terminal() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    finished(&r);
    metadata(&r, &ADK_CC_TAIL);
    assert!(r.ready(), "the adk-cc tail is that turn's end");
    for extra in [
        "cost-state",
        "tools-used",
        "file-history-snapshot",
        "queue-operation",
    ] {
        metadata(&r, &[extra]);
        assert!(r.ready(), "{extra}");
    }

    let r = old_launch();
    finished(&r);
    metadata(&r, &ADK_CC_TAIL);
    daemon_restart(&r.session);
    assert_eq!(
        restored(&r),
        Some(true),
        "and the restart floor rests on it"
    );
}

/// What Claude 2.1.330+ writes after a turn's end; a long idle adds the away recap.
fn data_flow(r: &Rig) {
    r.append(json!({"type":"data-flow-records","records":[{"response_id":"r1"}],"v":1}));
}
fn recap(r: &Rig) {
    data_flow(r);
    r.append(json!({"type":"system","subtype":"away_summary","content":"recap"}));
}

#[test]
fn an_away_recap_after_the_turn_keeps_its_hard_terminal() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    finished(&r);
    recap(&r);
    assert!(r.ready(), "the recap is no turn record");
    let r = old_launch();
    finished(&r);
    recap(&r);
    daemon_restart(&r.session);
    assert!(restored(&r) == Some(true), "the restart floor rests on it");
}

#[test]
fn trailing_data_flow_records_keep_the_hard_terminal() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    finished(&r);
    (0..3).for_each(|_| data_flow(&r));
    assert!(r.ready());
}

#[test]
fn turn_records_after_the_metadata_still_run() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (name, record) in [
        (
            "an answer",
            json!({"type":"assistant","timestamp":Utc::now(),
            "message":{"role":"assistant","content":[{"type":"text","text":"more"}]}}),
        ),
        (
            "a prompt",
            json!({"type":"user","timestamp":Utc::now(),
            "message":{"role":"user","content":"next"}}),
        ),
        (
            "an attachment",
            json!({"type":"attachment","timestamp":Utc::now()}),
        ),
        (
            "an api request",
            json!({"type":"api-request","timestamp":Utc::now()}),
        ),
        ("an unknown record", json!({"type":"something-new"})),
        (
            "a task",
            json!({"type":"system","subtype":"scheduled_task_fire"}),
        ),
        (
            "a compaction",
            json!({"type":"system","subtype":"compact_boundary"}),
        ),
    ] {
        let r = Rig::new();
        finished(&r);
        metadata(&r, &ADK_CC_TAIL);
        recap(&r);
        r.append(record);
        assert!(!r.ready(), "{name}");
    }
}

/// The child half of `the_boot_clock_starts_before_any_new_incarnation`, run in a fresh process.
#[test]
fn boot_clock_child() {
    let Ok(order) = std::env::var("AGENTDESK_I8161_BOOT_CHILD") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    if order == "first" {
        initialize();
    }
    let r = Rig::launched(None, Utc::now());
    std::thread::sleep(Duration::from_millis(5));
    initialize();
    finished(&r);
    let admitted = restored(&r) == Some(true);
    assert_eq!(admitted, order == "late", "order {order}");
    let mark = std::env::var("AGENTDESK_I8161_BOOT_MARK").unwrap();
    fs::write(mark, format!("{order}:{admitted}")).unwrap();
}

#[test]
fn the_boot_clock_starts_before_any_new_incarnation() {
    let dir = tempfile::tempdir().unwrap();
    for order in ["first", "late"] {
        let mark = dir.path().join(order);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "services::claude_tui::readiness::tests::restart_tests::boot_clock_child",
                "--test-threads=1",
                "--quiet",
            ])
            .env("AGENTDESK_I8161_BOOT_CHILD", order)
            .env("AGENTDESK_I8161_BOOT_MARK", &mark)
            .status()
            .unwrap();
        assert!(status.success(), "{order}: the child's claim failed");
        let admitted = order == "late";
        assert_eq!(
            fs::read_to_string(&mark).unwrap(),
            format!("{order}:{admitted}"),
            "the child ran"
        );
    }
}

/// A run resumed over an older history of `filler` answer bytes ending in a hard terminal.
fn resumed_over_history(filler: usize) -> Rig {
    initialize();
    let r = Rig::launched(None, Utc::now());
    let old = Utc::now() - chrono::Duration::hours(1);
    r.append(json!({"type":"user","timestamp":old,
        "message":{"role":"user","content":"earlier prompt"}}));
    r.append(json!({"type":"assistant","timestamp":old,
        "message":{"role":"assistant","content":[{"type":"text","text":"x".repeat(filler)}]}}));
    r.terminal();
    r.hook("resume");
    r
}
fn our_turn(r: &Rig, answer: &str) {
    r.append(json!({"type":"user","timestamp":Utc::now(),
        "message":{"role":"user","content":"one original"}}));
    r.assistant(answer);
    r.terminal();
}

#[test]
fn a_resumed_runs_oversized_answer_proves_its_turn_on_every_look() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = resumed_over_history(16);
    reserve(observe(&r.session, Some(&r.path)).unwrap())
        .unwrap()
        .sent();
    our_turn(&r, &"x".repeat(512 * 1024));
    assert!(r.ready(), "the look that consumes our reservation");
    let proof = observe(&r.session, Some(&r.path)).expect("the same bytes, looked at again");
    let reservation = reserve(proof).expect("the reservation re-reads that proof");
    reservation.not_sent();
    let again = reserve(observe(&r.session, Some(&r.path)).expect("after a pre-key refusal"));
    again.expect("and reserves again").not_sent();
}

#[test]
fn a_replaced_transcript_proves_its_turn_without_the_old_files_resume_offset() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = resumed_over_history(32 * 1024);
    assert!(r.ready(), "the resumed start admits its first input");
    fs::rename(&r.path, r.path.with_extension("aside")).unwrap();
    r.append(
        json!({"type":"assistant","timestamp":Utc::now() - chrono::Duration::hours(1),
        "message":{"role":"assistant","content":[{"type":"text","text":"short history"}]}}),
    );
    r.terminal();
    assert!(!r.ready(), "a smaller new file proves nothing yet");
    our_turn(&r, "its answer");
    let proof = observe(&r.session, Some(&r.path)).expect("its own turn after the new floor");
    reserve(proof).expect("and reserves").not_sent();
}
