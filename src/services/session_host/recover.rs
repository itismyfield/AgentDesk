//! Restart read of a durable execution on its own endpoint. The port only reads: it never
//! creates, resends, adopts or closes; the source, claim and output stay with their owners.

use super::herdr_gate::{HerdrPaneView, PaneProvider, PaneReading};
use super::model::HostKind;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, ProcessStamp,
};
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};

/// The endpoint part of a stored location; a pane id means nothing on another endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrEndpointId {
    pub host: String,
    pub execution_node: String,
    pub endpoint_config_key: String,
    pub socket_addr: String,
    pub named_session: String,
}

impl HerdrEndpointId {
    pub(crate) fn of(location: &HostedLocation) -> Self {
        Self {
            host: location.host.clone(),
            execution_node: location.execution_node.clone(),
            endpoint_config_key: location.endpoint_config_key.clone(),
            socket_addr: location.socket_addr.clone(),
            named_session: location.named_session.clone(),
        }
    }
}

/// What the pane runs now; `None` is a value the reader could not read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HerdrPaneEvidence {
    pub binding_nonce: Option<String>,
    /// The pane's `shell_pid` with its process start.
    pub root: Option<ProcessStamp>,
    pub provider_process: Option<ProcessStamp>,
    /// Herdr's reported agent session: a hint for the source owner, never part of the verdict.
    pub agent_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HerdrPaneReading {
    /// A complete, protocol-matching snapshot of the endpoint has no such pane.
    Missing,
    /// The control plane, snapshot or process read gave no usable answer.
    Unreadable(String),
    Present(HerdrPaneEvidence),
}

/// Reads one exact pane on its own endpoint; it never lists, searches or creates panes.
pub(crate) trait HerdrExecutionReader {
    /// `None` when no endpoint is configured.
    fn endpoint(&self) -> Option<&HerdrEndpointId>;
    fn read_pane(&self, pane_id: &str) -> HerdrPaneReading;
}

/// The production reader until a Herdr endpoint is configured: nothing is observable.
pub(crate) struct NoHerdrEndpoint;

impl HerdrExecutionReader for NoHerdrEndpoint {
    fn endpoint(&self) -> Option<&HerdrEndpointId> {
        None
    }

    fn read_pane(&self, _pane_id: &str) -> HerdrPaneReading {
        HerdrPaneReading::Unreadable("no herdr endpoint".to_string())
    }
}

/// Reads the stored pane on the endpoint this node registered for it; it never lists, searches,
/// creates or writes panes.
pub(crate) struct SocketHerdrReader {
    endpoint: HerdrEndpointId,
    view: HerdrPaneView,
    nonce: String,
}

impl SocketHerdrReader {
    /// `None` when no registered endpoint holds the record's location.
    pub(crate) fn of(stored: &HostedExecution) -> Option<Self> {
        let location = stored.location.as_ref()?;
        Some(Self {
            endpoint: HerdrEndpointId::of(location),
            view: super::herdr_registry::registry().view(stored)?,
            nonce: stored.execution_nonce.clone(),
        })
    }
}

impl HerdrExecutionReader for SocketHerdrReader {
    fn endpoint(&self) -> Option<&HerdrEndpointId> {
        Some(&self.endpoint)
    }

    fn read_pane(&self, pane_id: &str) -> HerdrPaneReading {
        if pane_id != self.view.pane_id() {
            return HerdrPaneReading::Unreadable("not the target's pane".into());
        }
        let (root, provider) = match self.view.read_execution() {
            PaneReading::Missing => return HerdrPaneReading::Missing,
            PaneReading::Unreadable(why) => return HerdrPaneReading::Unreadable(why),
            PaneReading::Present { root, provider } => (root, provider),
        };
        // Only a provider whose environment names this execution reports its nonce.
        let provider_process = match provider {
            PaneProvider::Execution(stamp) => Some(stamp),
            PaneProvider::Exited | PaneProvider::Unverified(_) => None,
        };
        HerdrPaneReading::Present(HerdrPaneEvidence {
            binding_nonce: provider_process.as_ref().map(|_| self.nonce.clone()),
            root: Some(root),
            provider_process,
            agent_session_id: None,
        })
    }
}

