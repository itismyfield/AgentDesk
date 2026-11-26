//! Restart reconcile of a Herdr-hosted session: the host's recover port reads the stored execution
//! on its exact pane and the row's state picks the verdict. It never writes, adopts or closes.
#![cfg_attr(not(test), allow(dead_code))]

use sqlx::PgPool;

use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedRecord, HostedState, load_hosted_execution_pg,
};
use crate::services::session_host::HerdrExecutionMatch;
pub(crate) use crate::services::session_host::{
    HerdrEndpointId, HerdrExecutionReader, HerdrPaneEvidence, HerdrPaneReading, NoHerdrEndpoint,
};
use crate::services::session_host::{HostKind, HostRecover, HostRecovery, host_for};
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostReconcile {
    /// A found row with no hosted record keeps main's tmux path.
    Legacy,
    /// The stored Bound execution against what its pane runs now.
    Herdr(HerdrExecutionMatch),
    /// The same comparison for a launch that never reached Bound; nothing reconnects to it.
    Pending(HerdrExecutionMatch),
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

/// The record's state picks the verdict; its execution is read only through the recover port.
fn reconcile_record(record: &HostedRecord, reader: &dyn HerdrExecutionReader) -> HostReconcile {
    use HerdrExecutionMatch::{Match, Mismatch, Unknown};
    let record = match record {
        HostedRecord::Legacy => return HostReconcile::Legacy,
        HostedRecord::Known(record) => record,
        HostedRecord::Unknown(_) => {
            return HostReconcile::Unresolved("unreadable hosted record".to_string());
        }
    };
    let verdict = match record.state {
        HostedState::Bound => HostReconcile::Herdr,
        HostedState::Pending => HostReconcile::Pending,
        HostedState::Retired => {
            return HostReconcile::Unresolved("retired hosted record".to_string());
        }
    };
    match host_for(HostKind::Herdr).recover(HostRecover::herdr(record, reader)) {
        HostRecovery::SameExecution => verdict(Match),
        HostRecovery::Gone => HostReconcile::Missing,
        HostRecovery::Withheld(why) => verdict(Mismatch(why)),
        HostRecovery::Unknown(why) => verdict(Unknown(why)),
    }
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

/// Whether the name's `.host_kind` marker names a host other than tmux; an absent or
/// unreadable marker does not.
pub(in crate::services::discord) fn names_another_host(tmux_name: &str) -> bool {
    let marker = read_host_kind_marker(tmux_name);
    matches!(marker, HostKindMarker::Known(kind) if kind != HostKind::Tmux)
}

#[cfg(test)]
#[path = "host_reconcile_tests.rs"]
mod tests;
