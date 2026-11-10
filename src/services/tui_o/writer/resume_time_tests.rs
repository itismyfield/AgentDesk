use super::*;
use crate::services::tui_o::store::ledger::LedgerState;
use crate::services::tui_o::writer::{
    ReaderConfig, ReplaySummary,
    rotation::{Owe, Sources},
};
use std::time::{Duration, SystemTime};

struct ReplayTouch {
    modified: Option<SystemTime>,
    records: usize,
    owed: VecDeque<Derived>,
}

impl Owe for ReplayTouch {
    fn owe(&mut self, source: &SourceId, record: &CapturedRecord, derived: Vec<Derived>) {
        self.records += 1;
        std::fs::File::open(&source.path)
            .unwrap()
            .set_modified(self.modified.unwrap_or_else(SystemTime::now))
            .unwrap();
        self.owed.owe(source, record, derived);
    }

    fn finish_replay(&mut self, ledger: &LedgerState) -> ReplaySummary {
        self.owed.finish_replay(ledger)
    }
}

fn retained() -> (Harness, PathBuf, SourceId, Arc<FakeBindings>) {
    let (h, path, source, bindings) = started(b"");
    let (mut s, mut w, mut d, mut owed) =
        reader_engine(&h, bindings.clone(), Some(ReaderConfig { dormant: true }));
    for index in 0..32 {
        append(&path, &row(&format!("m{index}"), "retained reply"));
    }
    s.capture(&mut w, &mut d, &mut owed).unwrap();
    assert_eq!(
        w.store().cursor(&source).unwrap().captured_through,
        len(&path)
    );
    (h, path, source, bindings)
}

#[test]
fn resume_time_live_replay_accepts_mtime_advanced_during_replay() {
    let (h, path, source, bindings) = retained();
    let mut s = Sources::new(CHANNEL, ShadowProvider::Claude, bindings)
        .with_readers(ReaderConfig { dormant: true });
    let sampled_before = SystemTime::now();
    let mut owed = ReplayTouch {
        modified: None,
        records: 0,
        owed: VecDeque::new(),
    };
    let result = s.resume(
        &mut h.writer(),
        &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
        &mut owed,
    );
    assert!(owed.records >= 32, "must really replay the retained spool");
    assert!(std::fs::metadata(path).unwrap().modified().unwrap() > sampled_before);
    assert!(
        result.is_ok(),
        "legal replay-time mtime advance: {source:?}: {result:?}"
    );
}

#[test]
fn resume_time_long_replay_checks_the_eof_boundary_and_reports_future_times() {
    for future in [false, true] {
        let (h, path, source, bindings) = retained();
        let sampled_at = SystemTime::now() - Duration::from_secs(60);
        let checked_at = sampled_at + Duration::from_secs(23);
        let modified = checked_at + Duration::from_nanos(u64::from(future));
        std::fs::File::open(path)
            .unwrap()
            .set_modified(sampled_at)
            .unwrap();
        let mut s = Sources::new(CHANNEL, ShadowProvider::Claude, bindings)
            .with_readers(ReaderConfig { dormant: true });
        let mut owed = ReplayTouch {
            modified: Some(modified),
            records: 0,
            owed: VecDeque::new(),
        };
        let result = s.resume_with_clock(
            &mut h.writer(),
            &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
            &mut owed,
            sampled_at,
            &|| checked_at,
        );
        assert!(owed.records >= 32);
        if future {
            let WriterAlarm::Halted { detail } = result.unwrap_err() else {
                panic!("legacy alarm variant required")
            };
            for expected in [
                source.path.display().to_string(),
                format!("modified={modified:?}"),
                format!("sampled_at={sampled_at:?}"),
                format!("checked_at={checked_at:?}"),
                "replay_elapsed_ms=".into(),
            ] {
                assert!(detail.contains(&expected), "missing {expected}: {detail}");
            }
        } else {
            assert!(
                result.is_ok(),
                "mtime equal to check time is valid: {result:?}"
            );
        }
    }
}

#[test]
fn resume_time_rejects_wall_clock_regression_during_replay() {
    let (h, path, source, bindings) = retained();
    let sampled_at = SystemTime::now();
    let mut s = Sources::new(CHANNEL, ShadowProvider::Claude, bindings)
        .with_readers(ReaderConfig { dormant: true });
    let mut owed = ReplayTouch {
        modified: Some(sampled_at - Duration::from_secs(10)),
        records: 0,
        owed: VecDeque::new(),
    };
    let result = s.resume_with_clock(
        &mut h.writer(),
        &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
        &mut owed,
        sampled_at,
        &|| sampled_at - Duration::from_secs(1),
    );
    assert!(
        matches!(result, Err(WriterAlarm::Halted { detail }) if detail.contains("wall clock regressed"))
    );
    assert!(std::fs::metadata(path).unwrap().modified().unwrap() < sampled_at);
    assert!(!h.channel().cursor(&source).unwrap().retired);
}

#[test]
fn resume_time_fixed_clock_still_rejects_a_real_future_mtime() {
    let (h, path, source, bindings) = retained();
    let sampled_at = SystemTime::now();
    let modified = sampled_at + Duration::from_secs(3600);
    std::fs::File::open(path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let mut s = Sources::new(CHANNEL, ShadowProvider::Claude, bindings)
        .with_readers(ReaderConfig { dormant: true });
    let result = s.resume_at(
        &mut h.writer(),
        &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
        &mut VecDeque::new(),
        sampled_at,
    );
    assert!(
        matches!(result, Err(WriterAlarm::Halted { detail }) if detail.contains("source mtime is in the future"))
    );
    assert!(!h.channel().cursor(&source).unwrap().retired);
}

#[test]
fn resume_time_still_rejects_changed_identity_or_prefix() {
    for replace in [false, true] {
        let (h, path, source, bindings) = retained();
        let cursor = h.channel().cursor(&source).unwrap().clone();
        let mut bytes = std::fs::read(&path).unwrap();
        if replace {
            std::fs::rename(&path, path.with_extension("old")).unwrap();
        } else {
            bytes[0] = b' ';
        }
        std::fs::write(path, bytes).unwrap();
        let mut s = Sources::new(CHANNEL, ShadowProvider::Claude, bindings)
            .with_readers(ReaderConfig { dormant: true });
        assert!(
            s.resume(
                &mut h.writer(),
                &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
                &mut VecDeque::new()
            )
            .is_err()
        );
        assert_eq!(h.channel().cursor(&source).unwrap(), &cursor);
    }
}

#[test]
fn resume_time_fixed_clock_preserves_cleanup_witness_observed_at() {
    let (h, path, old, bindings) = retained();
    let (_, new) = transcript(&path, "next.jsonl", "s2", b"");
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let at = SystemTime::now() + Duration::from_secs(3600);
    let (mut s, mut w, mut d, mut owed) = reader_engine_at(
        &h,
        bindings.clone(),
        Some(ReaderConfig { dormant: true }),
        at,
    );
    s.capture(&mut w, &mut d, &mut owed).unwrap();
    drop((s, w));
    let mut w = h.writer();
    let mut s = Sources::new(CHANNEL, ShadowProvider::Claude, bindings)
        .with_readers(ReaderConfig { dormant: true });
    s.resume_at(&mut w, &mut d, &mut owed, at).unwrap();
    let witness = w.store().cleanup_witness(&old).unwrap().unwrap();
    assert_eq!(witness.observed_at, at);
    assert_eq!(
        crate::services::tui_o::writer::rotation::probe::observed_at(&s, &old),
        Some(at)
    );
    assert!(witness.bound_to(w.store().cursor(&old).unwrap()));
}
