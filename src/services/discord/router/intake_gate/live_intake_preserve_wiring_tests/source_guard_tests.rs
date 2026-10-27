//! Classification 2: comment-free, as-written matching in `handle_event` or production
//! text before the tests. A lexical tripwire, not a run proof.

use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::test_support::CommentForgery;

pub(crate) const INTAKE_GATE: &str = include_str!("../../intake_gate.rs");

const HANDLE_EVENT: &str = "pub(in crate::services::discord) async fn handle_event(";
const TEST_BOUNDARY: &str = "#[cfg(test)]\nmod reply_context_tests";
const PRESERVATION_ORDER: [&str; 5] = [
    "let preserve_on_cancel =\n                !new_message.author.bot && !author_excluded_from_cancel_preservation;",
    "if !preserve_on_cancel",
    "stale_dispatch_turn_for_text(",
    "origin: super::IntakeOrigin::RawAttachment,",
    "origin: super::IntakeOrigin::LiveMessage,",
];
const REQUEST_REUSE: &str = "turn_kind,\n                        preserve_on_cancel,";
const RAW_SUBMISSION_REUSE: &str =
    "origin: super::IntakeOrigin::RawAttachment,\n                    preserve_on_cancel,";
const LIVE_SUBMISSION_REUSE: &str =
    "origin: super::IntakeOrigin::LiveMessage,\n                    preserve_on_cancel,";
const SHARED_GUARD: &str =
    "if is_allowed_bot_sender\n                && !should_admit_turn_message(";
const SOFT_INTERVENTION: &str = "SoftInterventionSpec {";
fn production(source: &str) -> Option<Lexed> {
    let lexed = Lexed::new(source);
    let end = lexed.find(TEST_BOUNDARY)?;
    Some(lexed.slice(0..end))
}

/// Human preservation is computed once, gates the stale lookup, and the same value
/// feeds both raw-attachment and live request/submission envelopes.
pub(crate) fn preservation_gaps(source: &str) -> Vec<String> {
    let Some(event) = Lexed::new(source).block(HANDLE_EVENT) else {
        return vec![format!("missing {HANDLE_EVENT}")];
    };
    let mut at: Vec<usize> = Vec::new();
    for step in PRESERVATION_ORDER {
        let from = at.last().map_or(0, |last| last + 1);
        match event.find_after(from, step) {
            Some(found) => at.push(found),
            None => return vec![format!("missing or out of order: {step}")],
        }
    }
    let end = event.text().len();
    [
        (at[2]..at[3], REQUEST_REUSE, "raw attachment IntakeRequest"),
        (
            at[3]..at[4],
            RAW_SUBMISSION_REUSE,
            "raw attachment IntakeSubmission",
        ),
        (at[3]..at[4], REQUEST_REUSE, "live IntakeRequest"),
        (at[4]..end, LIVE_SUBMISSION_REUSE, "live IntakeSubmission"),
    ]
    .into_iter()
    .filter(|(window, reuse, _)| !event.slice(window.clone()).contains(reuse))
    .map(|(_, _, label)| format!("{label} must reuse the precomputed preservation value"))
    .collect()
}

/// One outer admission guard precedes every soft-intervention queue entrypoint, and no
/// production branch decides the sender on its own.
pub(crate) fn admission_guard_gaps(source: &str) -> Vec<String> {
    let Some(production) = production(source) else {
        return vec![format!("missing {TEST_BOUNDARY}")];
    };
    let mut gaps = Vec::new();
    match (
        production.find(SHARED_GUARD),
        production.find(SOFT_INTERVENTION),
    ) {
        (Some(guard), Some(queue)) if guard < queue => {}
        _ => gaps.push("the shared admission guard must precede soft interventions".to_string()),
    }
    if production.text().contains("is_allowed_turn_sender(") {
        gaps.push("queue branches must not make sender decisions independently".to_string());
    }
    gaps
}

fn assert_rejects(judge: fn(&str) -> Vec<String>, needles: &[&str]) {
    assert_eq!(judge(INTAKE_GATE), Vec::<String>::new());
    for needle in needles {
        for forgery in CommentForgery::ALL {
            let forged = forge(INTAKE_GATE, needle, forgery);
            assert!(forged.contains(needle));
            assert!(
                !judge(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
}

#[test]
fn preservation_guard_rejects_commented_and_reordered_wiring() {
    let reuse = [REQUEST_REUSE, RAW_SUBMISSION_REUSE, LIVE_SUBMISSION_REUSE];
    let mut needles = vec![HANDLE_EVENT];
    needles.extend(PRESERVATION_ORDER.into_iter().chain(reuse));
    assert_rejects(preservation_gaps, &needles);
    let swapped = swap(INTAKE_GATE, PRESERVATION_ORDER[3], PRESERVATION_ORDER[4]);
    assert!(!preservation_gaps(&swapped).is_empty());
}

#[test]
fn admission_guard_rejects_commented_guard_and_counts_coded_sender_checks() {
    assert_rejects(
        admission_guard_gaps,
        &[TEST_BOUNDARY, SHARED_GUARD, SOFT_INTERVENTION],
    );
    let swapped = swap(INTAKE_GATE, SHARED_GUARD, SOFT_INTERVENTION);
    assert!(!admission_guard_gaps(&swapped).is_empty());
    let call = "is_allowed_turn_sender(";
    let prose = INTAKE_GATE.replacen(
        SHARED_GUARD,
        &format!("// {call}\n/* outer /* {call} */ */\n{SHARED_GUARD}"),
        1,
    );
    assert_eq!(admission_guard_gaps(&prose), Vec::<String>::new());
    let coded = INTAKE_GATE.replacen(SHARED_GUARD, &format!("{call});\n{SHARED_GUARD}"), 1);
    assert_eq!(
        admission_guard_gaps(&coded),
        ["queue branches must not make sender decisions independently"]
    );
}
