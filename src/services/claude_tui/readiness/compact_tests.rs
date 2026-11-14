use super::*;
use crate::services::claude_tui::input::submission::tests::{ScriptedTmux, view, wait};
use crate::services::claude_tui::readiness::compact::{Outcome, Pending};

fn poll(pending: &mut Pending, session: &str) -> Outcome {
    crate::services::claude_tui::composer_lock::with_composer_permit(session, |permit| {
        pending.poll(permit, |publish| publish())
    })
}

fn pane() -> ScriptedTmux {
    let tmux = ScriptedTmux::new(&view("", false));
    tmux.set_dead(false);
    fs::write(tmux.directory().join("control"), view("/compact", false)).unwrap();
    fs::write(
        tmux.directory().join("on_key"),
        format!(
            "if [ \"$1\" = /compact ]; then cp '{}' '{}'; fi\n",
            tmux.directory().join("control").display(),
            tmux.directory().join("screen").display()
        ),
    )
    .unwrap();
    tmux
}

#[test]
fn compact_waits_for_the_running_turn_before_any_control_key() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("still running");
    let tmux = pane();
    let mut pending = Pending::capture(&r.session).unwrap();
    let outcome = poll(&mut pending, &r.session);
    assert!(
        tmux.sends().is_empty(),
        "Running must leave compact pending; got {:?}",
        tmux.sends()
    );
    assert_eq!(outcome, Outcome::Pending);
    r.terminal();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Submitted);
    assert_eq!(tmux.sends(), ["/compact", "Enter"]);
    let _ = poll(&mut pending, &r.session);
    assert_eq!(tmux.sends(), ["/compact", "Enter"]);
    // Bounded actual caller admission observes no payload before completion.
    let previous = crate::services::claude_tui::input::FOLLOWUP_TIMEOUT_FOR_TESTS
        .with(|slot| slot.replace(Some(Duration::from_millis(25))));
    assert!(r.send().is_err());
    crate::services::claude_tui::input::FOLLOWUP_TIMEOUT_FOR_TESTS.with(|slot| slot.set(previous));
    assert_eq!(tmux.sends(), ["/compact", "Enter"]);
    r.hook("compact");
    note_hook(
        "claude",
        &HookEventKind::PostCompact,
        &r.session,
        &json!({"transcript_path":r.path,"session_id":r.ctx.expected_native_session_id}),
        Some(&r.ctx),
        Some(Utc::now()),
        false,
    );
    assert_eq!(poll(&mut pending, &r.session), Outcome::Pending);
    assert!(!r.ready());
    // The measured local command emits no assistant or native terminal after compact.
    compact_records(&r, false);
    let completion = poll(&mut pending, &r.session);
    let duplicate = poll(&mut pending, &r.session);
    tmux.show(&view("", false));
    let previous = crate::services::claude_tui::input::FOLLOWUP_TIMEOUT_FOR_TESTS
        .with(|slot| slot.replace(Some(Duration::from_millis(100))));
    let next = r.send();
    crate::services::claude_tui::input::FOLLOWUP_TIMEOUT_FOR_TESTS.with(|slot| slot.set(previous));
    assert_eq!(next, Ok(()));
    assert_eq!(completion, Outcome::Completed);
    assert_eq!(duplicate, Outcome::Obsolete);
    assert_eq!(
        tmux.sends(),
        ["/compact", "Enter", "load", "paste", "Enter"]
    );
}

#[test]
fn compact_pending_recovers_from_unread_person_draft_and_focus_without_a_switch() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("done");
    r.terminal();
    let tmux = pane();
    let mut pending = Pending::capture(&r.session).unwrap();
    fs::write(tmux.directory().join("capture_fail"), "").unwrap();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Pending);
    fs::remove_file(tmux.directory().join("capture_fail")).unwrap();
    for capture in [
        view("person's draft", false),
        view("[User: bot] owned draft", false),
        format!(
            "Allow this tool?\nAllow  Deny\nEnter to confirm\n{}",
            view("", false)
        ),
    ] {
        tmux.show(&capture);
        assert_eq!(poll(&mut pending, &r.session), Outcome::Pending);
        assert!(tmux.sends().is_empty());
    }
    let narrow = format!(
        "⚠ 1 MCP server(s) need authentication\n{}\n❯\u{a0}\n{}\n  ⏵⏵ bypass permissions on\n",
        "─".repeat(46),
        "─".repeat(46)
    );
    tmux.show(&narrow);
    assert_eq!(poll(&mut pending, &r.session), Outcome::Submitted);
    assert_eq!(tmux.sends(), ["/compact", "Enter"]);
}

