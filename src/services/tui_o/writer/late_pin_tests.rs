#![cfg(unix)]

use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::writer::binding::{BindingCause, BindingEvidence};
use std::sync::Mutex;
use tokio::sync::watch;

const CHANNEL: u64 = 6_325_101;
const TMUX: &str = "late-pin-unit";

struct Legacy {
    cursor: Mutex<LegacyCursor>,
    frontier: Mutex<Option<u64>>,
    late_busy: Mutex<bool>,
}

impl LegacyView for Legacy {
    fn started(&self) -> bool {
        true
    }
    fn cursor(&self, _: &str) -> LegacyCursor {
        self.cursor.lock().unwrap().clone()
    }
    fn late_cursor(&self, tmux: &str) -> Result<LegacyCursor, String> {
        if *self.late_busy.lock().unwrap() {
            return Err("source authority busy".into());
        }
        Ok(self.cursor(tmux))
    }
    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        *self.frontier.lock().unwrap()
    }
    fn tail_running(&self, _: &str) -> bool {
        false
    }
}

struct Log(Vec<BindingEvent>);
impl BindingEvents for Log {
    fn binding_events_since(&self, _: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        Ok(self.0.iter().filter(|e| e.seq > after).cloned().collect())
    }
    fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
        watch::channel(1).1
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    eof: u64,
    legacy: Legacy,
    log: Log,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("current.jsonl");
        let rows = [
            serde_json::json!({"type":"user", "uuid":"late-q", "message":{"content":"question"}}),
            serde_json::json!({"type":"assistant", "uuid":"late-a", "apiBlockIndex":0,
                "message":{"id":"late", "content":[{"type":"text", "text":"unique delivered answer"}]}}),
            serde_json::json!({"type":"system", "subtype":"turn_duration", "durationMs":5}),
        ];
        let text: String = rows.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let eof = std::fs::metadata(&path).unwrap().len();
        let source = source_id_for("late-unit", &path).unwrap();
        let at = chrono::Utc::now();
        let log = Log(vec![BindingEvent {
            seq: 1,
            channel_id: CHANNEL,
            provider: ShadowProvider::Claude,
            tmux_session: TMUX.into(),
            execution_nonce: "late-unit".into(),
            record: BindingRecord::Bound {
                old: None,
                new: BindingTarget::Source(source),
                cause: BindingCause::Startup,
                parent_hint: None,
                evidence: BindingEvidence {
                    hook_event: "SessionStart".into(),
                    received_at: at,
                    reclaims: false,
                },
            },
            committed_at: at,
        }]);
        Self {
            _dir: dir,
            path,
            eof,
            log,
            legacy: Legacy {
                cursor: Mutex::new(LegacyCursor::NoPane),
                frontier: Mutex::new(Some(eof)),
                late_busy: Mutex::new(false),
            },
        }
    }
    fn pin(&self) -> Result<Snapshot, Refused> {
        pin_late(&self.legacy, &self.log.0, CHANNEL)
    }
    fn bound(&self) {
        *self.legacy.cursor.lock().unwrap() = LegacyCursor::Bound {
            path: self.path.clone(),
            offset: self.eof,
        };
    }
}

fn assert_retry(result: Result<Snapshot, Refused>, reason: &str) {
    let refused = result.expect_err(reason);
    assert!(
        !matches!(refused.hold, Hold::Final | Hold::NoSource),
        "must remain retryable: {refused}"
    );
}

#[test]
fn late_pin_no_pane_delivered_closed_eof_has_no_owed_range() {
    let f = Fixture::new();
    let snapshot = f.pin().unwrap();
    assert_eq!(snapshot.start(), f.eof);
    assert!(snapshot.owed().is_none());
    assert!(snapshot.abandoned().is_none());
    assert_eq!(
        snapshot.recheck(&f.legacy, &f.log, CHANNEL).unwrap().len(),
        1
    );
}

#[test]
fn late_pin_no_pane_missing_or_non_eof_frontier_retries() {
    for frontier in [None, Some(0), Some(u64::MAX)] {
        let f = Fixture::new();
        *f.legacy.frontier.lock().unwrap() = frontier;
        assert_retry(f.pin(), "NoPane cannot bypass missing or unequal frontier");
    }
}

