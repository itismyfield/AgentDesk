//! Classification 2: tick wiring inside the real tick item, comments excluded. A lexical
//! tripwire; the runtime tick tests own behaviour, reachability and await pairing.

use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::test_support::CommentForgery;

const COLLECTOR: &str = include_str!("../turn_stream_collector.rs");
const TICK: &str = include_str!("../streaming_status_tick.rs");

const TICK_CALL: &str = "update_streaming_status_tick(";
const TICK_FN: &str = "pub(super) async fn update_streaming_status_tick(";
const PROGRESS: &str = "let progress_outcome = ctx.legacy_mode.is_legacy()";
const PERSIST: &str = "persist_watcher_stream_progress(";
const PANEL: &str = "if ctx.legacy_mode.is_legacy() && !terminal_progress_rejected {";
const PANEL_UPDATE: &str = "existing_panel_update::update_existing_panel(";
const CLEANUP: &str = "\"watcher_streaming_recent_stop_cleanup\",";
const STOP: &str = "if terminal_progress_rejected {";

/// Collector calls the tick; inside the tick, progress persistence and the panel update
/// run only in their Legacy blocks, progress first, and the cleanup precedes the stop.
fn tick_wiring_gaps(collector: &str, tick: &str) -> Vec<String> {
    let mut gaps = Vec::new();
    if !Lexed::new(collector).contains(TICK_CALL) {
        gaps.push("the collector does not call the tick".to_string());
    }
    let Some(body) = Lexed::new(tick).block(TICK_FN) else {
        gaps.push(format!("missing {TICK_FN}"));
        return gaps;
    };
    let before = |first: &str, second: &str| matches!((body.find(first), body.find(second)), (Some(a), Some(b)) if a < b);
    if !before(PROGRESS, PANEL) {
        gaps.push("Legacy progress must precede the panel gate".to_string());
    }
    for (gate, call) in [(PROGRESS, PERSIST), (PANEL, PANEL_UPDATE)] {
        if !body.block(gate).is_some_and(|block| block.contains(call)) {
            gaps.push(format!("{call} is not inside `{gate}`"));
        }
    }
    if !before(CLEANUP, STOP) {
        gaps.push("the recent-stop cleanup must precede the rejection stop".to_string());
    }
    gaps
}

pub(super) fn assert_progress_wiring() {
    let gaps = tick_wiring_gaps(COLLECTOR, TICK);
    assert!(gaps.is_empty(), "{gaps:#?}");
}

#[test]
fn tick_wiring_guard_rejects_commented_and_reordered_steps() {
    for forgery in CommentForgery::ALL {
        let forged = forge(COLLECTOR, TICK_CALL, forgery);
        assert!(!tick_wiring_gaps(&forged, TICK).is_empty());
        for needle in [
            TICK_FN,
            PROGRESS,
            PERSIST,
            PANEL,
            PANEL_UPDATE,
            CLEANUP,
            STOP,
        ] {
            let forged = forge(TICK, needle, forgery);
            assert!(
                !tick_wiring_gaps(COLLECTOR, &forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    for (first, second) in [(PROGRESS, PANEL), (CLEANUP, STOP)] {
        let reordered = swap(TICK, first, second);
        assert!(!tick_wiring_gaps(COLLECTOR, &reordered).is_empty());
    }
    // An extra rejection stop ahead of the cleanup counts; the same text as a comment does not.
    let cleanup = TICK.find(CLEANUP).expect("cleanup argument");
    let statement = TICK[..cleanup]
        .rfind("let outcome =")
        .expect("cleanup statement");
    let early_stop = format!("{STOP} return; }}\n");
    for (inserted, gaps) in [(early_stop.clone(), 1), (format!("// {early_stop}"), 0)] {
        let forged = [&TICK[..statement], inserted.as_str(), &TICK[statement..]].concat();
        assert_eq!(
            tick_wiring_gaps(COLLECTOR, &forged).len(),
            gaps,
            "{inserted}"
        );
    }
}

#[test]
fn tick_wiring_guard_requires_the_calls_inside_their_legacy_blocks() {
    let progress = "ctx.legacy_mode.is_legacy().then(|| {";
    let hoisted = TICK.replacen(
        progress,
        &format!("{progress}\n        }});\n        (|| {{"),
        1,
    );
    assert_ne!(hoisted, TICK);
    assert_eq!(tick_wiring_gaps(COLLECTOR, &hoisted).len(), 1);
    let unguarded = TICK.replacen(PANEL, &format!("{PANEL}\n        }}\n        {{"), 1);
    assert_ne!(unguarded, TICK);
    assert_eq!(tick_wiring_gaps(COLLECTOR, &unguarded).len(), 1);
}
