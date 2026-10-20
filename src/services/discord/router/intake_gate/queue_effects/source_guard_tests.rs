//! Classification 2: as-written, comment-free order inside the placeholder reuse caller.
//! A lexical auxiliary; PATCH/rollback behavior stays with the rekey window tests.

use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::test_support::CommentForgery;

pub(super) const QUEUE_EFFECTS: &str = include_str!("../queue_effects.rs");

const REUSE: &str = "async fn reuse_any_queued_placeholder_for_channel(";
const SEAM_ORDER: [&str; 3] = [
    "lock_owned().await",
    "mailbox_snapshot(",
    "rekey_channel_card_locked(",
];

/// The reuse caller takes the persist lock, then snapshots the mailbox, then rekeys.
pub(super) fn reuse_seam_gaps(source: &str) -> Vec<String> {
    let Some(reuse) = Lexed::new(source).block(REUSE) else {
        return vec![format!("missing {REUSE}")];
    };
    match SEAM_ORDER.map(|step| reuse.find(step)) {
        [Some(lock), Some(snapshot), Some(seam)] if lock < snapshot && snapshot < seam => {
            Vec::new()
        }
        found => vec![format!("lock → snapshot → rekey order broken: {found:?}")],
    }
}

#[test]
fn reuse_seam_guard_rejects_commented_and_reordered_steps() {
    assert_eq!(reuse_seam_gaps(QUEUE_EFFECTS), Vec::<String>::new());
    for needle in [REUSE].into_iter().chain(SEAM_ORDER) {
        for forgery in CommentForgery::ALL {
            let forged = forge(QUEUE_EFFECTS, needle, forgery);
            assert!(forged.contains(needle));
            assert!(
                !reuse_seam_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let reuse = Lexed::new(QUEUE_EFFECTS).find(REUSE).expect("reuse caller");
    let (head, tail) = QUEUE_EFFECTS.split_at(reuse);
    let late_lock = swap(tail, SEAM_ORDER[0], SEAM_ORDER[1]);
    assert!(!reuse_seam_gaps(&format!("{head}{late_lock}")).is_empty());
}
