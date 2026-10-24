//! Classification 2: production tick files (each cut at its coded `#[cfg(test)]`), stream loop and
//! exit settlement, comments and whitespace ignored. A lexical tripwire: no alias or reachability.

use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::test_support::CommentForgery;

const TICK: &str = include_str!("../stream_tick.rs");
const OUTPUT: &str = include_str!("output.rs");
const RUNTIME: &str = include_str!("runtime.rs");
const STREAM_LOOP: &str = include_str!("../stream_loop.rs");
const EXIT_RECONCILE: &str = include_str!("../stream_loop/exit_reconcile.rs");

const FLUSH: [&str; 3] = [
    "if state_dirty",
    "|| pending_current_message_candidate.is_some()",
    "persist_stream_tick_state_with_candidate_cleanup(",
];
const PREFLIGHT: &str = "\"turn_bridge::stream_tick::anchor_preflight\")";
const ANCHOR: [&str; 3] = [
    "ensure_bridge_current_message_anchor(",
    "reconcile_tick_runtime_from_inflight!(",
    "if !bridge_stream_relay_suppressed(",
];
const DIRTY_ANCHOR: &str = "let current_msg_id_before_flush = current_msg_id;";
const DIRTY: [&str; 3] = [
    "\"turn_bridge::stream_tick::dirty_flush\",",
    "reconcile_tick_runtime_from_inflight!(current_msg_id_before_flush);",
    "writeback_tick_state!();",
];
const ROLLOVER: [&str; 3] = [
    "TurnGateway::send_message(gateway.as_ref(), channel_id, visible_anchor)",
    "\"turn_bridge::stream_tick::rollover_candidate_bind\")",
    "\"turn_bridge::stream_tick::panel_reanchor\")",
];
const PARENT: [&str; 3] = [
    "if !bridge_stream_relay_suppressed(",
    "output::run_rollover_loop(",
    "output::refresh_streaming_response(",
];
const LOOP_DIRTY: &str = "let mut state_dirty = false;";
const OUTER: &str = "'outer: while";
const LOOP_EXIT: [&str; 3] = [
    "let tick_outcome = run_bridge_stream_tick(",
    "if tick_outcome == StreamTickOutcome::AuthorityLost",
    "break 'outer;",
];
const WRITEBACK: &str = "*state.inflight_state = inflight_state;";
const EXIT_SETTLE: &str = "settle_and_reconcile_exit_candidate(";
const EXIT: [&str; 3] = [
    "let current_msg_id_before_exit_settle = *context.state.current_msg_id;",
    "settle_pending_current_message_candidate_on_loop_exit(",
    "reconcile_saved_exit_candidate(",
];

/// `needle` without whitespace outside literals, to match a compacted view.
fn tight(needle: &str) -> String {
    Lexed::new(needle).compact().text().to_string()
}

/// The comment-free production prefix of one file: up to its first coded `#[cfg(test)]`.
fn production(source: &str) -> String {
    let lexed = Lexed::new(source);
    let end = lexed.find("#[cfg(test)]").unwrap_or(lexed.text().len());
    lexed.text()[..end].to_string()
}

/// Each step found at or after the previous one, from `from`; `None` once one is missing.
fn chain(lexed: &Lexed, from: usize, steps: &[&str]) -> Option<Vec<usize>> {
    let mut at = from;
    steps
        .iter()
        .map(|step| {
            at = lexed.find_after(at, &tight(step))?;
            Some(at)
        })
        .collect()
}

