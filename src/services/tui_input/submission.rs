//! Prompt submission guards; the caller holds its composer lock across every callback.

use super::actor::gate::{PaneVerdict, judge_pane, own_draft};
use super::actor::pane::MAX_PROMPT_BYTES;
use crate::services::claude_tui::host_input::{InputRefusal, InputRun, StopCause};
use crate::services::claude_tui::input::TuiInputAction;
use crate::services::provider::{CancelToken, cancel_requested};
use crate::services::tui_o::shadow::ShadowProvider;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    InvalidPrompt,
    UnpredictableRender,
    NotReady,
    CaptureUnavailable,
    OwnDraft,
}

pub(crate) struct Submission<'a> {
    pub provider: ShadowProvider,
    pub frame: &'a str,
    pub before: Option<&'a str>,
    pub mutations: usize,
}

/// An empty composer precedes the payload; only a fresh capture of our draft admits Enter.
pub(crate) fn run_prompt_submission_using(
    submission: Submission<'_>,
    cancel_token: Option<&CancelToken>,
    payload: impl FnOnce() -> InputRun,
    after: impl FnOnce() -> Option<String>,
    enter: impl FnOnce() -> InputRun,
) -> InputRun {
    let refused = |reason| InputRun::Refused(InputRefusal::Composer(reason));
    if submission.frame.len() > MAX_PROMPT_BYTES
        || submission.frame.trim().is_empty()
        || submission
            .frame
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return refused(Refusal::InvalidPrompt);
    }
    let Some(before) = submission.before else {
        return refused(Refusal::CaptureUnavailable);
    };
    if judge_pane(submission.provider, before) != PaneVerdict::Ready {
        return refused(Refusal::NotReady);
    }
    if cancel_requested(cancel_token) {
        return InputRun::Cancelled { confirmed: 0 };
    }
    let cancelled_after_payload = || InputRun::Cancelled {
        confirmed: submission.mutations,
    };
    let run = payload();
    if run != InputRun::Applied {
        return run;
    }
    if cancel_requested(cancel_token) {
        return cancelled_after_payload();
    }
    let late_refusal = |reason| InputRun::Indeterminate {
        confirmed: submission.mutations,
        cause: StopCause::Refused(InputRefusal::Composer(reason)),
    };
    let observed = after();
    if cancel_requested(cancel_token) {
        return cancelled_after_payload();
    }
    let Some(after) = observed else {
        return late_refusal(Refusal::CaptureUnavailable);
    };
    // Compare the provider's visible form while preserving the transmitted byte limit above.
    let canonical = submission.frame.replace("\r\n", "\n").replace('\r', "\n");
    let canonical = match submission.provider {
        ShadowProvider::Claude => canonical.replace('\t', "    "),
        ShadowProvider::Codex => canonical,
    };
    let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(&after);
    let busy = super::actor::gate::submission_busy(submission.provider, &plain);
    if busy || !own_draft(submission.provider, &after, &canonical, true) {
        return late_refusal(Refusal::OwnDraft);
    }
    if cancel_requested(cancel_token) {
        return cancelled_after_payload();
    }
    match enter() {
        InputRun::Refused(reason) => InputRun::Indeterminate {
            confirmed: submission.mutations,
            cause: StopCause::Refused(reason),
        },
        InputRun::Cancelled { confirmed } => InputRun::Cancelled {
            confirmed: submission.mutations + confirmed,
        },
        InputRun::Indeterminate { confirmed, cause } => InputRun::Indeterminate {
            confirmed: submission.mutations + confirmed,
            cause,
        },
        InputRun::Applied => InputRun::Applied,
    }
}

