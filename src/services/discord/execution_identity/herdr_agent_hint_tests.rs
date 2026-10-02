use super::*;
use AgentHint::{Diagnostic, Recheck};
use AgentState::{Absent, Blocked, Idle, Working};
use Foreground::{Provider, ShellOrEmpty, Unread};

fn scope() -> ReportScope {
    ReportScope {
        endpoint: "herdr.default".into(),
        pane_id: "w1-1".into(),
        execution_nonce: "n1".into(),
    }
}

fn report(seq: Option<u64>, state: AgentState, foreground: Foreground) -> AgentReport {
    AgentReport {
        transport: 1,
        source: "claude".into(),
        seq,
        state,
        agent_session_id: Some("s1".into()),
        resume_argv: false,
        foreground,
    }
}

// Each agent signal is at most a request to read the pane again: activity, idleness, a block,
// an absence or a resume hint never stands for liveness, completion or cleanup.
#[test]
fn herdr_agent_hint_turns_an_agent_signal_into_at_most_a_recheck() {
    let other_session = AgentReport {
        agent_session_id: Some("s2".into()),
        ..report(None, Idle, Provider)
    };
    let resumable = AgentReport {
        resume_argv: true,
        ..report(None, Idle, Provider)
    };
    let cases = [
        (report(None, Working, Provider), Diagnostic("agent working")),
        (report(None, Idle, Provider), Diagnostic("agent idle")),
        (report(None, Blocked, Unread), Diagnostic("agent blocked")),
        (report(None, Absent, Unread), Diagnostic("agent absent")),
        (report(None, Absent, Provider), Diagnostic("agent absent")),
        (report(None, Idle, ShellOrEmpty), Diagnostic("agent idle")),
        (
            report(None, Absent, ShellOrEmpty),
            Recheck("shell return candidate"),
        ),
        (other_session, Recheck("agent session differs")),
        (resumable, Diagnostic("resume argv reported")),
    ];
    for (report, hint) in cases {
        let mut observer = HerdrAgentObserver::new(1, scope(), Some("s1".into()));
        assert_eq!(observer.observe(&report), hint, "{report:?}");
    }
}

// Until the reporter's seq is verified nothing is ordered by it, so a later report with a
// smaller seq is still read; a reply on another transport is only noted.
#[test]
fn herdr_agent_hint_orders_nothing_by_an_unverified_seq_and_drops_another_transport() {
    let mut observer = HerdrAgentObserver::new(1, scope(), Some("s1".into()));
    assert_eq!(
        observer.observe(&report(Some(9), Working, Provider)),
        Diagnostic("agent working")
    );
    let back_at_shell = report(Some(3), Absent, ShellOrEmpty);
    assert_eq!(
        observer.observe(&back_at_shell),
        Recheck("shell return candidate")
    );
    let elsewhere = AgentReport {
        transport: 2,
        ..back_at_shell
    };
    assert_eq!(
        observer.observe(&elsewhere),
        Diagnostic("another transport's reply")
    );
}
