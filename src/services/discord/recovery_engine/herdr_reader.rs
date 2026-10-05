//! The production restart reader of a Herdr pane and the restart pass over this node's Herdr rows.
//! Every reading goes through the pane's gated target on one server; nothing here writes a pane.

use std::collections::BTreeMap;

use sqlx::PgPool;
use tokio::runtime::Handle;

use super::host_reconcile::{
    HerdrEndpointId, HerdrExecutionReader, HerdrPaneEvidence, HerdrPaneReading, HostReconcile,
    NoHerdrEndpoint,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedRecord, HostedState, list_local_herdr_rows_pg,
};
use crate::services::discord::tmux::execution_identity::herdr_observation::HerdrExecutionMatch;
use crate::services::discord::tui_prompt_relay::herdr_source::{
    HerdrSourceAttach, attach_restarted_herdr_source,
};
use crate::services::session_host::{HerdrTarget, PaneProvider, PaneReading, herdr_endpoints};
use crate::services::tui_prompt_dedupe::herdr_execution_listed;

/// Reads the stored pane on the endpoint this node registered for it; it never lists, searches,
/// creates or writes panes.
pub(crate) struct SocketHerdrReader {
    endpoint: HerdrEndpointId,
    target: HerdrTarget,
    nonce: String,
}

impl SocketHerdrReader {
    /// `None` when no registered endpoint holds the record's location.
    pub(crate) fn of(stored: &HostedExecution) -> Option<Self> {
        let location = stored.location.as_ref()?;
        Some(Self {
            endpoint: HerdrEndpointId::of(location),
            target: herdr_endpoints().target(stored)?,
            nonce: stored.execution_nonce.clone(),
        })
    }
}

impl HerdrExecutionReader for SocketHerdrReader {
    fn endpoint(&self) -> Option<&HerdrEndpointId> {
        Some(&self.endpoint)
    }