#[test]
fn late_pin_no_pane_quiet_tail_without_owed_still_needs_eof_frontier() {
    let mut f = Fixture::new();
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&f.path)
        .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({"type":"last-prompt", "lastPrompt":"quiet"})
    )
    .unwrap();
    f.eof = std::fs::metadata(&f.path).unwrap().len();
    assert_retry(
        f.pin(),
        "quiet bookkeeping cannot stand in for delivered EOF",
    );
}

#[test]
fn late_pin_bound_cursor_requires_eof_frontier_and_remains_positive_when_delivered() {
    let f = Fixture::new();
    f.bound();
    assert!(f.pin().unwrap().owed().is_none());
    *f.legacy.frontier.lock().unwrap() = None;
    assert_retry(f.pin(), "missing authority must retry the late path");
}

#[test]
fn late_pin_bound_lagging_frontier_quiet_tail_and_owed_range_retry() {
    use std::io::Write;
    for quiet in [false, true] {
        let mut f = Fixture::new();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&f.path)
            .unwrap();
        let row = if quiet {
            serde_json::json!({"type":"last-prompt", "lastPrompt":"quiet"})
        } else {
            serde_json::json!({"type":"user", "uuid":"owed-q", "message":{"content":"still owed"}})
        };
        writeln!(file, "{row}").unwrap();
        f.eof = std::fs::metadata(&f.path).unwrap().len();
        f.bound();
        assert_retry(f.pin(), "Bound cursor cannot skip a lagging frontier");
        if !quiet {
            *f.legacy.frontier.lock().unwrap() = Some(f.eof);
            assert_retry(
                f.pin(),
                "a delivered frontier cannot close an open Bound turn",
            );
        }
    }
}

#[test]
fn late_pin_bound_recheck_rejects_cursor_or_frontier_change() {
    for changed_cursor in [false, true] {
        let f = Fixture::new();
        f.bound();
        let snapshot = f.pin().unwrap();
        if changed_cursor {
            *f.legacy.cursor.lock().unwrap() = LegacyCursor::Bound {
                path: f.path.clone(),
                offset: 0,
            };
        } else {
            *f.legacy.frontier.lock().unwrap() = Some(0);
        }
        assert!(snapshot.recheck(&f.legacy, &f.log, CHANNEL).is_err());
    }
}

#[test]
fn late_pin_unsafe_named_source_remains_final() {
    let mut f = Fixture::new();
    let named = f._dir.path().join("unbound-parent.jsonl");
    std::fs::write(&named, b"unowned bytes\n").unwrap();
    let BindingRecord::Bound { parent_hint, .. } = &mut f.log.0[0].record else {
        panic!("fixture bound")
    };
    *parent_hint = Some(source_id_for("named", &named).unwrap());
    assert_eq!(f.pin().unwrap_err().hold, Hold::Final);
}

#[test]
fn late_pin_source_lookup_busy_retries_before_pin() {
    let f = Fixture::new();
    *f.legacy.late_busy.lock().unwrap() = true;
    assert_retry(f.pin(), "busy source authority is not a NoPane observation");
}

#[test]
fn late_pin_recheck_rejects_frontier_reset_and_pane_reappearance() {
    for pane_reappears in [false, true] {
        let f = Fixture::new();
        let snapshot = f.pin().unwrap();
        if pane_reappears {
            f.bound();
        } else {
            *f.legacy.frontier.lock().unwrap() = Some(0);
        }
        assert!(
            snapshot.recheck(&f.legacy, &f.log, CHANNEL).is_err(),
            "a changed late source must repin"
        );
    }
}

#[test]
fn late_pin_recheck_busy_source_is_retry_and_does_not_accept_cached_no_pane() {
    let f = Fixture::new();
    let snapshot = f.pin().unwrap();
    *f.legacy.late_busy.lock().unwrap() = true;
    assert!(snapshot.recheck(&f.legacy, &f.log, CHANNEL).is_err());
}

#[test]
fn late_pin_file_growth_or_open_turn_cannot_be_committed() {
    use std::io::Write;
    let f = Fixture::new();
    let snapshot = f.pin().unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&f.path)
        .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({"type":"user", "uuid":"open-q", "message":{"content":"still running"}})
    )
    .unwrap();
    assert!(snapshot.recheck(&f.legacy, &f.log, CHANNEL).is_err());
    *f.legacy.frontier.lock().unwrap() = Some(std::fs::metadata(&f.path).unwrap().len());
    assert_retry(f.pin(), "frontier equality does not close an open turn");
}
