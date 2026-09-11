//! #5464 (#5071 T5) A7 — retention for the shared relay-authority observation
//! sink, `<agentdesk root>/relay_authority/YYYY-MM-DD.jsonl`.
//!
//! **Two axes cohabit one file, so the retention unit is the WHOLE FILE.**
//! Axis A's lifecycle records ([`super::authority_observation::append_jsonl`])
//! are the only ones still appended, at
//! `agentdesk_root()/relay_authority/{Local today}.jsonl`. Axis B's
//! structural/reachability candidate records shared that exact path until their
//! writer was retired, so every archived file still carries both. Measured on the
//! live release sink on 2026-09-11, every one of the 8 files carried both
//! schemas (2026-09-07 alone: 1127 `relay_authority.axis_a.v3` lines and 1326
//! `relay_authority.axis_b.v1` lines). Nothing here ever rewrites, truncates or
//! line-filters a file: dropping "expired" lines out of a live file would delete
//! one axis's records out from under the other, and neither axis can be
//! reconstructed from the other's. A file is kept whole or removed whole.
//!
//! The inventory booked this as the one S2 item that lands on *operations*
//! rather than T6 teardown — "JSONL 이벤트 로그에 보존 정책이 없다 … 그 이상은
//! 무한 누적" (`docs/agent-maintenance/t5-t6-removal-inventory.md` §S2, design
//! §5.4 / §8 L-8). This module is that missing owner.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use chrono::NaiveDate;

/// Days of daily sink files to keep, counted from today's publish day.
///
/// **Floor comes from the promotion script, which this lane only reads.**
/// `scripts/relay_authority_rollout_report.py` promotes on ONE segment and
/// requires `WINDOW_DAY_FLOOR = 7` distinct observation days inside it, while
/// `SEGMENT_GAP_HOURS = 48` ends a segment at a gap of 48h or more. Consecutive
/// sample days inside one segment may therefore sit up to two calendar days
/// apart, so a *minimal* qualifying 7-day segment can span
/// `1 + 6 * 2 = 13` calendar days — not 7. Retention shorter than that can
/// delete the older half of a window the operator is still accumulating, which
/// is the one failure this policy exists to prevent.
///
/// Two further reasons the floor is not the ceiling:
///
/// * Daily files are named by **publish** day, not observation day, and a turn
///   stranded without a successor is published "hours or days later" (that
///   script's module docstring, and `authority_observation`'s). A file must
///   outlive the observations it carries.
/// * Stage 2 promotion needs `STAGE_TURN_FLOOR[2] = 500` turns against stage
///   1's 200, which accumulates over correspondingly more calendar days.
///
/// 30 days is ~2.3x the 13-day worst case and bounds the sink instead of
/// letting it grow forever: the live sink measured ~0.47 MB/day (3.3 MB over 7
/// days on 2026-09-11), so the steady state settles near 14 MB.
pub(in crate::services::discord) const OBSERVATION_RETENTION_DAYS: i64 = 30;

/// The narrowest window the promotion script can still be satisfied over, and
/// therefore the hard floor under [`OBSERVATION_RETENTION_DAYS`]. Derivation is
/// in that constant's docs; the guard test pins the relationship so a later edit
/// cannot quietly drop retention below what promotion needs.
pub(in crate::services::discord) const PROMOTION_WINDOW_WORST_CASE_DAYS: i64 = 13;

/// Compile-time floor: retention shorter than the promotion window it has to
/// survive deletes evidence the operator is still accumulating, so that edit
/// must not build at all rather than fail a test someone can skip.
const _: () = assert!(OBSERVATION_RETENTION_DAYS >= PROMOTION_WINDOW_WORST_CASE_DAYS);

const FILE_SUFFIX: &str = ".jsonl";
const FILE_DATE_FORMAT: &str = "%Y-%m-%d";

/// Publish day encoded in a sink file name, or `None` when the name is not one
/// this sink writes. Anything unrecognized is left alone permanently — the
/// directory is the sink's, but this module only claims the names it can prove
/// it owns.
fn sink_file_date(name: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(name.strip_suffix(FILE_SUFFIX)?, FILE_DATE_FORMAT).ok()
}

