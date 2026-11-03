//! Classification 2: orphan delete order inside the real reclaim item, comments excluded.
//! A lexical tripwire; aliases, macros, reachability and await pairing are not proven.

use crate::services::discord::router::source_guard_support::Lexed;

const STARTUP_RECLAIM: &str = include_str!("../../startup_reclaim.rs");

const RECLAIM_FN: &str = "async fn reclaim_orphan_placeholders(";
const GUARD: &str = "terminal_cleanup_protects_delete(";
const ATTEMPT: &str = "attempted += 1;";
const DELETE: &str = ".delete_message(http, msg.id)";
const RESULT: &str = "emit_relay_delete_result(";

/// Inside the orphan reclaim item, the cleanup protection, the attempt count, the delete
/// and the result observation all appear in code, in that order.
fn orphan_delete_order_gaps(source: &str) -> Vec<String> {
    let Some(body) = Lexed::new(source).block(RECLAIM_FN) else {
        return vec![format!("missing {RECLAIM_FN}")];
    };
    let steps = [GUARD, ATTEMPT, DELETE, RESULT].map(|needle| (needle, body.find(needle)));
    let mut gaps: Vec<String> = steps
        .iter()
        .filter(|(_, at)| at.is_none())
        .map(|(needle, _)| format!("missing `{needle}` in {RECLAIM_FN}"))
        .collect();
    if gaps.is_empty() && !steps.windows(2).all(|pair| pair[0].1 < pair[1].1) {
        gaps.push(
            "committed and retry-pending terminal placeholders must be skipped before a delete attempt"
                .to_string(),
        );
    }
    gaps
}

pub(super) fn assert_orphan_delete_order() {
    assert_eq!(
        orphan_delete_order_gaps(STARTUP_RECLAIM),
        Vec::<String>::new()
    );
}