/// A necessary Claude capacity veto; the captured whole draft still proves actual ownership.
pub(crate) fn claude_prompt_fits_pane(
    frame: &str,
    payload: &[TuiInputAction],
    size: Option<(usize, usize)>,
) -> bool {
    use unicode_width::UnicodeWidthStr;
    size.is_some_and(|(width, height)| {
        let width = width.saturating_sub(4);
        let rows = height.saturating_sub(10);
        if width == 0 || rows == 0 {
            return false;
        }
        let lf = frame.matches('\n').count();
        let paste =
            matches!(payload, [TuiInputAction::PasteBuffer(text)] if text.as_str() == frame);
        if paste && (frame.encode_utf16().count() > 800 || lf > rows.min(2)) {
            return lf > 0 || frame.chars().count() > 800;
        }
        frame
            .split('\n')
            .try_fold(0usize, |total, line| {
                let cells = line.replace('\t', "    ").width();
                total.checked_add(cells.div_ceil(width).max(1))
            })
            .is_some_and(|needed| needed <= rows)
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::atomic::Ordering;

    use super::*;

    const IDLE: &str = include_str!("../../../tests/fixtures/tui_input/claude-2.1.293-idle.txt");
    // An observed Claude paste: submitted history is above the two-row bottom composer.
    const OWN: &str = "\
❯ E2E PR1 direct hold. First output exactly [E2E:PR1:pb1-c-s5d-pr1-074645:HOLD]
  then run in the foreground python3 -c 'import time; time.sleep(60)' then
  output exactly [E2E:PR1:pb1-c-s5d-pr1-074645:DONE]

⏺ [E2E:PR1:pb1-c-s5d-pr1-074645:DONE]

✻ Churned for 1m 4s · done 7:47 AM

────────────────────────────────────────────────────────────────────────────────
❯\u{00a0}[📱 adk-e2e-phase-b · 343742347365974026 · b9bf9a71]
  응답에 정확히 한 줄로 [E2E:PR1:pb1-c-s5d-pr1-074645] 만 출력해줘.
────────────────────────────────────────────────────────────────────────────────
  ⏱ 34m │ █░░░░░░░░░ │ 11% │ 105K/1.0M │ 📦️ 100% │ $0.43
  MCP: 2 │ Tools: 2 done
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";
    const FRAME: &str = "[📱 adk-e2e-phase-b · 343742347365974026 · b9bf9a71]\n응답에 정확히 한 줄로 [E2E:PR1:pb1-c-s5d-pr1-074645] 만 출력해줘.";

    fn submission(frame: &str) -> Submission<'_> {
        Submission {
            provider: ShadowProvider::Claude,
            frame,
            before: Some(IDLE),
            mutations: 2,
        }
    }

    #[test]
    fn submission_orders_payload_capture_and_one_enter() {
        assert_eq!(judge_pane(ShadowProvider::Claude, IDLE), PaneVerdict::Ready);
        let events = RefCell::new(Vec::new());
        let run = run_prompt_submission_using(
            submission(FRAME),
            None,
            || {
                events.borrow_mut().push("payload");
                InputRun::Applied
            },
            || {
                events.borrow_mut().push("capture");
                Some(OWN.into())
            },
            || {
                events.borrow_mut().push("enter");
                InputRun::Applied
            },
        );
        assert_eq!(run, InputRun::Applied);
        assert_eq!(*events.borrow(), ["payload", "capture", "enter"]);
    }

    #[test]
    fn pre_guard_refuses_without_callbacks() {
        for (before, reason) in [
            (None, Refusal::CaptureUnavailable),
            (Some(OWN), Refusal::NotReady),
            (Some("capture unavailable"), Refusal::NotReady),
        ] {
            let mut submission = submission(FRAME);
            submission.before = before;
            assert_eq!(
                run_prompt_submission_using(
                    submission,
                    None,
                    || panic!("payload"),
                    || panic!("capture"),
                    || panic!("enter"),
                ),
                InputRun::Refused(InputRefusal::Composer(reason))
            );
        }
    }

    #[test]
    fn late_guard_keeps_payload_effect_indeterminate_and_enter_unsent() {
        let foreign = OWN.replace("출력해줘.", "foreign draft");
        let modal = format!(
            "{OWN}\nDo you want to proceed?\n  1. Yes\n  2. No\nEnter to confirm · esc to cancel"
        );
        for (after, reason) in [
            (None, Refusal::CaptureUnavailable),
            (Some(foreign), Refusal::OwnDraft),
            (Some(modal), Refusal::OwnDraft),
            (Some(format!("✳ Architecting…\n{OWN}")), Refusal::OwnDraft),
            (Some(IDLE.into()), Refusal::OwnDraft),
        ] {
            let events = RefCell::new(Vec::new());
            let run = run_prompt_submission_using(
                submission(FRAME),
                None,
                || {
                    events.borrow_mut().push("payload");
                    InputRun::Applied
                },
                || {
                    events.borrow_mut().push("capture");
                    after
                },
                || panic!("late refusal sent Enter"),
            );
            assert_eq!(
                run,
                InputRun::Indeterminate {
                    confirmed: 2,
                    cause: StopCause::Refused(InputRefusal::Composer(reason)),
                }
            );
            assert_eq!(*events.borrow(), ["payload", "capture"]);
        }
    }

    #[test]
    fn byte_bound_and_terminal_controls_are_checked_before_payload() {
        for frame in [
            "".into(),
            " \n\t".into(),
            "a".repeat(65537),
            format!("{}aa", "가".repeat(21845)),
            "hello\u{1b}[2J".into(),
            "hello\u{7}".into(),
            "hello\u{7f}".into(),
            "hello\u{9b}2J".into(),
        ] {
            assert_eq!(
                run_prompt_submission_using(
                    submission(&frame),
                    None,
                    || panic!("invalid payload"),
                    || panic!("capture"),
                    || panic!("enter"),
                ),
                InputRun::Refused(InputRefusal::Composer(Refusal::InvalidPrompt))
            );
        }
        for frame in ["a".repeat(65536), format!("{}a", "가".repeat(21845))] {
            assert_eq!(frame.len(), 65536);
            assert_eq!(
                run_prompt_submission_using(
                    submission(&frame),
                    None,
                    || InputRun::Refused(InputRefusal::Unknown),
                    || panic!("stopped payload captured"),
                    || panic!("stopped payload entered"),
                ),
                InputRun::Refused(InputRefusal::Unknown)
            );
        }
    }

    #[test]
    fn provider_visible_normalization_is_used_only_for_ownership() {
        let after = "────────────────────\n❯ hello\n      world\n────────────────────\n  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert_eq!(
            run_prompt_submission_using(
                submission("hello\r\n\tworld"),
                None,
                || InputRun::Applied,
                || Some(after.into()),
                || InputRun::Applied,
            ),
            InputRun::Applied
        );
    }

    #[test]
    fn cancellation_keeps_counts_and_stops_later_callbacks() {
        let cancel = CancelToken::new();
        cancel.cancelled.store(true, Ordering::Relaxed);
        assert_eq!(
            run_prompt_submission_using(
                submission(FRAME),
                Some(&cancel),
                || panic!("cancelled payload"),
                || panic!("capture"),
                || panic!("enter"),
            ),
            InputRun::Cancelled { confirmed: 0 }
        );
        cancel.cancelled.store(false, Ordering::Relaxed);
        assert_eq!(
            run_prompt_submission_using(
                submission(FRAME),
                Some(&cancel),
                || {
                    cancel.cancelled.store(true, Ordering::Relaxed);
                    InputRun::Applied
                },
                || panic!("cancelled capture"),
                || panic!("cancelled enter"),
            ),
            InputRun::Cancelled { confirmed: 2 }
        );
        cancel.cancelled.store(false, Ordering::Relaxed);
        assert_eq!(
            run_prompt_submission_using(
                submission(FRAME),
                Some(&cancel),
                || InputRun::Applied,
                || {
                    cancel.cancelled.store(true, Ordering::Relaxed);
                    Some(OWN.into())
                },
                || panic!("capture-time cancellation entered"),
            ),
            InputRun::Cancelled { confirmed: 2 }
        );
    }

    #[test]
    fn enter_failure_cannot_erase_confirmed_payload_mutations() {
        for (enter, expected) in [
            (
                InputRun::Refused(InputRefusal::Unknown),
                InputRun::Indeterminate {
                    confirmed: 2,
                    cause: StopCause::Refused(InputRefusal::Unknown),
                },
            ),
            (
                InputRun::Cancelled { confirmed: 1 },
                InputRun::Cancelled { confirmed: 3 },
            ),
            (
                InputRun::Indeterminate {
                    confirmed: 1,
                    cause: StopCause::Send("lost Enter acknowledgment".into()),
                },
                InputRun::Indeterminate {
                    confirmed: 3,
                    cause: StopCause::Send("lost Enter acknowledgment".into()),
                },
            ),
        ] {
            assert_eq!(
                run_prompt_submission_using(
                    submission(FRAME),
                    None,
                    || InputRun::Applied,
                    || Some(OWN.into()),
                    || enter,
                ),
                expected
            );
        }
    }
    #[test]
    fn cancellation_during_capture_preserves_payload_effect_before_any_late_refusal() {
        for after in [None, Some("unknown render".to_string())] {
            let token = CancelToken::new();
            let run = run_prompt_submission_using(
                submission(FRAME),
                Some(&token),
                || InputRun::Applied,
                || {
                    token.cancelled.store(true, Ordering::Relaxed);
                    after
                },
                || panic!("cancelled capture sent Enter"),
            );
            assert_eq!(run, InputRun::Cancelled { confirmed: 2 });
        }
    }
}
