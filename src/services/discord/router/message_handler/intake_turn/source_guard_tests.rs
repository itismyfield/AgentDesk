//! Classification 2: as-written, comment-free matching in `handle_text_message` or the named
//! helper, so prose and these needles never count. Lexical; counts are not await pairs.

use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::test_support::CommentForgery;

pub(super) const INTAKE_TURN: &str = include_str!("../intake_turn.rs");
pub(super) const RUNTIME_TRANSITION: &str = include_str!("runtime_transition.rs");
pub(super) const BUSY_RETRY: &str = include_str!("../busy_retry.rs");
pub(super) const TUI_FOLLOWUP: &str = include_str!("../tui_followup.rs");
pub(super) const STALE_GUARD: &str = include_str!("stale_dispatch_guard.rs");

const HANDLER: &str = "pub(super) async fn handle_text_message(";
const TRANSITION: [&str; 2] = [
    "    let Some((route, bootstrapped_fresh_thread_session))",
    "    let (mut session_id, mut memento_context_loaded",
];
const NATIVE_CLEAR_PROVIDER: &str = "(http, shared, token, &provider)";
const RECOVERED_FRESH: &str = "&mut t.recovered_fresh";
const SPAWN_BRIDGE: &str = "spawn_turn_bridge(";
const STAMP: &str = "dispatch_stamp::stamp_before_bridge_handoff";
const BUSY_LOG: &str =
    "claude_tui follow-up queued because hosted TUI is busy before prompt submission";
const FINALIZE: &str = "busy_retry::finalize_enqueue(";
const FINALIZE_FN: &str = "pub(super) async fn finalize_enqueue(";
const ACCEPTED: &str = "if outcome.enqueued {";
const REFUSAL: &str = "} else {\n        super::tui_followup::apply_tui_busy_enqueue_refusal(";
const PENDING_REACTION: &str = "note_busy_tui_pre_submit_queue_pending(";
const CLEARED_VIEW: &str = "note_intake_turn_cleared_current(";
const BRIDGE_CONTEXT: &str = "TurnBridgeContext {";
const BRIDGE_IDS: [&str; 3] = [
    "user_msg_id: Some(user_msg_id),",
    "current_msg_id: Some(placeholder_msg_id),",
    "is_external_input_tui_direct: false",
];
const STALE_CALL: &str = "abort_terminal_dispatch_at_turn_start(";
const EXECUTED: &str = "return Ok(IntakeExecution::Executed);";
pub(super) const CONTEXT_TAKE: &str =
    "let session_retry_context = take_session_retry_context(shared, channel_id, Some(&turn_id));";
pub(super) const REMINDER_TAKE: &str =
    "take_and_merge_feedback_reminder(shared, &provider, channel_id, reply_context);";
const RACE_LOSS: &str = "return race_loss::handle_race_loss_enqueue(";
const PLACEHOLDER_POSTED: &str = "intake_latency.mark_placeholder_posted();";
/// Where the recovered reply context enters the prompt (`TurnContext`).
const PROMPT_USE: &str = "reply: reply_context.as_deref(),";
const MANIFEST_USE: &str = "let recovery_context_for_manifest =";
const REFUSAL_FN: &str = "async fn apply_tui_busy_enqueue_refusal(";
const NOTICE: &str = "claude_tui_busy_followup_refusal_notice(";
const CONTEXT_PUT_BACK: &str = "put_back_session_retry_context(";
const REMINDER_PUT_BACK: &str = "put_back_voluntary_feedback_reminder(";
const FORWARD_REMINDER: &str = "session_retry_context,\n            feedback_reminder,";
const GUARD_FN: &str = "pub(super) async fn abort_terminal_dispatch_at_turn_start(";
const STALE_LOOKUP: &str = "stale_dispatch_turn_for_text(";
const PRESERVE_GATE: &str = "&& !preserve_on_cancel";
const ABORT_LOG: &str = "DISPATCH-GUARD: aborted terminal dispatch at turn start";
const PRESERVE_PARAM: &str = "preserve_on_cancel: bool,";

fn item(source: &str, head: &str) -> Result<Lexed, Vec<String>> {
    Lexed::new(source)
        .block(head)
        .ok_or_else(|| vec![format!("missing {head}")])
}