/// Remove whole daily files published before the retention window. Returns how
/// many were removed. Never partially edits a file, never touches a name it
/// cannot parse, and never touches a file dated today or later — a wall-clock
/// rollback therefore deletes nothing rather than reclassifying live data.
pub(in crate::services::discord) fn prune_expired_observation_files(
    dir: &Path,
    today: NaiveDate,
    retention_days: i64,
) -> usize {
    let Some(cutoff) = today.checked_sub_signed(chrono::Duration::days(retention_days)) else {
        return 0;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        // Keep the boundary day itself: retaining errs toward preserving a
        // window the operator may still be promoting on.
        if sink_file_date(&name).is_none_or(|date| date >= cutoff) {
            continue;
        }
        let path = entry.path();
        if path.is_file() && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Last publish day this process already pruned, per sink directory. The
/// directory is part of the key: a latch keyed on the day alone let the first
/// caller's directory win the day and turned every other directory's prune
/// into a silent no-op (#5891). The path is keyed as given, not canonicalised,
/// so two spellings of one directory can only prune twice, never skip once.
static LAST_PRUNED_DAY: LazyLock<Mutex<HashMap<PathBuf, NaiveDate>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Best-effort prune, at most once per process per publish day per directory,
/// driven from the sink's own write path so retention needs no new task, timer
/// or failure mode.
///
/// Cost is one `read_dir` over a directory this policy holds at ~30 entries plus
/// at most a handful of unlinks, and it is charged to the first write of each
/// day only. Like every other step on this path it is best-effort: a failure is
/// dropped rather than propagated back into the turn that produced the record.
/// The lock is held across the prune so the latch commits after the work, and
/// two first writers of one day serialise instead of both seeing an open latch.
pub(in crate::services::discord) fn prune_observation_dir_once_per_day(
    dir: &Path,
    today: NaiveDate,
) {
    let mut last = LAST_PRUNED_DAY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if last.get(dir) == Some(&today) {
        return;
    }
    let _ = prune_expired_observation_files(dir, today, OBSERVATION_RETENTION_DAYS);
    last.insert(dir.to_path_buf(), today);
}

#[cfg(test)]
mod tests {
    use super::*;

    const AXIS_A: &str = r#"{"schema":"relay_authority.axis_a.v3","site":"bridge_entry"}"#;
    const AXIS_B: &str = r#"{"schema":"relay_authority.axis_b.v1","site":"structural"}"#;

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, FILE_DATE_FORMAT).expect("test date")
    }

    /// A daily file exactly as the sink leaves it: both axes interleaved in one
    /// file, which is what makes whole-file deletion the only safe unit.
    fn write_cohabiting_file(dir: &Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("{AXIS_A}\n{AXIS_B}\n{AXIS_A}\n")).expect("write sink file");
        path
    }

    #[test]
    fn retention_window_is_not_shorter_than_the_promotion_window_it_must_survive() {
        // The whole point of the policy: `scripts/relay_authority_rollout_report.py`
        // needs >=7 distinct observation days in one segment, and a segment
        // tolerates gaps just under SEGMENT_GAP_HOURS=48, so the minimal
        // qualifying window spans 1 + 6*2 = 13 calendar days.
        assert_eq!(PROMOTION_WINDOW_WORST_CASE_DAYS, 1 + 6 * 2);
        assert!(
            OBSERVATION_RETENTION_DAYS >= PROMOTION_WINDOW_WORST_CASE_DAYS,
            "retention {OBSERVATION_RETENTION_DAYS}d would delete part of a \
             {PROMOTION_WINDOW_WORST_CASE_DAYS}d promotion window"
        );
    }

    #[test]
    fn expired_files_are_removed_and_files_inside_the_window_never_are() {
        let temp = tempfile::TempDir::new().expect("temp sink dir");
        let dir = temp.path();
        let today = day("2026-09-11");

        let expired = write_cohabiting_file(dir, "2026-08-01.jsonl");
        let just_expired = write_cohabiting_file(dir, "2026-08-11.jsonl");
        let boundary = write_cohabiting_file(dir, "2026-08-12.jsonl");
        let inside = write_cohabiting_file(dir, "2026-09-04.jsonl");
        let current = write_cohabiting_file(dir, "2026-09-11.jsonl");

        let removed = prune_expired_observation_files(dir, today, OBSERVATION_RETENTION_DAYS);

        assert_eq!(removed, 2, "only the two files past the window are removed");
        assert!(!expired.exists());
        assert!(!just_expired.exists());
        // 2026-08-12 is exactly today-30: the boundary day is retained, because
        // the safe direction for a promotion window is to keep it.
        assert!(boundary.exists(), "boundary day must never be deleted");
        assert!(
            inside.exists(),
            "a file inside the window must never be deleted"
        );
        assert!(current.exists(), "today's file must never be deleted");
    }

    #[test]
    fn a_retained_cohabiting_file_is_left_byte_identical_never_line_filtered() {
        let temp = tempfile::TempDir::new().expect("temp sink dir");
        let dir = temp.path();
        let inside = write_cohabiting_file(dir, "2026-09-04.jsonl");
        let before = fs::read(&inside).expect("read before prune");

        prune_expired_observation_files(dir, day("2026-09-11"), OBSERVATION_RETENTION_DAYS);

        let after = fs::read(&inside).expect("read after prune");
        assert_eq!(
            before, after,
            "retention must not rewrite a file in which axis A and axis B cohabit"
        );
        let text = String::from_utf8(after).expect("utf8 sink file");
        assert!(text.contains("relay_authority.axis_a.v3"));
        assert!(text.contains("relay_authority.axis_b.v1"));
    }

    #[test]
    fn an_expired_cohabiting_file_goes_whole_taking_both_axes_together() {
        let temp = tempfile::TempDir::new().expect("temp sink dir");
        let dir = temp.path();
        let expired = write_cohabiting_file(dir, "2026-01-02.jsonl");

        assert_eq!(
            prune_expired_observation_files(dir, day("2026-09-11"), OBSERVATION_RETENTION_DAYS),
            1
        );
        // Whole-file removal, not a surviving axis-B-only remnant.
        assert!(!expired.exists());
        assert_eq!(fs::read_dir(dir).expect("read dir").count(), 0);
    }

    #[test]
    fn unparsable_names_and_future_dates_are_never_touched() {
        let temp = tempfile::TempDir::new().expect("temp sink dir");
        let dir = temp.path();
        let readme = dir.join("README.md");
        fs::write(&readme, "not a sink file").expect("write readme");
        let odd = dir.join("2026-08-01.jsonl.bak");
        fs::write(&odd, "archived by an operator").expect("write archive");
        let nested = dir.join("2026-08-01");
        fs::create_dir(&nested).expect("create dir named like a date");
        // Wall-clock rollback: the sink is ahead of `today`.
        let future = write_cohabiting_file(dir, "2026-12-25.jsonl");

        assert_eq!(
            prune_expired_observation_files(dir, day("2026-09-11"), OBSERVATION_RETENTION_DAYS),
            0
        );
        assert!(readme.exists());
        assert!(odd.exists());
        assert!(nested.exists());
        assert!(future.exists());
    }

    #[test]
    fn a_missing_sink_directory_is_not_an_error() {
        let temp = tempfile::TempDir::new().expect("temp sink dir");
        let absent = temp.path().join("relay_authority");
        assert_eq!(
            prune_expired_observation_files(&absent, day("2026-09-11"), OBSERVATION_RETENTION_DAYS),
            0
        );
    }

    /// Production only ever calls the once-per-day entry point; every test
    /// above reaches past it straight into the predicate. Without this one,
    /// emptying that function, dropping its inner call or inverting its latch
    /// leaves the policy dead and the whole suite green — the exact regression
    /// this module exists to prevent. Two calls on one publish day pin both
    /// halves: the work happens, and it happens once.
    #[test]
    fn the_once_per_day_entry_point_prunes_first_then_latches_for_that_day() {
        let temp = tempfile::TempDir::new().expect("temp sink dir");
        let dir = temp.path();
        // The latch is keyed by directory, so this fresh temp dir is one no
        // other test in this binary -- however parallel -- can have latched.
        let today = day("2031-03-07");
        let expired_name = "2031-01-05.jsonl";

        let expired = write_cohabiting_file(dir, expired_name);
        prune_observation_dir_once_per_day(dir, today);
        assert!(
            !expired.exists(),
            "the first call of a publish day must actually prune"
        );

        let replanted = write_cohabiting_file(dir, expired_name);
        prune_observation_dir_once_per_day(dir, today);
        assert!(
            replanted.exists(),
            "a second call the same day must latch to a no-op"
        );
    }

    /// Production writers hand the SAME directory in, so a latch keyed on the
    /// day alone only ever held by coincidence: the first directory of a day
    /// won it and every other directory -- a second sink, or a parallel test's
    /// temp root -- was silently skipped for the rest of that day. That is the
    /// flake CI hit in `Library test sweep` and the defect behind #5891; the
    /// key must include the directory.
    #[test]
    fn two_sink_directories_on_one_publish_day_are_each_pruned() {
        let first = tempfile::TempDir::new().expect("first sink dir");
        let second = tempfile::TempDir::new().expect("second sink dir");
        let today = day("2032-06-15");
        let expired_name = "2032-04-01.jsonl";

        let in_first = write_cohabiting_file(first.path(), expired_name);
        let in_second = write_cohabiting_file(second.path(), expired_name);
        prune_observation_dir_once_per_day(first.path(), today);
        prune_observation_dir_once_per_day(second.path(), today);

        assert!(
            !in_first.exists(),
            "the first directory of the day is pruned"
        );
        assert!(
            !in_second.exists(),
            "a second directory on the same day must be pruned too, not eaten by the first's latch"
        );
    }
}