#[test]
fn compact_fresh_start_yields_to_first_input_and_waits_for_its_terminal() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    let tmux = pane();
    let mut pending = Pending::capture(&r.session).unwrap();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Pending);
    assert!(tmux.sends().is_empty());
    assert_eq!(r.send(), Ok(()));
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
    r.assistant("first answer");
    r.terminal();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Submitted);
    assert_eq!(
        tmux.sends(),
        ["load", "paste", "Enter", "/compact", "Enter"]
    );
}

// A resume start admits the first input over old history; that history's terminal is not this
// run's, so a compact still waits for the terminal of a turn this run finished.
#[test]
fn compact_resumed_start_yields_to_first_input_and_waits_for_its_terminal() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.append(
        json!({"type":"assistant","timestamp":Utc::now() - chrono::Duration::hours(1),
        "message":{"role":"assistant","content":[{"type":"text","text":"earlier answer"}]}}),
    );
    r.terminal();
    r.append(json!({"type":"cost-state"}));
    r.hook("resume");
    let tmux = pane();
    let mut pending = Pending::capture(&r.session).unwrap();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Pending);
    assert!(tmux.sends().is_empty());
    assert_eq!(r.send(), Ok(()));
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
    r.assistant("first answer");
    r.terminal();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Submitted);
    assert_eq!(
        tmux.sends(),
        ["load", "paste", "Enter", "/compact", "Enter"]
    );
}

#[test]
fn compact_yields_to_the_shared_first_input_reservation() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("done");
    r.terminal();
    let tmux = pane();
    let mut pending = Pending::capture(&r.session).unwrap();
    let front = reserve(observe(&r.session, None).unwrap()).unwrap();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Pending);
    assert!(tmux.sends().is_empty());
    front.not_sent();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Submitted);
    assert_eq!(tmux.sends(), ["/compact", "Enter"]);
}

#[test]
fn compact_unknown_literal_or_changed_focus_never_replays_or_sends_enter() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for mode in ["literal-error", "modal", "person-text"] {
        let r = Rig::new();
        r.assistant("done");
        r.terminal();
        let tmux = pane();
        let mut pending = Pending::capture(&r.session).unwrap();
        match mode {
            "literal-error" => tmux.fail_send(0),
            "modal" => fs::write(
                tmux.directory().join("control"),
                format!(
                    "Allow this tool?\nAllow  Deny\nEnter to confirm\n{}",
                    view("/compact", false)
                ),
            )
            .unwrap(),
            _ => fs::write(
                tmux.directory().join("control"),
                view("/compact person's text", false),
            )
            .unwrap(),
        }
        assert_eq!(poll(&mut pending, &r.session), Outcome::Ambiguous, "{mode}");
        assert_eq!(tmux.sends(), ["/compact"]);
        tmux.show(&view("", false));
        assert_eq!(poll(&mut pending, &r.session), Outcome::Obsolete);
        assert_eq!(tmux.sends(), ["/compact"]);
    }
}

#[test]
fn compact_pending_is_obsolete_after_clear_or_inode_replacement() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for replacement in [false, true] {
        let r = Rig::new();
        r.assistant("done");
        r.terminal();
        let tmux = pane();
        let mut pending = Pending::capture(&r.session).unwrap();
        if replacement {
            let next = r.path.with_extension("replacement");
            fs::write(&next, fs::read(&r.path).unwrap()).unwrap();
            fs::rename(next, &r.path).unwrap();
        } else {
            r.hook("clear");
        }
        assert_eq!(poll(&mut pending, &r.session), Outcome::Obsolete);
        assert!(tmux.sends().is_empty());
    }
}

#[test]
fn compact_ignores_suppressed_summary_timer_and_late_assistant_until_final() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("needs continuation");
    let tmux = pane();
    let mut pending = Pending::capture(&r.session).unwrap();
    note_hook(
        "claude",
        &HookEventKind::Stop,
        &r.session,
        &json!({"transcript_path":r.path,"session_id":r.ctx.expected_native_session_id}),
        Some(&r.ctx),
        Some(Utc::now()),
        true,
    );
    r.append(json!({"type":"system","subtype":"stop_hook_summary"}));
    r.terminal();
    for _ in 0..2 {
        assert_eq!(poll(&mut pending, &r.session), Outcome::Pending);
        assert!(tmux.sends().is_empty());
    }
    r.assistant("late assistant");
    assert_eq!(poll(&mut pending, &r.session), Outcome::Pending);
    assert!(tmux.sends().is_empty());
    r.terminal();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Submitted);
    assert_eq!(tmux.sends(), ["/compact", "Enter"]);
}