fn before(label: &str, earlier: Option<usize>, later: Option<usize>) -> Vec<String> {
    match (earlier, later) {
        (Some(earlier), Some(later)) if earlier < later => Vec::new(),
        found => vec![format!("{label}: {found:?}")],
    }
}

fn need(present: bool, label: &str) -> Vec<String> {
    match present {
        true => Vec::new(),
        false => vec![label.to_string()],
    }
}

/// The role-override transition keeps the original provider for native clear, and the
/// runtime transition forwards the recovered fresh flag.
pub(super) fn transition_provider_gaps(intake: &str, runtime: &str) -> Vec<String> {
    let src = match item(intake, HANDLER) {
        Ok(src) => src,
        Err(gaps) => return gaps,
    };
    let Some(transition) = src
        .find(TRANSITION[0])
        .and_then(|start| Some(src.slice(start..src.find_after(start, TRANSITION[1])?)))
    else {
        return vec!["runtime transition window is missing".to_string()];
    };
    let mut gaps = need(
        !transition.text().contains("let provider ="),
        "role override must not shadow the original native-clear provider",
    );
    gaps.extend(need(
        transition.contains(NATIVE_CLEAR_PROVIDER),
        "runtime transition and native clear retain settings.provider",
    ));
    gaps.extend(need(
        Lexed::new(runtime).contains(RECOVERED_FRESH),
        "runtime transition must forward recovered_fresh",
    ));
    gaps
}

/// One bridge spawn in the handler, with the dispatch stamp within the 20 lines above it.
pub(super) fn bridge_stamp_gaps(intake: &str) -> Vec<String> {
    let src = match item(intake, HANDLER) {
        Ok(src) => src,
        Err(gaps) => return gaps,
    };
    let Some(spawn) = src.find(SPAWN_BRIDGE) else {
        return vec!["intake handler lost its bridge spawn".to_string()];
    };
    let mut gaps = need(src.count(SPAWN_BRIDGE) == 1, "one bridge-spawn call site");
    let spawn_line = src.text()[..spawn].matches('\n').count();
    let window_start = spawn_line.checked_sub(21).map_or(0, |newline| {
        let mut newlines = src.text().match_indices('\n');
        newlines.nth(newline).map_or(0, |(at, _)| at + 1)
    });
    let window = src.slice(window_start..spawn);
    gaps.extend(need(
        window.contains(STAMP),
        "the dispatch stamp must stay within the preceding 20 lines",
    ));
    gaps
}

/// Busy pre-submit enqueue goes through the shared finalize helper, whose accepted
/// branch reacts as queued and only the refusal branch clears the pending view.
pub(super) fn busy_enqueue_view_gaps(intake: &str, busy_retry: &str) -> Vec<String> {
    let mut gaps = match item(intake, HANDLER) {
        Ok(src) => {
            let log = src.text().find(BUSY_LOG);
            need(
                log.is_some_and(|log| src.slice(0..log).contains(FINALIZE)),
                "busy pre-submit branch must finalize through the shared helper",
            )
        }
        Err(gaps) => gaps,
    };
    let helper = match item(busy_retry, FINALIZE_FN) {
        Ok(helper) => helper,
        Err(missing) => return [gaps, missing].concat(),
    };
    let accepted = helper.find(ACCEPTED);
    let refusal = accepted.and_then(|accepted| helper.find_after(accepted, REFUSAL));
    let (Some(accepted), Some(refusal)) = (accepted, refusal) else {
        gaps.push("accepted and refused enqueue branches must both exist".to_string());
        return gaps;
    };
    let accepted_branch = helper.slice(accepted..refusal);
    let refusal_branch = helper.slice(refusal..helper.text().len());
    gaps.extend(need(
        accepted_branch.contains(PENDING_REACTION),
        "accepted enqueue must apply the queue-pending reaction",
    ));
    gaps.extend(need(
        !accepted_branch.text().contains(CLEARED_VIEW),
        "accepted requeue must keep the reconciler-owned queued marker",
    ));
    gaps.extend(need(
        refusal_branch.contains(CLEARED_VIEW),
        "refused enqueue must clear the optimistic pending view",
    ));
    gaps
}

