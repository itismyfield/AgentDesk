//! Classification 2: Codex steering's submit path, judged inside the real entry item with
//! comments blanked. A lexical tripwire; behavior stays with the injection tests.

use crate::test_support::{
    CommentForgery, braced_item_span, comment_out_code, rust_code_mask, strip_rust_comments,
};

pub(super) const INPUT: &str = include_str!("../../input.rs");

const STEERING: &str = "pub(crate) fn inject_steering_prompt(";
const CONFIRMED: &str = "submit_codex_followup_prompt_under_lock(";
const RAW_EXECUTOR: &str = "run_actions_with_executor(";

/// Steering submits through the confirmed composer path and never the raw executor.
pub(super) fn steering_submit_gaps(source: &str) -> Vec<String> {
    let text = strip_rust_comments(source).expect("input source lexes");
    let mask = rust_code_mask(source).expect("input source lexes");
    let Some(span) = braced_item_span(&mask, STEERING) else {
        return vec![format!("missing {STEERING}")];
    };
    let mut gaps = Vec::new();
    if !mask[span.clone()].contains(CONFIRMED) {
        gaps.push(format!("steering does not call {CONFIRMED}"));
    }
    if text[span].contains(RAW_EXECUTOR) {
        gaps.push(format!("steering bypasses through {RAW_EXECUTOR}"));
    }
    gaps
}

#[test]
fn steering_submit_guard_rejects_commented_submit_and_counts_coded_executor() {
    for needle in [STEERING, CONFIRMED] {
        for forgery in CommentForgery::ALL {
            let forged = comment_out_code(INPUT, needle, forgery);
            assert!(
                !steering_submit_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let call = "submit_codex_followup_prompt_under_lock(session_name, prompt, None, None)";
    for (bypass, passes) in [
        (format!("/* not {RAW_EXECUTOR}) */ {call}"), true),
        (format!("{{ let _ = {RAW_EXECUTOR}); {call} }}"), false),
    ] {
        let forged = INPUT.replacen(call, &bypass, 1);
        assert_ne!(forged, INPUT);
        assert_eq!(steering_submit_gaps(&forged).is_empty(), passes, "{bypass}");
    }
}
