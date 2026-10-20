//! Classification 2: each controller acquire item mints a fresh lease holder exactly once,
//! counted in code inside the real fn item. A lexical tripwire: no alias or helper indirection.

use crate::services::discord::router::source_guard_support::{Lexed, forge};
use crate::test_support::CommentForgery;

const SOURCE: &str = include_str!("../../terminal_controller_cutover.rs");
const SHORT: &str = "pub(super) async fn deliver_short_replace_via_controller(";
const LONG: &str = "pub(super) async fn deliver_long_chunks_via_controller(";
const MINT: &str = "let holder = super::terminal_delivery::next_bridge_lease_holder();";

/// The short and long acquire items each hold exactly one coded mint, bounded by their own
/// closing brace rather than by the next item or a doc comment.
fn acquire_site_gaps(source: &str) -> Vec<String> {
    let lexed = Lexed::new(source);
    [SHORT, LONG]
        .into_iter()
        .filter_map(
            |head| match lexed.block(head).map(|item| item.count(MINT)) {
                Some(1) => None,
                Some(count) => Some(format!("{head} mints {count} holders; want 1")),
                None => Some(format!("missing {head}")),
            },
        )
        .collect()
}

pub(super) fn controller_bridge_acquire_sites_mint_fresh_attempts() {
    assert_eq!(acquire_site_gaps(SOURCE), Vec::<String>::new());
}

#[test]
fn acquire_site_guard_rejects_commented_mints_and_counts_coded_copies() {
    for forgery in CommentForgery::ALL {
        for needle in [SHORT, LONG, MINT] {
            let forged = forge(SOURCE, needle, forgery);
            assert!(
                !acquire_site_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let first = SOURCE.find(MINT).expect("short mint");
    for (extra, gaps) in [
        (format!("// {MINT}\n    "), 0),
        (format!("{MINT}\n    "), 1),
    ] {
        let forged = [&SOURCE[..first], extra.as_str(), &SOURCE[first..]].concat();
        assert_eq!(acquire_site_gaps(&forged).len(), gaps, "{extra}");
    }
    let doc = "/// #3998 S1-d: borrowed long-chunk locals";
    let reworded = SOURCE.replacen(doc, "/// Borrowed long-chunk locals", 1);
    assert_ne!(reworded, SOURCE);
    assert_eq!(acquire_site_gaps(&reworded), Vec::<String>::new());
}
