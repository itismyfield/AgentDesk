use super::*;
use crate::config::{TestEnvVarGuard, TestRuntimeRootGuard};
use crate::services::claude_tui::host_input::{InputRefusal, StopCause};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::store::spool::{Cursor, source_key};

/// An earlier execution whose provider runs while `alive`; each `/exit` attempt is counted.
#[derive(Default)]
struct Earlier {
    alive: bool,
    unknown: bool,
    answer: Option<ExitSend>,
    exits_on_send: bool,
    sends: usize,
}

impl EarlierExecution for Earlier {
    fn liveness(&mut self) -> Liveness {
        match (self.unknown, self.alive) {
            (true, _) => Liveness::Unknown("probe failed".into()),
            (false, true) => Liveness::Running,
            (false, false) => Liveness::Ended,
        }
    }

    fn send_exit(&mut self) -> ExitSend {
        self.sends += 1;
        let answer = self.answer.clone().unwrap_or(ExitSend::Sent);
        if answer == ExitSend::Sent && self.exits_on_send {
            self.alive = false;
        }
        answer
    }
}

const WAIT: Duration = Duration::from_millis(60);

#[test]
fn an_earlier_execution_already_gone_is_never_typed_to() {
    let mut earlier = Earlier::default();
    assert_eq!(end_by_exit(&mut earlier, WAIT), Ok(Ended::Already));
    assert_eq!(earlier.sends, 0);
}

#[test]
fn a_running_earlier_execution_ends_by_one_exit() {
    let mut earlier = Earlier {
        alive: true,
        exits_on_send: true,
        ..Earlier::default()
    };
    assert_eq!(end_by_exit(&mut earlier, WAIT), Ok(Ended::ByExit));
    assert_eq!(earlier.sends, 1);
}

#[test]
fn a_draft_busy_or_unclear_composer_keeps_the_earlier_execution_and_stops() {
    for (answer, says) in [
        (ExitSend::Untouched("draft".into()), "kept running"),
        (ExitSend::Unclear("lost reply".into()), "exit unclear"),
    ] {
        let mut earlier = Earlier {
            alive: true,
            exits_on_send: true,
            answer: Some(answer),
            ..Earlier::default()
        };
        let error = end_by_exit(&mut earlier, WAIT).unwrap_err();
        assert!(error.contains(says), "{error}");
        assert_eq!((earlier.sends, earlier.alive), (1, true));
    }
}

#[test]
fn an_earlier_execution_that_outlives_its_exit_stops_the_turn_without_another_exit() {
    let mut earlier = Earlier {
        alive: true,
        ..Earlier::default()
    };
    let error = end_by_exit(&mut earlier, WAIT).unwrap_err();
    assert!(error.contains("did not exit"), "{error}");
    assert_eq!(earlier.sends, 1);
}

#[test]
fn an_unobserved_earlier_execution_is_never_typed_to() {
    let mut earlier = Earlier {
        alive: true,
        unknown: true,
        ..Earlier::default()
    };
    let error = end_by_exit(&mut earlier, WAIT).unwrap_err();
    assert!(error.contains("unobserved"), "{error}");
    assert_eq!(earlier.sends, 0);
}

#[test]
fn only_an_applied_run_is_a_sent_exit() {
    assert_eq!(exit_sent(InputRun::Applied), ExitSend::Sent);
    let refused = exit_sent(InputRun::Refused(InputRefusal::Unknown));
    assert!(matches!(refused, ExitSend::Untouched(_)));
    let cancelled = exit_sent(InputRun::Cancelled { confirmed: 0 });
    assert!(matches!(cancelled, ExitSend::Untouched(_)));
    for run in [
        InputRun::Cancelled { confirmed: 1 },
        InputRun::Indeterminate {
            confirmed: 1,
            cause: StopCause::Send("lost".into()),
        },
    ] {
        assert!(matches!(exit_sent(run), ExitSend::Unclear(_)));
    }
    assert_eq!(
        exit_plan(),
        [
            TuiInputAction::Literal("/exit".into()),
            TuiInputAction::Enter
        ]
    );
}

