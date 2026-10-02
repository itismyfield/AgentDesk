//! Herdr agent reports for one observer, projected to a diagnostic or a request to read the pane
//! again; never liveness, completion, cleanup, approval or input. No production reader feeds it.
#![cfg_attr(not(test), allow(dead_code))]

use super::herdr_report_order::{ReportOrder, ReportOrderFilter, ReportScope, ReportSeq};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AgentState {
    Working,
    Idle,
    Blocked,
    /// A complete read named no agent: not integrated, suppressed, released or back at a shell.
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Foreground {
    Provider,
    ShellOrEmpty,
    Unread,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentReport {
    pub transport: u64,
    pub source: String,
    /// The seq as read; the observer decides whether it orders anything.
    pub seq: Option<u64>,
    pub state: AgentState,
    pub agent_session_id: Option<String>,
    pub resume_argv: bool,
    pub foreground: Foreground,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentHint {
    /// Recorded only; nothing acts on it.
    Diagnostic(&'static str),
    /// The owner reads the pane and its execution again before the next input.
    Recheck(&'static str),
}

/// One transport's observer: a new connection gets a new observer, so its order dies with it.
pub(crate) struct HerdrAgentObserver {
    transport: u64,
    scope: ReportScope,
    bound_session: Option<String>,
    order: ReportOrderFilter,
}

impl HerdrAgentObserver {
    pub(crate) fn new(transport: u64, scope: ReportScope, bound_session: Option<String>) -> Self {
        let order = ReportOrderFilter::default();
        Self {
            transport,
            scope,
            bound_session,
            order,
        }
    }

    /// No seq orders reports until the installed schema, the payload it orders and the
    /// reporter list are confirmed.
    fn seq(raw: Option<u64>) -> ReportSeq {
        raw.map_or(ReportSeq::Absent, |_| ReportSeq::Unverified)
    }

    pub(crate) fn observe(&mut self, report: &AgentReport) -> AgentHint {
        if report.transport != self.transport {
            return AgentHint::Diagnostic("another transport's reply");
        }
        let payload = (report.state, &report.agent_session_id, report.foreground);
        let seq = Self::seq(report.seq);
        match self
            .order
            .observe(self.transport, &self.scope, &report.source, seq, &payload)
        {
            ReportOrder::Stale | ReportOrder::OldConnection => {
                return AgentHint::Diagnostic("late report");
            }
            ReportOrder::Diverged | ReportOrder::SourceChanged | ReportOrder::AwaitingRecheck => {
                return AgentHint::Recheck("report order");
            }
            ReportOrder::Accepted | ReportOrder::Unordered => {}
        }
        let session_differs = match (&report.agent_session_id, &self.bound_session) {
            (Some(agent), Some(bound)) => agent != bound,
            _ => false,
        };
        if report.foreground == Foreground::ShellOrEmpty && report.state == AgentState::Absent {
            AgentHint::Recheck("shell return candidate")
        } else if session_differs {
            AgentHint::Recheck("agent session differs")
        } else if report.resume_argv {
            AgentHint::Diagnostic("resume argv reported")
        } else {
            AgentHint::Diagnostic(match report.state {
                AgentState::Working => "agent working",
                AgentState::Idle => "agent idle",
                AgentState::Blocked => "agent blocked",
                AgentState::Absent => "agent absent",
            })
        }
    }
}

#[cfg(test)]
#[path = "herdr_agent_hint_tests.rs"]
mod tests;
