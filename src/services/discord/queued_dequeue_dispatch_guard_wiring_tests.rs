//! Classification 2: the dequeue helper's stale guard inside the real helper item, comments
//! excluded. A lexical tripwire; aliases, macros and reachability are not proven.

use super::router::source_guard_support::{Lexed, forge};
use crate::test_support::CommentForgery;

const QUEUE_DISPATCH: &str = include_str!("queue_dispatch.rs");

const HELPER: &str = "async fn mailbox_take_soft_intervention(";
const QUEUED_GUARD: &str = "stale_dispatch_turn_for_queued_intervention(";
const TEXT_GUARD: &str =
    "stale_dispatch_turn_for_text(shared.pg_pool.as_ref(), &intervention.text)";

/// The dequeue helper calls the preservation-aware queued guard in code, and its
/// comment-free text (literals kept) never calls the raw text guard.
fn queued_stale_guard_gaps(source: &str) -> Vec<String> {
    let Some(helper) = Lexed::new(source).block(HELPER) else {
        return vec![format!("missing {HELPER}")];
    };
    let mut gaps = Vec::new();
    if !helper.contains(QUEUED_GUARD) {
        gaps.push("dequeue must retain the preservation-aware queued dispatch guard".to_string());
    }
    if helper.text().contains(TEXT_GUARD) {
        gaps.push(
            "dequeue must not bypass queued preservation with the raw text guard".to_string(),
        );
    }
    gaps
}

#[test]
fn dequeue_uses_preservation_aware_stale_dispatch_guard() {
    assert_eq!(
        queued_stale_guard_gaps(QUEUE_DISPATCH),
        Vec::<String>::new()
    );
}

#[test]
fn dequeue_guard_rejects_commented_helper_or_guard_and_counts_coded_bypass_only() {
    for forgery in CommentForgery::ALL {
        for needle in [HELPER, QUEUED_GUARD] {
            let forged = forge(QUEUE_DISPATCH, needle, forgery);
            assert!(
                !queued_stale_guard_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let helper_at = QUEUE_DISPATCH.find(HELPER).expect("helper head");
    let open = helper_at
        + QUEUE_DISPATCH[helper_at..]
            .find("    loop {\n")
            .expect("helper loop");
    let insert = |line: &str| {
        format!(
            "{}{line}\n{}",
            &QUEUE_DISPATCH[..open],
            &QUEUE_DISPATCH[open..]
        )
    };
    let commented = insert(&format!("    // let _ = super::{TEXT_GUARD};"));
    assert_eq!(queued_stale_guard_gaps(&commented), Vec::<String>::new());
    let coded = insert(&format!("    let _ = super::{TEXT_GUARD};"));
    assert_eq!(queued_stale_guard_gaps(&coded).len(), 1);
}

#[test]
fn dequeue_guard_ignores_a_decoy_helper_and_code_outside_the_helper() {
    let decoy = format!("// {HELPER}) {{ {QUEUED_GUARD} }}\n");
    let forged = forge(QUEUE_DISPATCH, QUEUED_GUARD, CommentForgery::Line);
    let decoyed = format!("{decoy}const _: &str = \"{HELPER}) {{ {QUEUED_GUARD} }}\";\n{forged}");
    assert!(!queued_stale_guard_gaps(&decoyed).is_empty());
    let tail = format!("{QUEUE_DISPATCH}\nfn later() {{ let _ = {TEXT_GUARD}; }}\n");
    assert_eq!(queued_stale_guard_gaps(&tail), Vec::<String>::new());
}
