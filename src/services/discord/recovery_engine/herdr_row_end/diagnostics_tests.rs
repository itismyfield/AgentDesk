//! The held-row line's rate limit and the per-reason time it reports, at injected instants.
use super::*;

fn at(t0: Instant, ms: u64) -> Instant {
    t0 + Duration::from_millis(ms)
}

/// A change logs at once after a quiet channel, waits until 5 s after the last line, and logs at
/// exactly 5 s.
#[test]
fn a_change_logs_at_five_seconds_and_not_before() {
    let t0 = Instant::now();
    let mut held = Held::new("composer_busy", "", t0);
    assert!(held.observe("composer_busy", "", t0), "the first pass logs");
    assert!(
        !held.observe("transcript_open", "", at(t0, 4_999)),
        "4.999 s is held back"
    );
    assert_eq!(
        held.unshown, 0,
        "the held-back change is still pending, not yet dropped"
    );
    assert!(
        held.observe("transcript_open", "", at(t0, 5_000)),
        "5.000 s logs it"
    );
    assert_eq!(
        (held.previous, held.reason),
        ("composer_busy", "transcript_open")
    );
}

/// Changes inside one limited window and repeats in between each keep their share of the time.
#[test]
fn each_reason_keeps_its_time_through_compressed_changes() {
    let t0 = Instant::now();
    let mut held = Held::new("composer_busy", "", t0);
    assert!(held.observe("composer_busy", "", t0));
    assert!(
        !held.observe("composer_busy", "", at(t0, 500)),
        "a repeat never logs"
    );
    assert!(!held.observe("transcript_open", "", at(t0, 1_000)));
    assert!(!held.observe("file_identity", "", at(t0, 2_000)));
    assert!(!held.observe("ending_busy", "observe", at(t0, 3_000)));
    assert!(held.observe("composer_busy", "", at(t0, 5_000)));
    assert_eq!(held.unshown, 3, "three changes went unlogged in the window");
    assert_eq!(
        held.spent_text(),
        "composer_busy=1000ms transcript_open=1000ms file_identity=1000ms ending_busy@observe=2000ms"
    );
    held.close(at(t0, 7_500));
    assert_eq!(
        held.spent_text(),
        "composer_busy=3500ms transcript_open=1000ms file_identity=1000ms ending_busy@observe=2000ms",
        "the settle closes the open interval into the current reason"
    );
}