/// The Discord-origin bridge context keeps the user and placeholder ids apart.
pub(super) fn bridge_context_gaps(intake: &str) -> Vec<String> {
    let context = match item(intake, HANDLER).map(|src| src.block(BRIDGE_CONTEXT)) {
        Ok(Some(context)) => context,
        Ok(None) => return vec![format!("missing {BRIDGE_CONTEXT}")],
        Err(gaps) => return gaps,
    };
    BRIDGE_IDS
        .into_iter()
        .filter(|field| !context.contains(field))
        .map(|field| format!("bridge context lost {field}"))
        .collect()
}

/// The stale-dispatch abort returns before the destructive recovery-context take.
pub(super) fn stale_abort_take_gaps(intake: &str) -> Vec<String> {
    item(intake, HANDLER).map_or_else(
        |gaps| gaps,
        |src| {
            let abort_return = src
                .find(STALE_CALL)
                .and_then(|call| src.find_after(call, EXECUTED));
            before(
                "stale abort returns before the take",
                abort_return,
                src.find(CONTEXT_TAKE),
            )
        },
    )
}

/// One destructive take after the race-loss return and before the placeholder mark and
/// every prompt use; `uses` lists the uses that consume this take.
pub(super) fn take_once_gaps(intake: &str, take: &str, uses: &[&str]) -> Vec<String> {
    let src = match item(intake, HANDLER) {
        Ok(src) => src,
        Err(gaps) => return gaps,
    };
    let mut gaps = need(src.count(take) == 1, "exactly one destructive take");
    gaps.extend(before(
        "race-loss return precedes the take",
        src.find(RACE_LOSS),
        src.find(take),
    ));
    for used in [PLACEHOLDER_POSTED].iter().chain(uses) {
        gaps.extend(before(used, src.find(take), src.find(used)));
    }
    gaps
}

pub(super) const CONTEXT_USES: [&str; 2] = [PROMPT_USE, MANIFEST_USE];
pub(super) const REMINDER_USES: [&str; 1] = [PROMPT_USE];

/// The refusal routes through the put-back helper, which restores the taken input
/// before rendering the refusal notice.
pub(super) fn refusal_put_back_gaps(
    busy_retry: &str,
    tui_followup: &str,
    (forward, put_back): (&str, &str),
) -> Vec<String> {
    let mut gaps = match item(busy_retry, FINALIZE_FN) {
        Ok(helper) => need(
            helper.contains(forward),
            "refusal call site must forward the input",
        ),
        Err(gaps) => gaps,
    };
    match item(tui_followup, REFUSAL_FN) {
        Ok(helper) => gaps.extend(before(
            "put-back precedes the refusal notice",
            helper.find(put_back),
            helper.find(NOTICE),
        )),
        Err(missing) => gaps.extend(missing),
    }
    gaps
}

pub(super) const CONTEXT_PUT_BACK_PAIR: (&str, &str) = (REFUSAL, CONTEXT_PUT_BACK);
pub(super) const REMINDER_PUT_BACK_PAIR: (&str, &str) = (FORWARD_REMINDER, REMINDER_PUT_BACK);

/// The turn-start guard gates its raw stale-text lookup on `!preserve_on_cancel`.
pub(super) fn preserve_gate_gaps(guard: &str) -> Vec<String> {
    item(guard, GUARD_FN).map_or_else(
        |gaps| gaps,
        |src| {
            before(
                "preserve gate precedes the stale lookup",
                src.find(PRESERVE_GATE),
                src.find(STALE_LOOKUP),
            )
        },
    )
}

/// The gated lookup feeds the abort branch, which finishes the mailbox turn and signals
/// abort within its byte budget; the intake caller returns early on that signal.
pub(super) fn abort_branch_gaps(guard: &str, intake: &str) -> Vec<String> {
    let mut gaps = match item(guard, GUARD_FN) {
        Ok(src) => {
            let abort = src
                .find(STALE_LOOKUP)
                .and_then(|lookup| Some(lookup + src.text()[lookup..].find(ABORT_LOG)?));
            match abort {
                Some(abort) => {
                    let within = |needle, limit| {
                        src.find_after(abort, needle)
                            .is_some_and(|at| at - abort < limit)
                    };
                    let mut gaps = need(
                        within("mailbox_finish_turn", 600),
                        "abort must finish the turn",
                    );
                    gaps.extend(need(
                        within("return true;", 1200),
                        "abort must signal the caller",
                    ));
                    gaps
                }
                None => vec!["gated stale lookup must feed the abort log".to_string()],
            }
        }
        Err(gaps) => gaps,
    };
    match item(intake, HANDLER) {
        Ok(src) => {
            let call = src.find(STALE_CALL);
            let early = call.is_some_and(|call| {
                src.find_after(call, EXECUTED)
                    .is_some_and(|at| at - call < 400)
            });
            gaps.extend(need(
                early,
                "the live-intake caller must return early on abort",
            ));
        }
        Err(missing) => gaps.extend(missing),
    }
    gaps
}