/// The session's `.host_kind` marker as its reader classified it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HerdrMarkerEvidence {
    Herdr,
    OtherHost,
    /// Absent, unreadable or unrecognized.
    Lost,
}

/// What a reader saw for the pane now. `None` means the value could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrCurrentExecution {
    pub location: HostedLocation,
    pub binding_nonce: Option<String>,
    pub root: Option<ProcessStamp>,
    pub provider_process: Option<ProcessStamp>,
    pub marker: HerdrMarkerEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HerdrUnknown {
    NoStoredEvidence,
    EndpointChanged,
    MarkerLost,
    NotObserved,
    /// No endpoint, or the endpoint did not answer the read.
    ProbeFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HerdrMismatch {
    OtherPane,
    OtherHostMarker,
    OtherNonce,
    RootReplaced,
    ProviderReplaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HerdrExecutionMatch {
    Match,
    Mismatch(HerdrMismatch),
    Unknown(HerdrUnknown),
}

/// A root or provider process is the same only when pid and start both agree.
fn stamp(stored: &ProcessStamp, current: Option<&ProcessStamp>) -> Option<bool> {
    current.map(|current| current == stored)
}

/// Match needs every stored field confirmed; an unreadable value is Unknown, never Mismatch.
pub(crate) fn compare_herdr_execution(
    stored: &HostedExecution,
    current: &HerdrCurrentExecution,
) -> HerdrExecutionMatch {
    use HerdrExecutionMatch::{Match, Mismatch, Unknown};
    let (Some(location), Some(expected)) = (&stored.location, &stored.expected) else {
        return Unknown(HerdrUnknown::NoStoredEvidence);
    };
    let now = &current.location;
    let same_endpoint = (&location.host, &location.execution_node)
        == (&now.host, &now.execution_node)
        && (&location.endpoint_config_key, &location.socket_addr)
            == (&now.endpoint_config_key, &now.socket_addr)
        && location.named_session == now.named_session;
    if !same_endpoint {
        return Unknown(HerdrUnknown::EndpointChanged);
    }
    if location.pane_id != now.pane_id {
        return Mismatch(HerdrMismatch::OtherPane);
    }
    if current.marker == HerdrMarkerEvidence::OtherHost {
        return Mismatch(HerdrMismatch::OtherHostMarker);
    }
    let checks = [
        (
            current
                .binding_nonce
                .as_deref()
                .map(|nonce| nonce == stored.execution_nonce),
            HerdrMismatch::OtherNonce,
        ),
        (
            stamp(&expected.root, current.root.as_ref()),
            HerdrMismatch::RootReplaced,
        ),
        (
            stamp(
                &expected.provider_process,
                current.provider_process.as_ref(),
            ),
            HerdrMismatch::ProviderReplaced,
        ),
    ];
    // A lost marker keeps only a confirmed process replacement on this endpoint and pane.
    let trusted = |kind: &HerdrMismatch| {
        current.marker == HerdrMarkerEvidence::Herdr || *kind != HerdrMismatch::OtherNonce
    };
    let replaced = checks
        .iter()
        .find(|(same, kind)| *same == Some(false) && trusted(kind));
    if let Some((_, mismatch)) = replaced {
        return Mismatch(*mismatch);
    }
    if current.marker == HerdrMarkerEvidence::Lost {
        return Unknown(HerdrUnknown::MarkerLost);
    }
    if checks.iter().any(|(same, _)| same.is_none()) {
        return Unknown(HerdrUnknown::NotObserved);
    }
    Match
}

/// A durable execution (owner, location, expected) and the witness reading its endpoint now.
pub(crate) struct HostRecover<'a> {
    record: &'a HostedExecution,
    witness: &'a dyn HerdrExecutionReader,
}

impl<'a> HostRecover<'a> {
    pub(crate) fn herdr(
        record: &'a HostedExecution,
        witness: &'a dyn HerdrExecutionReader,
    ) -> Self {
        Self { record, witness }
    }
}

/// The stored execution as read now. Only `SameExecution` is that execution; the row's owner
/// decides every effect, and `Unknown` is never taken as gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostRecovery {
    SameExecution,
    /// A complete snapshot of the stored endpoint has no such pane.
    Gone,
    /// The pane answers with something other than the stored execution.
    Withheld(HerdrMismatch),
    Unknown(HerdrUnknown),
}

