#![cfg(unix)]

use super::*;

use test_support::{FakeTmux, cleared, refusals, take_exit_reason};

const NAME: &str = "adk-w2a-teardown-sink";

// Only a clearance for this very session, or a turn with no key, tears it down, and then
// in the order main used: audit probe, exit reason, kill.
#[test]
fn teardown_runs_only_for_its_own_clearance_or_an_unkeyed_turn() {
    let (_root, tmux) = FakeTmux::install_with_runtime_root(NAME);
    for (label, clearance) in [
        ("cleared", cleared(NAME)),
        ("unkeyed", TeardownClearance::Unkeyed),
    ] {
        assert_eq!(
            teardown_tmux(Some(&clearance), NAME, "test", "code", "why"),
            Ok(())
        );
        let calls = tmux.take_calls();
        let kill = calls.iter().position(|c| c.starts_with("kill-session"));
        let probe = calls.iter().position(|c| c.starts_with("list-panes"));
        assert!(
            probe.is_some() && probe < kill,
            "{label}: audit first: {calls:?}"
        );
        assert_eq!(
            calls.last().map(String::as_str),
            Some("reason-before-kill"),
            "{label}"
        );
        assert!(take_exit_reason(NAME), "{label}");

        let reported = report_tmux_death(Some(&clearance), NAME, "test", "code", "why", Some(7));
        assert_eq!(reported, Ok(()), "{label}");
        let calls = tmux.take_calls();
        assert!(
            calls.iter().any(|c| c.starts_with("capture-pane")),
            "{label}: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.starts_with("kill-session")),
            "{label}"
        );
        assert!(
            !take_exit_reason(NAME),
            "{label}: a death report writes no exit reason"
        );
    }
    for (label, clearance) in refusals(NAME) {
        let torn = teardown_tmux(clearance.as_ref(), NAME, "test", "code", "why");
        assert!(
            torn.is_err_and(|e| e.contains("host guard kept")),
            "{label}"
        );
        let reported = report_tmux_death(clearance.as_ref(), NAME, "test", "code", "why", None);
        assert!(reported.is_err(), "{label}");
        assert_eq!(
            tmux.take_calls(),
            Vec::<String>::new(),
            "{label}: no audit, no kill"
        );
        assert!(!take_exit_reason(NAME), "{label}");
    }
}