#[test]
fn compact_usage_caller_keeps_one_pending_across_busy_completion_and_duplicate_usage() {
    usage_completion(false);
}

#[test]
fn compact_old_generation_cannot_release_a_recreated_cycle() {
    usage_completion(true);
}

fn usage_completion(recreated: bool) {
    use crate::services::{
        claude_compact_trigger, discord::ManagedCompactTurnIdentity, provider::ProviderKind,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _compact = claude_compact_trigger::STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let r = Rig::new();
    r.assistant("busy");
    let tmux = pane();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _enter = runtime.enter();
    let channel = 808099;
    let identity = ManagedCompactTurnIdentity::test_fixture(channel, &r.session);
    let request = || {
        claude_compact_trigger::maybe_inject_compact(
            identity.clone(),
            &ProviderKind::Claude,
            600_000,
            Some(1_000_000),
            50,
            300_000,
        )
    };
    request();
    request();
    wait("compact checked busy at least twice", || {
        fs::read_to_string(tmux.directory().join("argv"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("capture-pane"))
            .count()
            >= 2
    });
    assert!(tmux.sends().is_empty());
    r.terminal();
    wait("one compact Enter", || {
        tmux.sends().iter().filter(|key| *key == "Enter").count() == 1
    });
    request();
    assert_eq!(tmux.sends(), ["/compact", "Enter"]);
    assert!(!r.ready());
    if recreated {
        claude_compact_trigger::clear_for_tmux(&r.session);
        let before = fs::read_to_string(tmux.directory().join("argv"))
            .unwrap()
            .matches("capture-pane")
            .count();
        tmux.show(&view("", false));
        request();
        compact_records(&r, false);
        wait("new generation polled without adopting old compact", || {
            fs::read_to_string(tmux.directory().join("argv"))
                .unwrap_or_default()
                .matches("capture-pane")
                .count()
                > before
        });
        assert!(!r.ready());
        assert_eq!(tmux.sends(), ["/compact", "Enter"]);
    } else {
        claude_compact_trigger::maybe_inject_compact(
            identity.clone(),
            &ProviderKind::Claude,
            1,
            Some(1_000_000),
            50,
            300_000,
        );
        compact_records(&r, false);
        wait("submitted generation completes after usage re-arm", || {
            r.ready()
        });
        tmux.show(&view("", false));
        assert_eq!(r.send(), Ok(()));
        assert_eq!(
            tmux.sends(),
            ["/compact", "Enter", "load", "paste", "Enter"]
        );
    }
    claude_compact_trigger::clear_for_tmux(&r.session);
}

// Redacted record kinds/order from the coordinator's isolated 2.1.322 capture, rows 46..64.
fn compact_records(r: &Rig, failed: bool) {
    if failed {
        r.append(
            json!({"type":"user","message":{"content":"<command-name>/compact</command-name>"}}),
        );
        r.append(json!({"type":"user","message":{"content":"<local-command-stderr>Error: unable to compact</local-command-stderr>"}}));
    } else {
        for line in
            include_str!("../../../../tests/fixtures/claude_compact/local-command-2.1.322.jsonl")
                .lines()
        {
            r.append(serde_json::from_str(line).unwrap());
        }
    }
}

#[test]
fn compact_failure_output_releases_only_the_submitted_control_without_claiming_success() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("done");
    r.terminal();
    let tmux = pane();
    let mut pending = Pending::capture(&r.session).unwrap();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Submitted);
    tmux.show(&view("", false));
    assert!(!r.ready());
    let floor = evidence::Stamp::read(&r.path).unwrap();
    compact_records(&r, true);
    assert_eq!(
        compact::completion::read(
            &r.path,
            &floor,
            r.ctx.expected_native_session_id.as_deref().unwrap(),
            true
        ),
        Some(false)
    );
    assert_eq!(poll(&mut pending, &r.session), Outcome::Completed);
    assert_eq!(r.send(), Ok(()));
    assert_eq!(
        tmux.sends(),
        ["/compact", "Enter", "load", "paste", "Enter"]
    );
}

