//! Classification 2: the bridge entry gate and authority-loss exit inside the real items, comments
//! and whitespace ignored. A lexical tripwire: no alias, macro, reachability or await pairing.

use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::test_support::CommentForgery;

const CALLER: &str = include_str!("../../mod.rs");
const HELPER: &str = include_str!("../../bridge_entry_persist.rs");

const SPAWN: &str = "pub(in crate::services::discord) fn spawn_turn_bridge_with_pin(";
const AUTHORITY: &str =
    "if let Err(notice) = bridge_entry_persist::establish_bridge_entry_authority(";
const ENTRY_OWNER: &str = "let mut bridge_entry_watcher_owner_epoch_current = inflight_state .effective_relay_owner_kind()";
const GUARDS: &str = "let (mut completion_guard, mut inflight_guard) = make_bridge_guards(";
const NOTICE_DECISION: &str = "resumed_long_running_placeholder_notice_message_id(";
const NOTICE_EDIT: &str = "resumed_msg_id,";
const EMIT: &str = "crate::services::observability::emit_turn_started";
const STREAM: &str = "stream_loop::run_stream_loop";
const FINALIZE: &str = "post_loop_finalize::run_post_loop_finalize";
const FINALIZE_OWNER: &str = "bridge_entry_watcher_owner_epoch_current,";
const OUTCOME_MATCH: &str = "match stream_loop_output.outcome";
const AUTHORITY_LOST: &str = "StreamLoopOutcome::AuthorityLost";
const RELINQUISH: &str = "completion_guard.relinquish_bridge_authority()";
const DEFUSE: &str = "inflight_guard.defuse()";

const ESTABLISH: &str = "pub(super) async fn establish_bridge_entry_authority";
const PERSIST: &str = "let outcome =";
const GATE: &str = "if !bridge_entry_disposition_continues(";
const ANCHOR: &str = "&& !ensure_bridge_current_message_anchor";
const REFRESH: &str = "reconcile_runtime_locals_from_inflight_state";
const ANCHOR_SIGNAL: &str = "signal_bridge_entry_abort_completion(&mut ctx.bridge.completion_tx);";
const ANCHOR_RETURN: &str = "return Err(None);";
const GATE_SIGNAL: &str = "signal_bridge_entry_abort_completion";
const GATE_RETURN: &str = "return Err(capture_entry_abort_notice(";
const GATE_CALL: &str = "bridge_entry_disposition_continues(outcome, !anchor_was_absent)";
const ABORT_FN: &str = "pub(super) fn signal_bridge_entry_abort_completion";
const RECONCILE_FN: &str = "pub(super) fn reconcile_runtime_locals_from_inflight_state";

/// `needle` without whitespace outside literals, to match a compacted view.
fn tight(needle: &str) -> String {
    Lexed::new(needle).compact().text().to_string()
}

/// Each anchor found at or after the previous one, from `from`; `None` once one is missing.
fn chain(lexed: &Lexed, from: usize, anchors: &[&str]) -> Option<Vec<usize>> {
    let mut at = from;
    anchors
        .iter()
        .map(|anchor| {
            at = lexed.find_after(at, &tight(anchor))?;
            Some(at)
        })
        .collect()
}

/// Helper: persist → gate → anchor → refresh, each abort signalling only the waiter. Caller:
/// authority → owner snapshot → guards → notice → emit → stream → finalize, nothing before.
fn entry_order_gaps(caller: &str, helper: &str) -> Vec<String> {
    let mut gaps = Vec::new();
    let helper = Lexed::new(helper).compact();
    match helper.block(&tight(ESTABLISH)) {
        Some(item) => match chain(&item, 0, &[PERSIST, GATE, ANCHOR, REFRESH]).as_deref() {
            Some(&[_, gate, anchor, refresh]) => {
                let failed = item.slice(gate..anchor);
                let unanchored = item.slice(anchor..refresh);
                for (part, needle) in [
                    (&failed, GATE_SIGNAL),
                    (&failed, GATE_RETURN),
                    (&failed, GATE_CALL),
                    (&unanchored, ANCHOR_SIGNAL),
                    (&unanchored, ANCHOR_RETURN),
                ] {
                    if !part.contains(&tight(needle)) {
                        gaps.push(format!("an entry abort lacks `{needle}`"));
                    }
                }
                if failed.text().contains("cohort") {
                    gaps.push("the entry gate reads a rollout cohort".into());
                }
            }
            _ => gaps.push("persist → gate → anchor → refresh order broken".into()),
        },
        None => gaps.push(format!("missing {ESTABLISH}")),
    }
    match chain(&helper, 0, &[ABORT_FN, RECONCILE_FN]).as_deref() {
        Some(&[abort, reconcile]) => {
            let signal = helper.slice(abort..reconcile);
            for banned in ["register_start", "InflightSignal::Completed"] {
                if signal.text().contains(banned) {
                    gaps.push(format!("the entry abort signal reaches `{banned}`"));
                }
            }
        }
        _ => gaps.push(format!("missing {ABORT_FN} before {RECONCILE_FN}")),
    }
    let Some(scope) = Lexed::new(caller).compact().block(&tight(SPAWN)) else {
        gaps.push(format!("missing {SPAWN}"));
        return gaps;
    };
    let steps = [
        AUTHORITY,
        GUARDS,
        NOTICE_DECISION,
        NOTICE_EDIT,
        EMIT,
        STREAM,
        FINALIZE,
    ];
    let Some(&[authority, guards, .., finalize]) = chain(&scope, 0, &steps).as_deref() else {
        gaps.push("authority → guards → notice → emit → stream → finalize order broken".into());
        return gaps;
    };
    if !scope
        .find_after(authority, &tight(ENTRY_OWNER))
        .is_some_and(|owner| owner < guards)
    {
        gaps.push("the relay owner snapshot must sit between authority and guards".into());
    }
    let pre_authority = scope.slice(0..authority);
    for banned in ["make_bridge_guards(", "send_message"] {
        if pre_authority.text().contains(banned) {
            gaps.push(format!("the pre-authority path calls `{banned}`"));
        }
    }
    let guard_call = scope
        .find_after(guards, ");")
        .map(|end| scope.slice(guards..end + 2));
    if !guard_call.is_some_and(|call| call.contains("&inflight_state")) {
        gaps.push("guards must take the post-authority merge".into());
    }
    if !scope
        .slice(finalize..scope.text().len())
        .contains(FINALIZE_OWNER)
    {
        gaps.push("finalize must receive the entry owner verdict".into());
    }
    gaps
}

