use super::*;
use crate::services::claude_tui::composer_lock::draft_guarded;
use crate::services::claude_tui::host_input::{SpyGuard, SpyState};
use crate::services::tui_input::human_frame::{history, idle};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const MEASURED: &str =
    include_str!("../../../../tests/fixtures/tui_input/claude-2.1.309-folded-paste.ansi");

fn capture(frame: &str, effort: bool) -> String {
    let shown = MEASURED
        .replacen(
            "❯\u{a0}[Pasted text #1 +14 lines]",
            &format!("❯\u{a0}{}", frame.replace('\n', "\n  ")),
            1,
        )
        .replace(
            "paste again to expand",
            "⏵⏵ bypass permissions on (shift+tab to cycle)",
        );
    if effort {
        shown
    } else {
        format!(
            "\n{}\n",
            shown.lines().skip(1).collect::<Vec<_>>().join("\n")
        )
    }
}

fn spy_for(pane: &str, custody: Rc<RefCell<String>>) -> SpyGuard {
    SpyGuard::install(SpyState {
        captures: std::iter::repeat_n(Some(pane.to_string()), 64).collect(),
        on_send: Some(Box::new(move |_| custody.borrow_mut().clear())),
        ..SpyState::default()
    })
}

fn register(session: &str, id: &str, transcript: &std::path::Path, frame: &str) {
    std::fs::write(transcript, "").unwrap();
    history::register(("claude", session, id, "7"), transcript, 0, frame).unwrap();
    assert!(history::screen_pending(session, frame));
}

#[test]
fn pending_plain_frame_is_not_cleanup_owned_with_or_without_effort_status() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for effort in [false, true] {
        for suffix in ["", "\n  a person appended this — 한글"] {
            let dir = tempfile::tempdir().unwrap();
            let session = format!("pending-cleanup-{}", uuid::Uuid::new_v4().simple());
            let id = uuid::Uuid::new_v4().simple().to_string();
            let frame = idle("ann", "7", "please inspect\nthe next line");
            register(&session, &id, &dir.path().join("native.jsonl"), &frame);
            let pane = capture(&format!("{frame}{suffix}"), effort);
            let custody = Rc::new(RefCell::new(pane.clone()));
            let spy = spy_for(&pane, custody.clone());
            assert!(history::screen_pending(
                &session,
                &format!("{frame}{suffix}")
            ));
            assert!(
                stranded_draft_is_ours(&session, PromptReadinessKind::Followup).is_err(),
                "pending frame is not cleanup-owned: effort={effort}, suffix={suffix:?}"
            );
            assert_eq!(spy.0.borrow().sends, 0);
            assert_eq!(*custody.borrow(), pane);
        }
    }
}

#[test]
fn pending_plain_frame_blocks_the_real_timeout_cleanup_without_prior_holds() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for effort in [false, true] {
        for suffix in ["", "\n  a person appended this — 한글"] {
            let dir = tempfile::tempdir().unwrap();
            let session = format!("pending-clear-{}", uuid::Uuid::new_v4().simple());
            let id = uuid::Uuid::new_v4().simple().to_string();
            let frame = idle("ann", "7", "please inspect\nthe next line");
            register(&session, &id, &dir.path().join("native.jsonl"), &frame);
            let pane = capture(&format!("{frame}{suffix}"), effort);
            let custody = Rc::new(RefCell::new(pane.clone()));
            let spy = spy_for(&pane, custody.clone());
            assert!(
                !draft_guarded(&session),
                "a prior draft guard must not hide this path"
            );
            let entered = Cell::new(false);
            let result = super::super::with_composer_cleanup_lock(&session, || {
                entered.set(true);
                match stranded_draft_is_ours(&session, PromptReadinessKind::Followup) {
                    Ok(true) => {
                        super::clear_prompt_draft(&session);
                        Ok(())
                    }
                    Ok(false) => Ok(()),
                    Err(error) => Err(error),
                }
            });
            assert!(
                entered.get(),
                "the actual cleanup lock consumer must execute"
            );
            assert!(
                matches!(result, Some(Err(_))),
                "pending cleanup must refuse inside the lock: effort={effort}, suffix={suffix:?}"
            );
            let writes: Vec<_> = spy
                .calls()
                .into_iter()
                .filter(|call| {
                    ["keys:", "literal:", "load:", "paste:", "retire:"]
                        .iter()
                        .any(|prefix| call.starts_with(prefix))
                })
                .collect();
            assert!(writes.is_empty(), "pending cleanup sent {writes:?}");
            assert_eq!(spy.0.borrow().sends, 0);
            assert_eq!(*custody.borrow(), pane);
            assert_eq!(host_input::observe_draft(&session), Some(pane));
        }
    }
}

#[test]
fn receipted_plain_frame_keeps_the_existing_cleanup_owned_meaning() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for effort in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let session = format!("receipted-clear-{}", uuid::Uuid::new_v4().simple());
        let id = uuid::Uuid::new_v4().simple().to_string();
        let transcript = dir.path().join("native.jsonl");
        let frame = idle("ann", "7", "please inspect\nthe next line");
        register(&session, &id, &transcript, &frame);
        let record =
            serde_json::json!({"type":"user", "uuid":"accepted-row", "message":{"content":frame}});
        std::fs::write(&transcript, format!("{record}\n")).unwrap();
        assert_eq!(
            history::receipt(&id, &transcript, 0, "user", Some("accepted-row"), &frame),
            Some(true)
        );
        assert!(!history::screen_pending(&session, &frame));
        let pane = capture(&frame, effort);
        let _spy = spy_for(&pane, Rc::new(RefCell::new(pane.clone())));
        assert_eq!(
            stranded_draft_is_ours(&session, PromptReadinessKind::Followup),
            Ok(true)
        );
    }
}