#[test]
fn a_resume_transcript_is_absent_only_when_certainly_gone() {
    let home = tempfile::tempdir().unwrap();
    let _home = TestEnvVarGuard::set_path("CLAUDE_CONFIG_DIR", home.path());
    let cwd = tempfile::tempdir().unwrap();
    let id = "11111111-1111-4111-8111-111111111111";
    assert_eq!(transcript_file(cwd.path(), id), Ok(None));
    let bad = transcript_file(cwd.path(), "not-a-session").unwrap_err();
    assert!(bad.contains("session_id"), "{bad}");
    let path =
        crate::services::claude_tui::transcript_tail::claude_transcript_path(cwd.path(), id, None)
            .unwrap();
    let dir = path.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&path, "{}\n").unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let source = SourceId {
        session_id: id.into(),
        path: path.clone(),
        dev: meta.dev(),
        ino: meta.ino(),
    };
    assert_eq!(transcript_file(cwd.path(), id), Ok(Some(source)));
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let unreadable = transcript_file(cwd.path(), id);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(unreadable.is_err(), "{unreadable:?}");
}

#[test]
fn o_reads_only_a_cursor_its_own_bind_would_continue() {
    let _root = TestRuntimeRootGuard::new();
    let channel = 7_503;
    let id = "11111111-1111-4111-8111-111111111111";
    let source = |session: &str, dev: u64, ino: u64| SourceId {
        session_id: session.into(),
        path: Path::new("/tmp/claude/resumed.jsonl").to_path_buf(),
        dev,
        ino,
    };
    let resumed = source(id, 1, 2);
    assert_eq!(o_reads(channel, &resumed), Ok(false), "no store");
    let root = crate::config::runtime_root().unwrap();
    let cursors = root
        .join("o_store")
        .join(channel.to_string())
        .join("cursor");
    std::fs::create_dir_all(&cursors).unwrap();
    assert_eq!(o_reads(channel, &resumed), Ok(false), "no cursor");
    let store = |stored: &SourceId| {
        let cursor = Cursor {
            source: stored.clone(),
            captured_through: 10,
            prefix_hash: "00".into(),
            retired: false,
            transition_cleanup: None,
        };
        let file = cursors.join(format!("{}.json", source_key(stored)));
        std::fs::write(&file, serde_json::to_vec(&cursor).unwrap()).unwrap();
        file
    };
    let file = store(&resumed);
    assert_eq!(o_reads(channel, &resumed), Ok(true));
    assert_eq!(
        o_reads(channel, &source(id, 9, 2)),
        Ok(true),
        "a renumbered dev"
    );
    assert_eq!(
        o_reads(channel, &source(id, 1, 3)),
        Ok(false),
        "another file"
    );
    assert_eq!(o_reads(channel + 1, &resumed), Ok(false), "another channel");
    std::fs::write(&file, "{").unwrap();
    assert!(o_reads(channel, &resumed).is_err(), "torn cursor");
    std::fs::remove_file(&file).unwrap();
    let empty = store(&source("", 1, 2));
    assert_eq!(
        o_reads(channel, &resumed),
        Ok(true),
        "stored with an empty session"
    );
    let other_empty = store(&source("", 5, 2));
    assert!(o_reads(channel, &resumed).is_err(), "two candidates");
    std::fs::remove_file(&empty).unwrap();
    std::fs::remove_file(&other_empty).unwrap();
    store(&source(id, 1, 2));
    store(&source(id, 5, 2));
    assert!(
        o_reads(channel, &resumed).is_err(),
        "two stored variants of one file"
    );
}

#[test]
fn only_the_own_resumed_start_of_an_execution_counts() {
    use crate::services::claude_tui::hook_server::{HookEvent, HookEventKind};
    use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
    let _root = TestRuntimeRootGuard::new();
    let id = "11111111-1111-4111-8111-111111111111";
    let prepared =
        PreparedIncarnation::prepare_at("claude", "resume-start", Some(1), Some(id), true, None)
            .unwrap();
    let nonce = prepared.context.execution_nonce.clone();
    let event = |source: &str, context: Option<_>| HookEvent {
        provider: "claude".into(),
        session_id: id.into(),
        kind: HookEventKind::SessionStart,
        payload: serde_json::json!({"session_id": id, "source": source}),
        original_context: context,
        ..HookEvent::default()
    };
    let own = Some(prepared.context.clone());
    assert!(resumed_start(&event("resume", own.clone()), id, &nonce));
    assert!(!resumed_start(&event("startup", own.clone()), id, &nonce));
    assert!(!resumed_start(&event("resume", None), id, &nonce));
    assert!(!resumed_start(&event("resume", own), id, "another"));
}