/// Inside the real outcome match, the authority-loss arm relinquishes, defuses and returns
/// in that order, and the match precedes visible finalization.
fn authority_loss_gaps(caller: &str) -> Vec<String> {
    let lexed = Lexed::new(caller).compact();
    let scope = lexed.block(&tight(SPAWN));
    let outcome = scope.as_ref().and_then(|scope| {
        let at = scope.find(&tight(OUTCOME_MATCH))?;
        let tail = scope.slice(at..scope.text().len());
        let end = at + tail.block(&tight(OUTCOME_MATCH))?.text().len();
        Some((
            tail.arm(AUTHORITY_LOST)?,
            scope.find_after(end, &tight(FINALIZE)),
        ))
    });
    let Some((arm, finalize)) = outcome else {
        return vec!["missing the authority-loss arm of the stream outcome match".into()];
    };
    let mut gaps = Vec::new();
    if chain(&arm, 0, &[RELINQUISH, DEFUSE, "return;"]).is_none() {
        gaps.push("authority loss must relinquish, defuse, then return".into());
    }
    if finalize.is_none() {
        gaps.push("visible finalization must follow the outcome match".into());
    }
    gaps
}

pub(super) fn bridge_entry_failure_gate_precedes_emit_stream_and_finalize() {
    assert_eq!(entry_order_gaps(CALLER, HELPER), Vec::<String>::new());
}

pub(super) fn stream_authority_loss_relinquishes_guards_before_visible_finalization() {
    assert_eq!(authority_loss_gaps(CALLER), Vec::<String>::new());
}

#[test]
fn bridge_entry_guards_reject_commented_and_reordered_steps() {
    let caller_needles = [
        SPAWN,
        AUTHORITY,
        "let mut bridge_entry_watcher_owner_epoch_current = inflight_state",
        "let (mut completion_guard, mut inflight_guard) =",
        NOTICE_DECISION,
        NOTICE_EDIT,
        EMIT,
        STREAM,
        FINALIZE,
        FINALIZE_OWNER,
        "&inflight_state, &shared_owned",
    ];
    let helper_needles = [
        ESTABLISH,
        PERSIST,
        GATE,
        ANCHOR,
        REFRESH,
        ANCHOR_SIGNAL,
        ANCHOR_RETURN,
        GATE_RETURN,
        GATE_CALL,
        ABORT_FN,
    ];
    for forgery in CommentForgery::ALL {
        for needle in caller_needles {
            let forged = forge(CALLER, needle, forgery);
            assert!(
                !entry_order_gaps(&forged, HELPER).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
        for needle in helper_needles {
            let forged = forge(HELPER, needle, forgery);
            assert!(
                !entry_order_gaps(CALLER, &forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
        for needle in [
            SPAWN,
            OUTCOME_MATCH,
            AUTHORITY_LOST,
            RELINQUISH,
            DEFUSE,
            FINALIZE,
        ] {
            let forged = forge(CALLER, needle, forgery);
            assert!(
                !authority_loss_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    for (first, second) in [(EMIT, STREAM), (STREAM, FINALIZE), (NOTICE_DECISION, EMIT)] {
        assert!(!entry_order_gaps(&swap(CALLER, first, second), HELPER).is_empty());
    }
    for (first, second) in [
        (GATE, ANCHOR),
        (ANCHOR, "reconcile_runtime_locals_from_inflight_state(ctx"),
    ] {
        assert!(!entry_order_gaps(CALLER, &swap(HELPER, first, second)).is_empty());
    }
    assert!(!authority_loss_gaps(&swap(CALLER, RELINQUISH, DEFUSE)).is_empty());
}

#[test]
fn bridge_entry_bans_count_coded_calls_but_not_comments() {
    let before = |source: &str, anchor: &str, line: &str| {
        let at = source.find(anchor).expect("insertion anchor");
        [&source[..at], line, "\n", &source[at..]].concat()
    };
    let cases = [
        (
            AUTHORITY,
            "// gateway.send_message(channel_id, \"x\");",
            "gateway.send_message(channel_id, \"x\");",
            true,
        ),
        (ANCHOR, "// let cohort = 1;", "let cohort = 1;", false),
        (
            "pub(super) fn seed_bridge_local_body",
            "// register_start();",
            "fn x() { register_start(); }",
            false,
        ),
    ];
    for (anchor, comment, code, in_caller) in cases {
        let judge = |text: &str, line: &str| {
            if in_caller {
                entry_order_gaps(&before(text, anchor, line), HELPER)
            } else {
                entry_order_gaps(CALLER, &before(text, anchor, line))
            }
        };
        let source = if in_caller { CALLER } else { HELPER };
        assert_eq!(judge(source, comment), Vec::<String>::new(), "{comment}");
        assert_eq!(judge(source, code).len(), 1, "{code}");
    }
}