#[test]
fn compact_completion_does_not_release_unknown_foreign_source_or_newer_reservation() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for mode in [
        "unknown-enter",
        "unknown-literal",
        "clear",
        "inode",
        "native",
        "nonce",
        "other-command",
        "new-reservation",
        "new-recovery",
    ] {
        let r = Rig::new();
        r.assistant("done");
        r.terminal();
        let tmux = pane();
        let mut pending = Pending::capture(&r.session).unwrap();
        if mode == "unknown-enter" {
            tmux.fail_send(1);
        }
        if mode == "unknown-literal" {
            tmux.fail_send(0);
        }
        let first = poll(&mut pending, &r.session);
        assert_eq!(
            first,
            if mode.starts_with("unknown") {
                Outcome::Ambiguous
            } else {
                Outcome::Submitted
            }
        );
        let keys = tmux.sends();
        match mode {
            "clear" => r.hook("clear"),
            "inode" => {
                let replacement = r.path.with_extension("replacement");
                fs::write(&replacement, fs::read(&r.path).unwrap()).unwrap();
                fs::rename(replacement, &r.path).unwrap();
            }
            "nonce" => fs::write(
                crate::services::tmux_common::session_temp_path(&r.session, "spawn_nonce"),
                "different",
            )
            .unwrap(),
            "new-recovery" => {
                let source = source::Source::current(&r.session, None).unwrap();
                ledger().entries.get_mut(&source).unwrap().recovery = Some((1000, Utc::now()));
            }
            "new-reservation" => {
                let source = source::Source::current(&r.session, None).unwrap();
                let mut state = ledger();
                let entry = state.entries.get_mut(&source).unwrap();
                entry.pending = entry.pending.map(|(floor, token)| (floor, token + 100));
            }
            _ => {}
        }
        if mode == "other-command" {
            r.append(json!({"type":"user","message":{"content":"<command-name>/compact</command-name>"}}));
        }
        compact_records(&r, false);
        if mode == "native" {
            r.append(json!({"type":"attachment","sessionId":"another-native"}));
        }
        if mode == "other-command" {
            r.append(json!({"type":"user","message":{"content":"<command-name>/compact</command-name>"}}));
        }
        let _ = poll(&mut pending, &r.session);
        tmux.show(&view("", false));
        assert!(!r.ready(), "{mode}");
        assert_eq!(tmux.sends(), keys, "{mode}");
    }
}

#[test]
fn compact_completion_preserves_root_continuation_and_rejects_unowned_commands() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for owned in [false, true] {
        let r = Rig::new();
        r.assistant("done");
        r.terminal();
        let tmux = pane();
        let mut pending = Pending::capture(&r.session).unwrap();
        if owned {
            assert_eq!(poll(&mut pending, &r.session), Outcome::Submitted);
            note_hook(
                "claude",
                &HookEventKind::Stop,
                &r.session,
                &json!({"transcript_path":r.path,"session_id":r.ctx.expected_native_session_id}),
                Some(&r.ctx),
                Some(Utc::now()),
                true,
            );
        }
        compact_records(&r, false);
        r.hook("compact");
        note_hook(
            "claude",
            &HookEventKind::PostCompact,
            &r.session,
            &json!({"transcript_path":r.path,"session_id":r.ctx.expected_native_session_id}),
            Some(&r.ctx),
            Some(Utc::now()),
            false,
        );
        if owned {
            assert_eq!(poll(&mut pending, &r.session), Outcome::Completed);
        }
        tmux.show(&view("", false));
        assert!(!r.ready());
        assert_eq!(tmux.sends().len(), if owned { 2 } else { 0 });
    }
}