/// The handler's `preserve_on_cancel` parameter flows unmodified into the guard call,
/// and the guard still gates on it.
pub(super) fn preserve_param_gaps(intake: &str, guard: &str) -> Vec<String> {
    let mut gaps = match item(intake, HANDLER) {
        Ok(src) => {
            let call = src
                .find(PRESERVE_PARAM)
                .and_then(|param| src.find_after(param, STALE_CALL));
            let forwarded = call.and_then(|call| {
                let end = call + src.text()[call..].find(".await")?;
                Some(src.slice(call..end).contains("preserve_on_cancel"))
            });
            need(
                forwarded == Some(true),
                "preserve_on_cancel must reach the guard call",
            )
        }
        Err(gaps) => gaps,
    };
    gaps.extend(need(
        Lexed::new(guard).contains(PRESERVE_GATE),
        "the extracted guard must gate on !preserve_on_cancel",
    ));
    gaps
}

type IntakeJudge = fn(&str) -> Vec<String>;

fn assert_rejects(source: &str, judge: &dyn Fn(&str) -> Vec<String>, needles: &[&str]) {
    assert_eq!(judge(source), Vec::<String>::new());
    for needle in needles {
        for forgery in CommentForgery::ALL {
            let forged = forge(source, needle, forgery);
            assert!(forged.contains(needle));
            assert!(!judge(&forged).is_empty(), "{forgery:?} `{needle}` passes");
        }
    }
}

#[test]
fn handler_guards_reject_commented_steps() {
    let take_once = |src: &str| take_once_gaps(src, CONTEXT_TAKE, &CONTEXT_USES);
    let reminder_once = |src: &str| take_once_gaps(src, REMINDER_TAKE, &REMINDER_USES);
    let busy = |src: &str| busy_enqueue_view_gaps(src, BUSY_RETRY);
    let abort = |src: &str| abort_branch_gaps(STALE_GUARD, src);
    let param = |src: &str| preserve_param_gaps(src, STALE_GUARD);
    let transition = |src: &str| transition_provider_gaps(src, RUNTIME_TRANSITION);
    let cases: [(&dyn Fn(&str) -> Vec<String>, &[&str]); 9] = [
        (
            &transition,
            &[HANDLER, TRANSITION[0], TRANSITION[1], NATIVE_CLEAR_PROVIDER],
        ),
        (&(bridge_stamp_gaps as IntakeJudge), &[SPAWN_BRIDGE, STAMP]),
        (&busy, &[FINALIZE]),
        (
            &(bridge_context_gaps as IntakeJudge),
            &[BRIDGE_CONTEXT, BRIDGE_IDS[0], BRIDGE_IDS[1], BRIDGE_IDS[2]],
        ),
        (
            &(stale_abort_take_gaps as IntakeJudge),
            &[STALE_CALL, EXECUTED, CONTEXT_TAKE],
        ),
        (
            &take_once,
            &[
                CONTEXT_TAKE,
                RACE_LOSS,
                PLACEHOLDER_POSTED,
                PROMPT_USE,
                MANIFEST_USE,
            ],
        ),
        (
            &reminder_once,
            &[REMINDER_TAKE, RACE_LOSS, PLACEHOLDER_POSTED, PROMPT_USE],
        ),
        (&abort, &[STALE_CALL, EXECUTED]),
        (&param, &[PRESERVE_PARAM, STALE_CALL]),
    ];
    for (judge, needles) in cases {
        assert_rejects(INTAKE_TURN, judge, needles);
    }
}

