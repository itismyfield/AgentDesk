//! Restart reconcile of a Herdr-hosted session: the stored launch record is compared with what
//! its exact pane runs now. It only reads; it never writes, adopts, relaunches or closes.
#![cfg_attr(not(test), allow(dead_code))]

use sqlx::PgPool;

use crate::db::dispatched_sessions::hosted_execution::{
    HostedLocation, HostedLookup, HostedLookupKey, HostedRecord, HostedState, ProcessStamp,
    load_hosted_execution_pg,
};
use crate::services::discord::tmux::execution_identity::herdr_observation::{
    HerdrCurrentExecution, HerdrExecutionMatch, HerdrMarkerEvidence, HerdrUnknown,
    compare_herdr_execution,
};
use crate::services::session_host::HostKind;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostReconcile {
    /// A found row with no hosted record keeps main's tmux path.
    Legacy,
    /// The stored execution against what its pane runs now.
    Herdr(HerdrExecutionMatch),
    /// A complete snapshot of the stored endpoint has no such pane; no other pane stands in.
    Missing,
    /// No row, a failed or conflicting read, an unreadable or retired record.
    Unresolved(String),
}

impl HostReconcile {
    /// Only a confirmed match lets input, an attach or a watcher take the pane.
    pub(crate) fn admits_reconnect(&self) -> bool {
        *self == Self::Herdr(HerdrExecutionMatch::Match)
    }
}

/// The found row's record against its exact pane; a missing, failed or conflicting read is
/// never legacy.
pub(crate) fn reconcile_hosted(
    lookup: &HostedLookup,
    reader: &dyn HerdrExecutionReader,
) -> HostReconcile {
    match lookup {
        HostedLookup::Found(found) => reconcile_record(&found.record, reader),
        other => HostReconcile::Unresolved(format!("{other:?}")),
    }
}

/// The record stays the expectation: nothing observed is stored, and a pane is read only
/// on the stored endpoint.
fn reconcile_record(record: &HostedRecord, reader: &dyn HerdrExecutionReader) -> HostReconcile {
    use HerdrExecutionMatch::Unknown;
    let record = match record {
        HostedRecord::Legacy => return HostReconcile::Legacy,
        HostedRecord::Known(record) => record,
        HostedRecord::Unknown(_) => {
            return HostReconcile::Unresolved("unreadable hosted record".to_string());
        }
    };
    if record.state == HostedState::Retired {
        return HostReconcile::Unresolved("retired hosted record".to_string());
    }
    let (Some(stored), Some(_)) = (&record.location, &record.expected) else {
        return HostReconcile::Herdr(Unknown(HerdrUnknown::NoStoredEvidence));
    };
    match reader.endpoint() {
        None => return HostReconcile::Herdr(Unknown(HerdrUnknown::ProbeFailed)),
        Some(endpoint) if *endpoint != HerdrEndpointId::of(stored) => {
            return HostReconcile::Herdr(Unknown(HerdrUnknown::EndpointChanged));
        }
        Some(_) => {}
    }
    let evidence = match reader.read_pane(&stored.pane_id) {
        HerdrPaneReading::Missing => return HostReconcile::Missing,
        HerdrPaneReading::Unreadable(_) => {
            return HostReconcile::Herdr(Unknown(HerdrUnknown::ProbeFailed));
        }
        HerdrPaneReading::Present(evidence) => evidence,
    };
    let current = HerdrCurrentExecution {
        location: stored.clone(),
        binding_nonce: evidence.binding_nonce,
        root: evidence.root,
        provider_process: evidence.provider_process,
        marker: marker_evidence(&record.owner.logical_key),
    };
    HostReconcile::Herdr(compare_herdr_execution(record, &current))
}

/// Reads the session's row and reconciles it; the row is never written.
pub(crate) async fn reconcile_hosted_session_pg(
    pool: &PgPool,
    key: HostedLookupKey<'_>,
    reader: &(dyn HerdrExecutionReader + Sync),
) -> HostReconcile {
    let lookup = load_hosted_execution_pg(pool, key).await;
    reconcile_hosted(&lookup, reader)
}

fn marker_evidence(logical_key: &str) -> HerdrMarkerEvidence {
    match read_host_kind_marker(logical_key) {
        HostKindMarker::Known(HostKind::Herdr) => HerdrMarkerEvidence::Herdr,
        HostKindMarker::Known(_) => HerdrMarkerEvidence::OtherHost,
        _ => HerdrMarkerEvidence::Lost,
    }
}

/// Whether the name's `.host_kind` marker names a host other than tmux; an absent or
/// unreadable marker does not.
pub(in crate::services::discord) fn names_another_host(tmux_name: &str) -> bool {
    let marker = read_host_kind_marker(tmux_name);
    matches!(marker, HostKindMarker::Known(kind) if kind != HostKind::Tmux)
}

#[cfg(test)]
#[path = "host_reconcile_tests.rs"]
mod tests;
