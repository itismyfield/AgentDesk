//! Classification 2: each handoff tail entry point calls the shared options constructor,
//! judged in the code mask of its real item. Lexical; meaningful only beside the value test.

use crate::test_support::{CommentForgery, braced_item_span, comment_out_code, rust_code_mask};

pub(super) const ROLLOUT_TAIL: &str = include_str!("../../rollout_tail.rs");

const ENTRIES: [&str; 3] = [
    "pub fn tail_latest_rollout_for_cwd_with_handoff_for_tmux",
    "pub fn tail_warm_followup_rollout_for_tmux",
    "pub fn tail_resumed_rollout_for_session_with_handoff_for_tmux",
];
const SHARED: &str = "discord_handoff_tail_options(";

/// Entry points whose item is missing or never calls the shared constructor in code.
pub(super) fn handoff_options_gaps(source: &str) -> Vec<String> {
    let mask = rust_code_mask(source).expect("rollout tail source lexes");
    ENTRIES
        .into_iter()
        .filter(|entry| {
            braced_item_span(&mask, entry).is_none_or(|span| !mask[span].contains(SHARED))
        })
        .map(|entry| {
            format!(
                "{entry} builds its own options instead of the shared constructor, so it \
                 can silently miss terminal_range_eligible"
            )
        })
        .collect()
}

#[test]
fn handoff_options_guard_rejects_commented_entries_and_constructor_calls() {
    let mask = rust_code_mask(ROLLOUT_TAIL).expect("rollout tail source lexes");
    for entry in ENTRIES {
        for forgery in CommentForgery::ALL {
            let forged = comment_out_code(ROLLOUT_TAIL, entry, forgery);
            let gaps = handoff_options_gaps(&forged);
            assert!(
                gaps.first().is_some_and(|gap| gap.starts_with(entry)),
                "{forgery:?} commented {entry} still passes"
            );
            // Only this entry's constructor calls become comments.
            let span = braced_item_span(&mask, entry).expect("entry item");
            let body = comment_out_code(&ROLLOUT_TAIL[span.clone()], SHARED, forgery);
            let forged = [
                &ROLLOUT_TAIL[..span.start],
                &body,
                &ROLLOUT_TAIL[span.end..],
            ]
            .concat();
            assert!(forged.contains(SHARED));
            let gaps = handoff_options_gaps(&forged);
            assert_eq!(gaps.len(), 1, "{forgery:?} {entry} {gaps:?}");
            assert!(gaps[0].starts_with(entry));
        }
    }
}