#[test]
fn helper_guards_reject_commented_steps() {
    let context = |src: &str| refusal_put_back_gaps(src, TUI_FOLLOWUP, CONTEXT_PUT_BACK_PAIR);
    let reminder = |src: &str| refusal_put_back_gaps(src, TUI_FOLLOWUP, REMINDER_PUT_BACK_PAIR);
    let busy = |src: &str| busy_enqueue_view_gaps(INTAKE_TURN, src);
    assert_rejects(BUSY_RETRY, &context, &[FINALIZE_FN, REFUSAL]);
    assert_rejects(BUSY_RETRY, &reminder, &[FORWARD_REMINDER]);
    assert_rejects(
        BUSY_RETRY,
        &busy,
        &[ACCEPTED, REFUSAL, PENDING_REACTION, CLEARED_VIEW],
    );
    let context = |src: &str| refusal_put_back_gaps(BUSY_RETRY, src, CONTEXT_PUT_BACK_PAIR);
    let reminder = |src: &str| refusal_put_back_gaps(BUSY_RETRY, src, REMINDER_PUT_BACK_PAIR);
    assert_rejects(
        TUI_FOLLOWUP,
        &context,
        &[REFUSAL_FN, CONTEXT_PUT_BACK, NOTICE],
    );
    assert_rejects(TUI_FOLLOWUP, &reminder, &[REMINDER_PUT_BACK]);
    let abort = |src: &str| abort_branch_gaps(src, INTAKE_TURN);
    assert_rejects(
        STALE_GUARD,
        &preserve_gate_gaps,
        &[GUARD_FN, PRESERVE_GATE, STALE_LOOKUP],
    );
    assert_rejects(
        STALE_GUARD,
        &abort,
        &["mailbox_finish_turn", "return true;"],
    );
    let param = |src: &str| preserve_param_gaps(INTAKE_TURN, src);
    assert_rejects(STALE_GUARD, &param, &[PRESERVE_GATE]);
    let transition = |src: &str| transition_provider_gaps(INTAKE_TURN, src);
    assert_rejects(RUNTIME_TRANSITION, &transition, &[RECOVERED_FRESH]);
}

#[test]
fn intake_guards_reject_reordered_steps_and_count_coded_bans() {
    let take_first = swap(INTAKE_TURN, RACE_LOSS, CONTEXT_TAKE);
    assert!(!take_once_gaps(&take_first, CONTEXT_TAKE, &CONTEXT_USES).is_empty());
    let late_take = swap(INTAKE_TURN, CONTEXT_TAKE, PLACEHOLDER_POSTED);
    assert!(!take_once_gaps(&late_take, CONTEXT_TAKE, &CONTEXT_USES).is_empty());
    let notice_first = swap(TUI_FOLLOWUP, CONTEXT_PUT_BACK, NOTICE);
    let gaps = refusal_put_back_gaps(BUSY_RETRY, &notice_first, CONTEXT_PUT_BACK_PAIR);
    assert!(!gaps.is_empty());
    let lookup_first = swap(STALE_GUARD, PRESERVE_GATE, STALE_LOOKUP);
    assert!(!preserve_gate_gaps(&lookup_first).is_empty());

    let at = Lexed::new(INTAKE_TURN)
        .find(TRANSITION[0])
        .expect("transition window")
        + TRANSITION[0].len();
    let insert = |text: &str| format!("{}{text}{}", &INTAKE_TURN[..at], &INTAKE_TURN[at..]);
    let prose = insert(" = /* let provider = x; */ // let provider = x;\n");
    let none = Vec::<String>::new();
    assert_eq!(transition_provider_gaps(&prose, RUNTIME_TRANSITION), none);
    let coded = insert(" = { let provider = x; };\n");
    assert_eq!(
        transition_provider_gaps(&coded, RUNTIME_TRANSITION).len(),
        1
    );

    let at = Lexed::new(BUSY_RETRY)
        .find(PENDING_REACTION)
        .expect("accepted reaction");
    let insert = |text: &str| format!("{}{text}{}", &BUSY_RETRY[..at], &BUSY_RETRY[at..]);
    let prose = insert(&format!("/* {CLEARED_VIEW} */ // {CLEARED_VIEW}\n"));
    assert_eq!(busy_enqueue_view_gaps(INTAKE_TURN, &prose), none);
    let coded = insert(&format!("{CLEARED_VIEW});\n"));
    assert_eq!(busy_enqueue_view_gaps(INTAKE_TURN, &coded).len(), 1);
}
