use super::*;

fn canonical_pattern() -> regex::Regex {
    let duration = r"(?:\d+시간 \d+분|\d+분(?: \d+초)?|\d+초)";
    let percent = r"(?:100|[1-9]?\d)";
    regex::Regex::new(&format!(
        r"^-# (?:✅(?: 완료| {duration}(?: · 📦 {percent}%)?| · 📦 {percent}%)|(?:⏹ 중단|⚠️ 오류)(?: · {duration})?(?: · 📦 {percent}%)?)$"
    )).unwrap()
}

#[test]
fn canonical_completed_cancelled_and_failed_examples_match_exactly() {
    for (outcome, milliseconds, percent, expected) in [
        (
            Outcome::Completed,
            Some(434_000),
            Some(45),
            "-# ✅ 7분 14초 · 📦 45%",
        ),
        (
            Outcome::Cancelled,
            Some(123_000),
            Some(41),
            "-# ⏹ 중단 · 2분 3초 · 📦 41%",
        ),
        (Outcome::Failed, Some(12_000), None, "-# ⚠️ 오류 · 12초"),
        (Outcome::Completed, None, None, "-# ✅ 완료"),
        (Outcome::Cancelled, None, None, "-# ⏹ 중단"),
        (Outcome::Failed, None, None, "-# ⚠️ 오류"),
        (Outcome::Completed, None, Some(45), "-# ✅ · 📦 45%"),
        (Outcome::Cancelled, None, Some(45), "-# ⏹ 중단 · 📦 45%"),
        (Outcome::Failed, None, Some(45), "-# ⚠️ 오류 · 📦 45%"),
    ] {
        assert_eq!(render(outcome, milliseconds, percent), expected);
    }
}

#[test]
fn elapsed_milliseconds_floor_at_every_second_minute_and_hour_boundary() {
    for (milliseconds, expected) in [
        (0, "0초"),
        (999, "0초"),
        (1000, "1초"),
        (59_000, "59초"),
        (59_999, "59초"),
        (60_000, "1분"),
        (60_999, "1분"),
        (61_000, "1분 1초"),
        (3_599_000, "59분 59초"),
        (3_599_999, "59분 59초"),
        (3_600_000, "1시간 0분"),
        (3_600_999, "1시간 0분"),
        (3_660_000, "1시간 1분"),
        (7_199_999, "1시간 59분"),
        (7_200_000, "2시간 0분"),
        (86_400_000, "24시간 0분"),
        (u64::MAX, "5124095576030시간 25분"),
    ] {
        for outcome in [Outcome::Completed, Outcome::Cancelled, Outcome::Failed] {
            let prefix = match outcome {
                Outcome::Completed => "-# ✅ ",
                Outcome::Cancelled => "-# ⏹ 중단 · ",
                Outcome::Failed => "-# ⚠️ 오류 · ",
            };
            assert_eq!(
                render(outcome, Some(milliseconds), None),
                format!("{prefix}{expected}")
            );
        }
    }
}

#[test]
fn all_percent_values_and_elapsed_extremes_are_bounded_single_lines_in_the_canonical_grammar() {
    let pattern = canonical_pattern();
    for outcome in [Outcome::Completed, Outcome::Cancelled, Outcome::Failed] {
        for milliseconds in [
            None,
            Some(0),
            Some(59_999),
            Some(60_000),
            Some(3_599_999),
            Some(3_600_000),
            Some(u64::MAX),
        ] {
            for percent in std::iter::once(None).chain((0..=u8::MAX).map(Some)) {
                let line = render(outcome, milliseconds, percent);
                assert!(!line.is_empty());
                assert!(line.encode_utf16().count() <= 80, "{line:?}");
                assert!(!line.chars().any(char::is_control), "{line:?}");
                assert!(pattern.is_match(&line), "{line:?}");
                if let Some(percent) = percent {
                    assert!(
                        line.ends_with(&format!("📦 {}%", percent.min(100))),
                        "{line:?}"
                    );
                } else {
                    assert!(!line.contains('📦'));
                }
            }
        }
    }
}

#[test]
fn canonical_grammar_requires_the_completed_space_and_rejects_extra_status_text() {
    let pattern = canonical_pattern();
    for accepted in [
        "-# ✅ 7분 14초 · 📦 45%",
        "-# ⏹ 중단 · 2분 3초 · 📦 41%",
        "-# ⚠️ 오류 · 12초",
    ] {
        assert!(pattern.is_match(accepted), "{accepted:?}");
    }
    for rejected in [
        "-# ✅ · 7분 14초 · 📦 45%",
        "-# ✅",
        "-# ✅ 완료 · 📦 45%",
        "-# ⏹ 중단 2분 3초",
        "-# ✅ 1초 · 📦 101%",
        "-# ✅ 1초 · 도구 실행 중",
        "answer\n-# ✅ 완료",
    ] {
        assert!(!pattern.is_match(rejected), "{rejected:?}");
    }
}
