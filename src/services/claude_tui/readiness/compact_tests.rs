use super::*;
use crate::services::claude_tui::input::submission::tests::{ScriptedTmux, view, wait};
use crate::services::claude_tui::readiness::compact::{Outcome, Pending};

fn poll(pending: &mut Pending, session: &str) -> Outcome {
    crate::services::claude_tui::composer_lock::with_composer_permit(session, |permit| {
        pending.poll(permit)
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
    assert_eq!(poll(&mut pending, &r.session), Outcome::Obsolete);
    assert_eq!(tmux.sends(), ["/compact", "Enter"]);
    // Synthetic native completion; the operational CLI's compact ordering remains unmeasured.
    r.append(json!({"type":"user","message":{"content":"/compact"}}));
    r.assistant("compacted");
    r.terminal();
    tmux.show(&view("", false));
    assert_eq!(r.send(), Ok(()));
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
    claude_compact_trigger::clear_for_tmux(&r.session);
}