    fn read_pane(&self, pane_id: &str) -> HerdrPaneReading {
        if pane_id != self.target.pane_id() {
            return HerdrPaneReading::Unreadable("not the target's pane".into());
        }
        let (root, provider) = match self.target.read_execution() {
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

/// One row's result in the restart pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reconnect {
    /// The source was restored, or the execution's own clear admits input.
    Published,
    /// Refused on what was read; tried again only for a new execution.
    Refused,
    /// Refused for now; read again on the next pass.
    Withheld,
    Unknown,
    /// Left to its next turn; nothing is launched or attached here.
    Pending,
}

impl Reconnect {
    fn of(attach: &HerdrSourceAttach) -> Self {
        use HerdrExecutionMatch::{Mismatch, Unknown};
        match attach {
            HerdrSourceAttach::Published { .. } | HerdrSourceAttach::AwaitingClear => {
                Self::Published
            }
            HerdrSourceAttach::Refused(HostReconcile::Herdr(Mismatch(_)))
            | HerdrSourceAttach::Refused(HostReconcile::Missing) => Self::Refused,
            HerdrSourceAttach::Refused(HostReconcile::Herdr(Unknown(_)))
            | HerdrSourceAttach::Refused(HostReconcile::Unresolved(_)) => Self::Unknown,
            _ => Self::Withheld,
        }
    }

    fn settled(self) -> bool {
        matches!(self, Self::Published | Self::Refused)
    }
}

type Results = BTreeMap<i64, (String, Reconnect)>;

/// The latest pass's result per sessions row, with the execution it read.
#[cfg(not(test))]
static RECONNECTS: std::sync::Mutex<Results> = std::sync::Mutex::new(BTreeMap::new());

#[cfg(not(test))]
fn with_results<R>(use_them: impl FnOnce(&mut Results) -> R) -> R {
    use_them(
        &mut RECONNECTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

#[cfg(test)]
thread_local! {
    /// Tests see only the passes run on their own thread.
    static RECONNECTS: std::cell::RefCell<Results> = const { std::cell::RefCell::new(BTreeMap::new()) };
    /// Passes on this thread that got past the endpoint switch.
    pub(crate) static PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The in-process Herdr server, for tests that drive a pass through its callers.
#[cfg(test)]
pub(crate) use crate::services::session_host::herdr_socket_rig_tests::HerdrRig;

#[cfg(test)]
fn with_results<R>(use_them: impl FnOnce(&mut Results) -> R) -> R {
    RECONNECTS.with_borrow_mut(use_them)
}

/// Reconnects this node's Bound Herdr executions after a restart, each through its own reader;
/// a settled one is not read again. Without a local endpoint it reads nothing.
pub(in crate::services::discord) fn reconnect_restarted_herdr_panes(pool: Option<&PgPool>) {
    if herdr_endpoints().is_empty() {
        return;
    }
    #[cfg(test)]
    PASSES.with(|passes| passes.set(passes.get() + 1));
    let (Some(pool), Some(node), Ok(runtime)) = (
        pool,
        crate::config::session_hosts::local_node(),
        Handle::try_current(),
    ) else {
        return;
    };
    let rows = match runtime.block_on(list_local_herdr_rows_pg(pool, &node)) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "herdr reconnect: rows unreadable");
            return;
        }
    };
    let before = with_results(|results| results.clone());
    let mut after = BTreeMap::new();
    for row in rows {
        let record = match &row.record {
            HostedRecord::Known(record) => record,
            _ => {
                after.insert(row.session_id(), (String::new(), Reconnect::Unknown));
                continue;
            }
        };
        let nonce = record.execution_nonce.clone();
        let seen = before
            .get(&row.session_id())
            .filter(|(seen, _)| *seen == nonce);
        let result = match seen.map(|(_, result)| *result) {
            Some(result) if result.settled() => result,
            // Only this pass's own refusal listed the pane; any other listing is this process's.
            Some(result) if result != Reconnect::Pending => {
                reconnect_unless_pending(&runtime, pool, record)
            }
            _ if herdr_execution_listed(&record.owner.logical_key) => continue,
            _ => reconnect_unless_pending(&runtime, pool, record),
        };
        after.insert(row.session_id(), (nonce, result));
    }
    with_results(|results| *results = after);
}

/// A Pending execution is left to its next turn: nothing is launched or attached for it here.
fn reconnect_unless_pending(
    runtime: &Handle,
    pool: &PgPool,
    record: &HostedExecution,
) -> Reconnect {
    if record.state == HostedState::Pending {
        return Reconnect::Pending;
    }
    Reconnect::of(&runtime.block_on(reconnect(pool, record)))
}

async fn reconnect(pool: &PgPool, record: &HostedExecution) -> HerdrSourceAttach {
    let Ok(channel) = record.owner.channel_id.parse::<u64>() else {
        return HerdrSourceAttach::Refused(HostReconcile::Unresolved("channel id".into()));
    };
    let attached = match SocketHerdrReader::of(record) {
        Some(reader) => attach_restarted_herdr_source(pool, &record.owner, channel, &reader).await,
        None => attach_restarted_herdr_source(pool, &record.owner, channel, &NoHerdrEndpoint).await,
    };
    tracing::info!(channel, ?attached, "herdr reconnect after restart");
    attached
}

/// The latest pass's counts: rows read, then published, refused or withheld, unknown and pending.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ReconnectCounts {
    pub channels: usize,
    pub published: usize,
    pub withheld: usize,
    pub unknown: usize,
    pub pending: usize,
}

/// A node with a local endpoint: the latest pass's counts and how many inputs are held. `None`
/// without one, which reads nothing.
pub(crate) fn local_reconnect_health() -> Option<(ReconnectCounts, Result<usize, String>)> {
    if herdr_endpoints().is_empty() {
        return None;
    }
    let holds = crate::services::claude::herdr_turn::input_holds().map(|holds| holds.len());
    Some((reconnect_counts(), holds))
}

pub(crate) fn reconnect_counts() -> ReconnectCounts {
    let results: Vec<Reconnect> = with_results(|results| results.values().map(|r| r.1).collect());
    let mut counts = ReconnectCounts {
        channels: results.len(),
        ..ReconnectCounts::default()
    };
    for result in results {
        match result {
            Reconnect::Published => counts.published += 1,
            Reconnect::Refused | Reconnect::Withheld => counts.withheld += 1,
            Reconnect::Unknown => counts.unknown += 1,
            Reconnect::Pending => counts.pending += 1,
        }
    }
    counts
}

#[cfg(test)]
#[path = "herdr_reader_tests.rs"]
mod tests;
