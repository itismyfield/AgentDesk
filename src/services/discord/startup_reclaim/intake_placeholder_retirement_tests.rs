use super::*;

#[test]
fn old_and_processing_intake_placeholders_share_exact_orphan_cleanup_guards() {
    let now = 1_700_000_000;
    let old = now - ORPHAN_PLACEHOLDER_MIN_AGE_SECS - 1;
    for content in ["...", "⠋ 계속 처리 중"] {
        let protected = HashSet::from([9001]);
        assert!(is_orphan_placeholder_candidate(
            7, 7, content, 9002, old, now, &protected
        ));
        for (author, message, changed) in [(8, 9002, old), (7, 9001, old), (7, 9002, old + 1)] {
            assert!(!is_orphan_placeholder_candidate(
                author, 7, content, message, changed, now, &protected
            ));
        }
    }
}

#[test]
fn intake_retirement_does_not_turn_prose_or_other_spinners_into_orphan_candidates() {
    let now = 1_700_000_000;
    let old = now - ORPHAN_PLACEHOLDER_MIN_AGE_SECS - 1;
    for content in [
        "…",
        " ... ",
        "⠋ 계속 처리 중\nanswer",
        "answer ⠋ 계속 처리 중",
        "⠙ 계속 처리 중",
    ] {
        assert!(!is_orphan_placeholder_candidate(
            7,
            7,
            content,
            9002,
            old,
            now,
            &HashSet::new()
        ));
    }
}
