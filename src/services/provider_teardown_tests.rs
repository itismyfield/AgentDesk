#![cfg(unix)]

use super::*;

pub(crate) mod test_support {
    pub(crate) use super::super::test_support::FakeTmux;
    use super::*;
    use crate::services::session_host::{
        AutomaticEffect, HostWitness, SessionTargetEvidence, SessionTargetEvidenceSource,
        SessionTargetInput, StateChange, clear_legacy_session, resolve_session_target,
    };

    struct LegacyRow(String);

    impl SessionTargetEvidenceSource for LegacyRow {
        fn read_evidence(&self, _input: &SessionTargetInput) -> SessionTargetEvidence {
            SessionTargetEvidence {
                session_key: Some(format!("claude/hash/mac-mini:{}", self.0)),
                session_name: Some(self.0.clone()),
                session_record: HostWitness::LegacyRow,
                inflight_locator: HostWitness::Absent,
                host_marker: HostWitness::Absent,
                ..SessionTargetEvidence::unread()
            }
        }
    }

    /// A clearance the guard admitted for a found legacy row named `name`.
    pub(crate) fn cleared(name: &str) -> TeardownClearance {
        let input = SessionTargetInput::SessionKey(format!("claude/hash/mac-mini:{name}"));
        let target = resolve_session_target(input, &LegacyRow(name.to_string()));
        let change = StateChange::Automatic {
            effect: AutomaticEffect::Kill,
            observed: None,
        };
        TeardownClearance::Cleared(clear_legacy_session(&target, change).expect("legacy row"))
    }

    /// Every clearance a teardown must refuse for `name`, labelled.
    pub(crate) fn refusals(name: &str) -> Vec<(&'static str, Option<TeardownClearance>)> {
        vec![
            (
                "refused",
                Some(TeardownClearance::Refused("kept".to_string())),
            ),
            ("missing clearance", None),
            (
                "cleared for another session",
                Some(cleared(&format!("{name}-other"))),
            ),
        ]
    }

    /// Whether the exit reason of `name` is on disk, removing it for the next case.
    pub(crate) fn take_exit_reason(name: &str) -> bool {
        let path = crate::services::tmux_common::session_temp_path(name, "exit_reason");
        std::fs::remove_file(path).is_ok()
    }
}

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
