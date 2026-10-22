//! Classification 2: the pinned write-target declarations, judged with comments and
//! literals blanked inside the real function. A lexical tripwire, not a behavior proof.

use crate::test_support::{CommentForgery, braced_item_span, comment_out_code, rust_code_mask};

pub(super) const ENTRIES: &str = include_str!("../../entries.rs");

const UPDATE: &str = "pub async fn update_entry_status_on_pg(";
const RETRY_LOOP: &str = "\n    loop {";
const PINS: [&str; 2] = [
    "let pinned_dispatch_id = options",
    "let pinned_slot_index = options.slot_index.or(current.slot_index);",
];

/// Both `let pinned_*` bindings sit in the update body before its stale-retry loop.
pub(super) fn pinned_identity_gaps(source: &str) -> Vec<String> {
    let mask = rust_code_mask(source).expect("entries source lexes");
    let Some(span) = braced_item_span(&mask, UPDATE) else {
        return vec![format!("missing {UPDATE}")];
    };
    let body = &mask[span];
    let Some(retry_loop) = body.find(RETRY_LOOP) else {
        return vec!["missing the stale-retry loop".to_string()];
    };
    let pins: Vec<usize> = body
        .match_indices("let pinned_")
        .map(|(at, _)| at)
        .collect();
    if pins.len() != 2 || pins.iter().any(|at| *at > retry_loop) {
        return vec![format!(
            "pinned_* declarations {pins:?} vs loop at {retry_loop}"
        )];
    }
    Vec::new()
}

#[test]
fn pinned_identity_guard_rejects_commented_and_loop_scoped_pins() {
    assert_eq!(pinned_identity_gaps(ENTRIES), Vec::<String>::new());
    for needle in [UPDATE].into_iter().chain(PINS) {
        for forgery in CommentForgery::ALL {
            let forged = comment_out_code(ENTRIES, needle, forgery);
            assert!(
                !pinned_identity_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    // The pin recomputed per iteration: the real declarations moved into the loop.
    let start = ENTRIES.find(PINS[0]).expect("first pin");
    let end = ENTRIES.find(PINS[1]).expect("second pin") + PINS[1].len();
    let pins = &ENTRIES[start..end];
    let hoisted_out = ENTRIES.replacen(pins, "", 1);
    let inside = hoisted_out.replacen(RETRY_LOOP, &format!("{RETRY_LOOP}\n        {pins}"), 1);
    assert!(inside.contains(pins) && !pinned_identity_gaps(&inside).is_empty());
    let commented = ENTRIES.replacen(
        RETRY_LOOP,
        &format!("\n    // let pinned_x = 0;{RETRY_LOOP}"),
        1,
    );
    assert_eq!(pinned_identity_gaps(&commented), Vec::<String>::new());
}
