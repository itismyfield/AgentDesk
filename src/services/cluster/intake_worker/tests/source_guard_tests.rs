//! Classification 2: worker wiring judged in the code mask of the real items. Lexical
//! tripwires: they count calls, not execution, aliases or macro expansion.

use crate::test_support::{
    CommentForgery, braced_item_span, comment_out_code, rust_code_mask, strip_rust_comments,
};

pub(super) const WORKER_ENTRY: &str =
    include_str!("../../../discord/router/message_handler/intake_turn/worker_entry.rs");
pub(super) const INTAKE_WORKER: &str = include_str!("../../intake_worker.rs");

const EXECUTOR: &str = "pub(crate) async fn execute_intake_turn_core(";
const BODY_CALL: &str = "super::handle_text_message(";
const FORWARDED: &str = "request.preserve_on_cancel,\n        request,";
const LITERAL_FALSE: &str = "\n        false,\n        request,";

/// The worker executor forwards the restored preservation bit, never a literal `false`.
pub(super) fn preservation_forwarding_gaps(source: &str) -> Vec<String> {
    let text = strip_rust_comments(source).expect("worker entry lexes");
    let mask = rust_code_mask(source).expect("worker entry lexes");
    let Some(executor) = braced_item_span(&mask, EXECUTOR) else {
        return vec![format!("missing {EXECUTOR}")];
    };
    let Some(call) = mask[executor.clone()]
        .find(BODY_CALL)
        .map(|at| executor.start + at)
    else {
        return vec![format!("executor does not call {BODY_CALL}")];
    };
    let Some(end) = mask[call..executor.end].find("\n    .await") else {
        return vec!["executor does not await the intake body".to_string()];
    };
    let mut gaps = Vec::new();
    if !mask[call..call + end].contains(FORWARDED) {
        gaps.push("the restored preservation bit is not forwarded".to_string());
    }
    if text[call..call + end].contains(LITERAL_FALSE) {
        gaps.push("the historical literal false is forwarded".to_string());
    }
    gaps
}

const TICK: &str = "pub(crate) async fn run_intake_worker_tick(";
const RESULT: &str = "let result = execute_intake_turn_core(";
const OK_ARM: &str = "Ok(IntakeExecution::Executed) => {";
const ERR_ARM: &str = "Err(error) => {";
pub(super) const DONE_WRITE: &str = "let advanced = mark_done(pool, row.id, claim_owner).await?;";

/// From the Ok arm to the Err arm the tick calls `mark_done(` once; from the Err arm on,
/// never. Comments do not count; strings count only as written text, never as calls.
pub(super) fn done_writer_gaps(source: &str) -> Vec<String> {
    let text = strip_rust_comments(source).expect("intake worker lexes");
    let mask = rust_code_mask(source).expect("intake worker lexes");
    let Some(tick) = braced_item_span(&mask, TICK) else {
        return vec![format!("missing {TICK}")];
    };
    let Some(result) = mask[tick.clone()].find(RESULT).map(|at| tick.start + at) else {
        return vec![format!("the tick never reaches {RESULT}")];
    };
    let arm = |head: &str| mask[result..tick.end].find(head).map(|at| result + at);
    let (Some(ok), Some(err)) = (arm(OK_ARM), arm(ERR_ARM)) else {
        return vec!["executor Ok/Err arms are missing".to_string()];
    };
    if ok >= err {
        return vec!["the Ok arm must precede the Err arm".to_string()];
    }
    let mut gaps = Vec::new();
    if mask[ok..err].matches("mark_done(").count() != 1 {
        gaps.push(
            "the Ok arm must call mark_done once; a T2 migration updates the gate allowlist too"
                .to_string(),
        );
    }
    if text[err..tick.end].matches("mark_done(").count() != 0 {
        gaps.push(
            "the Err arm must not call mark_done; a T2 migration updates the gate allowlist too"
                .to_string(),
        );
    }
    gaps
}

#[test]
fn preservation_forwarding_guard_rejects_commented_bit_and_counts_coded_false() {
    for needle in [EXECUTOR, BODY_CALL, "request.preserve_on_cancel,"] {
        for forgery in CommentForgery::ALL {
            let forged = comment_out_code(WORKER_ENTRY, needle, forgery);
            assert!(
                !preservation_forwarding_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let commented = WORKER_ENTRY.replacen(FORWARDED, &format!("{FORWARDED} // false,"), 1);
    assert_eq!(
        preservation_forwarding_gaps(&commented),
        Vec::<String>::new()
    );
    let coded = WORKER_ENTRY.replacen("request.preserve_on_cancel,", "false,", 1);
    assert_eq!(preservation_forwarding_gaps(&coded).len(), 2);
}

#[test]
fn done_writer_guard_rejects_commented_arms_and_counts_coded_err_writes() {
    for needle in [TICK, RESULT, OK_ARM, DONE_WRITE] {
        for forgery in CommentForgery::ALL {
            let forged = comment_out_code(INTAKE_WORKER, needle, forgery);
            assert!(
                !done_writer_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let err_body = "let msg = format!(\"turn execution: {error}\");";
    for (insert, passes) in [
        ("// mark_done(pool, row.id, claim_owner)", true),
        ("let _ = mark_done(pool, row.id, claim_owner).await;", false),
    ] {
        let forged = INTAKE_WORKER.replacen(err_body, &format!("{err_body}\n{insert}"), 1);
        assert_eq!(done_writer_gaps(&forged).is_empty(), passes, "{insert}");
    }
    let doubled =
        INTAKE_WORKER.replacen(DONE_WRITE, &format!("{DONE_WRITE} /* {DONE_WRITE} */"), 1);
    assert_eq!(done_writer_gaps(&doubled), Vec::<String>::new());
}