/// Candidate retry, anchor preflight → locals reconcile → relay gate, dirty flush reconcile,
/// rollover bind, parent gate → rollover → refresh, authority-loss exit and exit settlement.
fn production_tick_gaps(files: [&str; 3], stream_loop: &str, exit_reconcile: &str) -> Vec<String> {
    let mut gaps = Vec::new();
    let mut need = |held: bool, what: &str| {
        if !held {
            gaps.push(what.to_string());
        }
    };
    let tick = Lexed::new(&files.map(production).join("\n")).compact();
    let first = |needle: &str| tick.find(&tight(needle));
    need(
        chain(&tick, 0, &FLUSH).is_some(),
        "the flush predicate retries a pending candidate",
    );
    let anchor = first(ANCHOR[0]).and_then(|at| chain(&tick, at, &ANCHOR));
    need(
        matches!((first(PREFLIGHT), anchor), (Some(p), Some(a)) if p < a[0]),
        "anchor preflight → ensure → reconcile → relay gate",
    );
    let dirty = first(DIRTY[0]).and_then(|at| chain(&tick, at, &DIRTY));
    let dirty_anchor = |flush: usize| {
        tick.find_all(&tight(DIRTY_ANCHOR))
            .into_iter()
            .any(|at| at < flush)
    };
    need(
        dirty.is_some_and(|steps| dirty_anchor(steps[0])),
        "dirty flush captures, reconciles and writes back",
    );
    let rollover = first(ROLLOVER[0]).and_then(|at| chain(&tick, at, &ROLLOVER));
    need(
        rollover.is_some(),
        "rollover send → candidate bind → panel reanchor",
    );
    let parent = Lexed::new(&production(files[0])).compact();
    let firsts = PARENT.map(|needle| parent.find(&tight(needle)));
    need(
        matches!(firsts, [Some(a), Some(b), Some(c)] if a < b && b < c),
        "parent tick gates rollover before refreshing the response",
    );
    let stream_loop = Lexed::new(stream_loop).compact();
    let (dirty_init, outer) = (
        stream_loop.find(&tight(LOOP_DIRTY)),
        stream_loop.find(&tight(OUTER)),
    );
    let exit = outer.and_then(|outer| {
        let steps = chain(&stream_loop, outer, &LOOP_EXIT)?;
        let writeback = stream_loop.find_after(outer, &tight(WRITEBACK))?;
        let settle = stream_loop.find_after(writeback, &tight(EXIT_SETTLE))?;
        Some((outer, steps[2], writeback, settle))
    });
    need(
        matches!((dirty_init, exit), (Some(d), Some((o, brk, w, _))) if d < o && brk < w),
        "the stream loop exits on authority loss before staging the exit settlement",
    );
    let exit_reconcile = Lexed::new(exit_reconcile).compact();
    need(
        chain(&exit_reconcile, 0, &EXIT).is_some(),
        "exit settlement captures, settles, then reconciles",
    );
    gaps
}

pub(super) fn production_tick_reconciles_anchor_dirty_flush_and_exit_candidate_merges() {
    let gaps = production_tick_gaps([TICK, OUTPUT, RUNTIME], STREAM_LOOP, EXIT_RECONCILE);
    assert_eq!(gaps, Vec::<String>::new());
}

/// A literal anchor as written in `source`: from its opening quote through the next `)`.
fn written<'a>(source: &'a str, anchor: &str) -> &'a str {
    let literal = &anchor[..anchor.rfind('"').expect("quoted anchor") + 1];
    let at = source.find(literal).expect("literal anchor");
    &source[at..at + source[at..].find(')').expect("closing paren") + 1]
}

#[test]
fn production_tick_guard_rejects_commented_and_reordered_steps() {
    let judge = |files: [&str; 3], stream_loop: &str, exit_reconcile: &str| {
        production_tick_gaps(files, stream_loop, exit_reconcile)
    };
    let tick_needles = [
        (TICK, FLUSH[0]),
        (TICK, FLUSH[1]),
        (TICK, FLUSH[2]),
        (TICK, PREFLIGHT),
        (TICK, ANCHOR[0]),
        (TICK, DIRTY_ANCHOR),
        (TICK, DIRTY[0]),
        (TICK, DIRTY[2]),
        (TICK, PARENT[1]),
        (TICK, PARENT[2]),
        (OUTPUT, ROLLOVER[0]),
        (OUTPUT, written(OUTPUT, ROLLOVER[1])),
        (OUTPUT, written(OUTPUT, ROLLOVER[2])),
    ];
    for forgery in CommentForgery::ALL {
        for (file, needle) in tick_needles {
            let forged = forge(file, needle, forgery);
            let files =
                [TICK, OUTPUT, RUNTIME].map(|f| if f == file { forged.as_str() } else { f });
            assert!(
                !judge(files, STREAM_LOOP, EXIT_RECONCILE).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
        for needle in [
            LOOP_DIRTY,
            OUTER,
            LOOP_EXIT[0],
            LOOP_EXIT[1],
            WRITEBACK,
            EXIT_SETTLE,
        ] {
            let forged = forge(STREAM_LOOP, needle, forgery);
            assert!(
                !judge([TICK, OUTPUT, RUNTIME], &forged, EXIT_RECONCILE).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
        for needle in EXIT {
            let forged = forge(EXIT_RECONCILE, needle, forgery);
            assert!(
                !judge([TICK, OUTPUT, RUNTIME], STREAM_LOOP, &forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let tick_swaps = [(PREFLIGHT, ANCHOR[0]), (PARENT[1], PARENT[2])];
    for (first, second) in tick_swaps {
        let swapped = swap(TICK, first, second);
        assert!(!judge([&swapped, OUTPUT, RUNTIME], STREAM_LOOP, EXIT_RECONCILE).is_empty());
    }
    let rollover = swap(
        OUTPUT,
        written(OUTPUT, ROLLOVER[1]),
        written(OUTPUT, ROLLOVER[2]),
    );
    assert!(!judge([TICK, &rollover, RUNTIME], STREAM_LOOP, EXIT_RECONCILE).is_empty());
    let exit_swapped = swap(EXIT_RECONCILE, EXIT[1], EXIT[2]);
    assert!(!judge([TICK, OUTPUT, RUNTIME], STREAM_LOOP, &exit_swapped).is_empty());
}