#[cfg(test)]
thread_local! { static IN_PORT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

/// Whether this thread is inside the recover port, for a fake witness to tell a bypassing caller.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn recover_in_effect() -> bool {
    IN_PORT.get()
}

pub(super) fn recover(kind: HostKind, request: HostRecover<'_>) -> HostRecovery {
    #[cfg(test)]
    let entered = IN_PORT.replace(true);
    let recovery = match kind {
        HostKind::Herdr => read_herdr(request.record, request.witness),
        // Only a Herdr execution keeps a stored location to read again.
        HostKind::Tmux | HostKind::Process => HostRecovery::Unknown(HerdrUnknown::ProbeFailed),
    };
    #[cfg(test)]
    IN_PORT.set(entered);
    tracing::debug!(?kind, ?recovery, "host recover read");
    recovery
}

/// The record stays the expectation: nothing observed is stored, and a pane is read only on the
/// stored endpoint.
fn read_herdr(record: &HostedExecution, witness: &dyn HerdrExecutionReader) -> HostRecovery {
    use HerdrUnknown::{EndpointChanged, NoStoredEvidence, ProbeFailed};
    let (Some(stored), Some(_)) = (&record.location, &record.expected) else {
        return HostRecovery::Unknown(NoStoredEvidence);
    };
    match witness.endpoint() {
        None => return HostRecovery::Unknown(ProbeFailed),
        Some(endpoint) if *endpoint != HerdrEndpointId::of(stored) => {
            return HostRecovery::Unknown(EndpointChanged);
        }
        Some(_) => {}
    }
    let evidence = match witness.read_pane(&stored.pane_id) {
        HerdrPaneReading::Missing => return HostRecovery::Gone,
        HerdrPaneReading::Unreadable(_) => return HostRecovery::Unknown(ProbeFailed),
        HerdrPaneReading::Present(evidence) => evidence,
    };
    let current = HerdrCurrentExecution {
        location: stored.clone(),
        binding_nonce: evidence.binding_nonce,
        root: evidence.root,
        provider_process: evidence.provider_process,
        marker: marker_evidence(&record.owner.logical_key),
    };
    match compare_herdr_execution(record, &current) {
        HerdrExecutionMatch::Match => HostRecovery::SameExecution,
        HerdrExecutionMatch::Mismatch(why) => HostRecovery::Withheld(why),
        HerdrExecutionMatch::Unknown(why) => HostRecovery::Unknown(why),
    }
}

fn marker_evidence(logical_key: &str) -> HerdrMarkerEvidence {
    match read_host_kind_marker(logical_key) {
        HostKindMarker::Known(HostKind::Herdr) => HerdrMarkerEvidence::Herdr,
        HostKindMarker::Known(_) => HerdrMarkerEvidence::OtherHost,
        _ => HerdrMarkerEvidence::Lost,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::db::dispatched_sessions::hosted_execution::HostedState;
    use crate::db::dispatched_sessions::hosted_execution::tests::{owner, record};
    use crate::services::session_host::{InteractiveSessionHost, TmuxHost};

    struct Witness(Cell<usize>);

    impl HerdrExecutionReader for Witness {
        fn endpoint(&self) -> Option<&HerdrEndpointId> {
            panic!("a host of another kind consulted the Herdr witness")
        }

        fn read_pane(&self, _pane_id: &str) -> HerdrPaneReading {
            self.0.set(self.0.get() + 1);
            HerdrPaneReading::Missing
        }
    }

    // A Herdr record given to a host of another kind reads nothing and is never taken as gone.
    #[test]
    fn a_host_of_another_kind_reads_nothing_and_never_reports_gone() {
        let stored = record(&owner("100"), "n1", HostedState::Bound);
        let witness = Witness(Cell::new(0));
        let recovered = TmuxHost.recover(HostRecover::herdr(&stored, &witness));
        assert_eq!(recovered, HostRecovery::Unknown(HerdrUnknown::ProbeFailed));
        assert_eq!(witness.0.get(), 0);
        assert!(!recover_in_effect());
    }
}