#[test]
fn compact_completion_ignores_replayed_old_output_and_incomplete_output() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    compact_records(&r, false);
    r.assistant("done");
    r.terminal();
    let tmux = pane();
    let mut pending = Pending::capture(&r.session).unwrap();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Submitted);
    assert_eq!(poll(&mut pending, &r.session), Outcome::Pending);
    r.append(json!({"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"manual"}}));
    r.append(json!({"type":"user","message":{"content":"<command-name>/compact</command-name>"}}));
    assert_eq!(poll(&mut pending, &r.session), Outcome::Pending);
    assert!(!r.ready());
    r.append(json!({"type":"user","message":{"content":"<local-command-stdout>Compacted (summary)</local-command-stdout>"}}));
    assert_eq!(poll(&mut pending, &r.session), Outcome::Completed);
    let unmutated = reserve(observe(&r.session, None).unwrap()).unwrap();
    assert!(!r.ready());
    unmutated.not_sent();
    assert!(r.ready());
    r.append(json!({"type":"attachment"}));
    r.append(json!({"type":"permission-mode"}));
    tmux.show(&view("", false));
    assert_eq!(r.send(), Ok(()));
    assert_eq!(
        tmux.sends(),
        ["/compact", "Enter", "load", "paste", "Enter"]
    );
    assert!(!r.ready());
}

#[test]
fn compact_stdout_error_is_completion_without_success_and_new_submit_keeps_its_own_fence() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("done");
    r.terminal();
    let tmux = pane();
    let mut pending = Pending::capture(&r.session).unwrap();
    let floor = evidence::Stamp::read(&r.path).unwrap();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Submitted);
    r.append(json!({"type":"user","message":{"content":"<command-name>/compact</command-name>"}}));
    r.append(json!({"type":"user","message":{"content":"<local-command-stdout>Error: not enough messages to compact</local-command-stdout>"}}));
    assert_eq!(
        compact::completion::read(
            &r.path,
            &floor,
            r.ctx.expected_native_session_id.as_deref().unwrap(),
            true
        ),
        Some(false)
    );
    assert_eq!(poll(&mut pending, &r.session), Outcome::Completed);
    tmux.show(&view("", false));
    assert_eq!(r.send(), Ok(()));
    assert!(!r.ready());
    assert_eq!(
        tmux.sends(),
        ["/compact", "Enter", "load", "paste", "Enter"]
    );
}

#[test]
fn compact_receipt_does_not_survive_a_new_start_on_the_same_file() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("done");
    r.terminal();
    let tmux = pane();
    let mut pending = Pending::capture(&r.session).unwrap();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Submitted);
    compact_records(&r, false);
    assert_eq!(poll(&mut pending, &r.session), Outcome::Completed);
    assert!(r.ready());
    r.hook("clear");
    assert!(!r.ready());
    assert_eq!(tmux.sends(), ["/compact", "Enter"]);
    r.assistant("new current turn");
    r.terminal();
    tmux.show(&view("", false));
    assert_eq!(r.send(), Ok(()));
    assert_eq!(
        tmux.sends(),
        ["/compact", "Enter", "load", "paste", "Enter"]
    );
}

#[test]
fn compact_completion_retains_the_recovery_floor_already_proven_by_its_reservation() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    let source = source::Source::current(&r.session, None).unwrap();
    let recovery = Some((0, Utc::now()));
    {
        let mut state = ledger();
        let entry = state.entries.get_mut(&source).unwrap();
        entry.start = false;
        entry.recovery = recovery;
    }
    r.assistant("new recovery activity");
    r.terminal();
    assert!(r.ready());
    let tmux = pane();
    let mut pending = Pending::capture(&r.session).unwrap();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Submitted);
    compact_records(&r, false);
    assert_eq!(poll(&mut pending, &r.session), Outcome::Completed);
    assert_eq!(ledger().entries.get(&source).unwrap().recovery, recovery);
    tmux.show(&view("", false));
    assert_eq!(r.send(), Ok(()));
    assert_eq!(
        tmux.sends(),
        ["/compact", "Enter", "load", "paste", "Enter"]
    );
}

#[test]
fn compact_completed_prefix_cannot_hide_a_new_native_turn_or_require_its_submit_hook() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    r.assistant("done");
    r.terminal();
    let tmux = pane();
    let mut pending = Pending::capture(&r.session).unwrap();
    assert_eq!(poll(&mut pending, &r.session), Outcome::Submitted);
    compact_records(&r, false);
    r.append(json!({"type":"user","message":{"content":"a new busy original"}}));
    r.assistant("new turn still running");
    assert_eq!(poll(&mut pending, &r.session), Outcome::Completed);
    assert!(!r.ready());
    assert_eq!(tmux.sends(), ["/compact", "Enter"]);
    r.terminal();
    tmux.show(&view("", false));
    assert_eq!(r.send(), Ok(()));
    assert_eq!(
        tmux.sends(),
        ["/compact", "Enter", "load", "paste", "Enter"]
    );
}
