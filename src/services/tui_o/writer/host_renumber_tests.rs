//! First adoption through the real host after a reboot renumbered a logged transcript's dev.

use super::*;
use crate::services::tui_o::shadow::capture::{renumber, same_file};

const REBOOT: u64 = 1 << 40;

fn line(seq: u64, old: Option<&SourceId>, new: &SourceId, cause: p5::BindingCause) -> Vec<u8> {
    let event = p5::BindingEvent {
        seq,
        channel_id: CHANNEL,
        provider: "claude".into(),
        tmux_session: "tmux".into(),
        execution_nonce: None,
        old: old.cloned(),
        new: p5::BindingTarget::Source(new.clone()),
        cause,
        parent_hint: None,
        evidence: p5::BindingEvidence {
            hook_event: None,
            received_at: Utc::now(),
        },
        committed_at: Utc::now(),
    };
    let mut line = serde_json::to_vec(&event).unwrap();
    line.push(b'\n');
    line
}

fn closed_turn(id: &str, text: &str) -> Vec<u8> {
    let closed = serde_json::json!({"type":"system", "subtype":"turn_duration", "durationMs":5});
    [row(id, text), format!("{closed}\n").into_bytes()].concat()
}

#[tokio::test(start_paused = true)]
async fn a_file_logged_before_and_after_a_renumber_is_adopted_as_one_source() {
    let (harness, a_path) = fresh(startup);
    let a0 = source_id_for("s1", &a_path).unwrap();
    append(&a_path, &closed_turn("m0", "delivered"));
    let b_path = a_path.with_file_name("b.jsonl");
    std::fs::write(&b_path, closed_turn("n0", "b delivered")).unwrap();
    let b = source_id_for("s2", &b_path).unwrap();
    let _reboot = renumber::shift(&a_path, REBOOT);
    let a1 = source_id_for("s1", &a_path).unwrap();
    assert!(a1.dev != a0.dev && same_file(&a1, &a0));
    let log = [
        line(1, None, &a0, p5::BindingCause::Startup),
        line(2, Some(&a0), &b, p5::BindingCause::Clear),
        line(3, Some(&b), &a1, p5::BindingCause::Resume),
    ];
    p5_log(harness._runtime.path(), CHANNEL, &log.concat());
    harness.gate.acquired();
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let end = std::fs::metadata(&a_path).unwrap().len();
    let legacy = Cursor {
        path: a_path.clone(),
        cursor: end,
        frontier: end,
    };
    *io.legacy.lock().unwrap() = Some(Arc::new(legacy));
    let tasks = start_host(&harness, &io, &ready);
    polls(3).await;
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    assert_eq!(io.alarms.halted(), []);
    let init = harness.store.read_init(CHANNEL).unwrap().unwrap();
    let named: Vec<_> = init.sources.iter().map(|s| s.source_id.clone()).collect();
    assert_eq!(named, [a0, b], "one source per file, named as first logged");
    append(&a_path, &row("m1", "after the adoption"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["after the adoption"]);
    assert_eq!(io.alarms.halted(), []);
    abort(tasks);
}

#[tokio::test(start_paused = true)]
async fn a_renumbered_source_legacy_stays_behind_is_given_up_only_after_the_stall() {
    let (harness, path) = fresh(startup);
    let logged = source_id_for("s1", &path).unwrap();
    let debt = closed_turn("m0", "undelivered");
    append(&path, &debt);
    let _reboot = renumber::shift(&path, REBOOT);
    harness.gate.acquired();
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let legacy = Cursor {
        path: path.clone(),
        cursor: debt.len() as u64,
        frontier: 0,
    };
    *io.legacy.lock().unwrap() = Some(Arc::new(legacy));
    let tasks = start_host(&harness, &io, &ready);
    polls(3).await;
    assert_eq!(adoption(CHANNEL), Adoption::Deferred);
    let minutes = |n: u64| std::time::Duration::from_secs(n * 60);
    tokio::time::sleep(minutes(39)).await;
    assert_eq!(adoption(CHANNEL), Adoption::Deferred);
    // A renumbered file that stays still keeps the stall clock running to its end.
    tokio::time::sleep(minutes(3)).await;
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    let init = harness.store.read_init(CHANNEL).unwrap().unwrap();
    assert_eq!(init.sources[0].source_id, logged);
    assert_eq!(io.alarms.halted(), []);
    abort(tasks);
}

#[tokio::test(start_paused = true)]
async fn a_new_empty_channel_whose_transcript_was_renumbered_is_activated() {
    let (harness, path) = fresh(startup);
    let logged = source_id_for("s1", &path).unwrap();
    let _reboot = renumber::shift(&path, REBOOT);
    harness.gate.acquired();
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let tasks = start_host(&harness, &io, &ready);
    polls(3).await;
    assert!(ready.accepts(CHANNEL));
    assert_eq!(io.alarms.halted(), []);
    let init = harness.store.read_init(CHANNEL).unwrap().unwrap();
    assert_eq!(init.sources[0].source_id, logged);
    append(&path, &row("m1", "first"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first"]);
    abort(tasks);
}
