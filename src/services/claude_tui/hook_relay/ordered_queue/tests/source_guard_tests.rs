//! Classification 2: the atomic-publication syscalls, judged in the code mask of the real
//! publish and sync-stage items. Fault injection proves error handling; this pins the calls.

use crate::test_support::{CommentForgery, braced_item_span, comment_out_code, rust_code_mask};

pub(super) const ORDERED_QUEUE: &str = include_str!("../../ordered_queue.rs");

const PUBLISH: &str = "fn publish_atomic_file(";
const STAGE: &str = "fn sync_atomic_stage(";
const CALLS: [(&str, &str); 3] = [
    (PUBLISH, "|| temp.sync_all()"),
    (PUBLISH, "runtime_store::fsync_parent_dir(path)"),
    (STAGE, "    sync().map_err("),
];

/// Each item invokes its sync step as code, not in a comment or string.
pub(super) fn atomic_sync_gaps(source: &str) -> Vec<String> {
    let mask = rust_code_mask(source).expect("ordered queue source lexes");
    CALLS
        .into_iter()
        .filter(|(item, call)| {
            braced_item_span(&mask, item).is_none_or(|span| !mask[span].contains(call))
        })
        .map(|(item, call)| format!("{item} must invoke {call}"))
        .collect()
}

#[test]
fn atomic_sync_guard_rejects_commented_calls() {
    assert_eq!(atomic_sync_gaps(ORDERED_QUEUE), Vec::<String>::new());
    let needles = [PUBLISH, STAGE]
        .into_iter()
        .chain(CALLS.map(|(_, call)| call));
    for needle in needles {
        for forgery in CommentForgery::ALL {
            let forged = comment_out_code(ORDERED_QUEUE, needle, forgery);
            assert!(
                !atomic_sync_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
}
