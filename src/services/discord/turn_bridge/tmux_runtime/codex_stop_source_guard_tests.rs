//! Classification 2: the `/stop` slash entry inside the real `cmd_stop` item, comments
//! excluded. A lexical tripwire: no alias, macro expansion or reachability.

use crate::services::discord::router::source_guard_support::{Lexed, forge};
use crate::test_support::CommentForgery;

const CONTROL: &str = include_str!("../../commands/control.rs");
const CMD_STOP: &str = "async fn cmd_stop(";
const RUN: &str = "run_slash_stop(";
const BYPASSES: [&str; 2] = ["begin_command_stop(", "begin_user_stop("];

/// `cmd_stop` reaches the common stop entry once and never the older begin-stop paths.
fn stop_entry_gaps(control: &str) -> Vec<String> {
    let Some(body) = Lexed::new(control).block(CMD_STOP) else {
        return vec![format!("missing {CMD_STOP}")];
    };
    let mut gaps = Vec::new();
    if body.count(RUN) != 1 {
        gaps.push(format!("{RUN} appears {} times; want 1", body.count(RUN)));
    }
    for bypass in BYPASSES {
        if body.text().contains(bypass) {
            gaps.push(format!("cmd_stop bypasses the common stop with {bypass}"));
        }
    }
    gaps
}

pub(super) fn act7_slash_stop_enters_through_run_slash_stop() {
    assert_eq!(stop_entry_gaps(CONTROL), Vec::<String>::new());
}

#[test]
fn slash_stop_guards_reject_commented_steps_and_count_coded_calls() {
    for forgery in CommentForgery::ALL {
        for needle in [CMD_STOP, RUN] {
            let forged = forge(CONTROL, needle, forgery);
            assert!(
                !stop_entry_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let local = "let (shared, provider) = (&ctx.data().shared";
    let at = CONTROL.find(local).expect("local stop statement");
    let with = |line: &str| [&CONTROL[..at], line, "\n    ", &CONTROL[at..]].concat();
    for call in [
        "begin_user_stop(shared, channel_id);",
        "run_slash_stop(shared, provider, channel_id);",
    ] {
        assert_eq!(
            stop_entry_gaps(&with(&format!("// {call}"))),
            Vec::<String>::new()
        );
        assert_eq!(stop_entry_gaps(&with(call)).len(), 1, "{call}");
    }
}
