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
    HostedExecution, HostedRecord, HostedState, LIVE, list_local_herdr_rows_pg,
};
use crate::services::discord::tmux::execution_identity::herdr_observation::HerdrExecutionMatch;
use crate::services::discord::tui_prompt_relay::herdr_source::{
    HerdrSourceAttach, attach_restarted_herdr_source,
};
use crate::services::provider::ProviderKind;
use crate::services::session_host::{HerdrPaneView, PaneProvider, PaneReading, herdr_endpoints};
use crate::services::tui_prompt_dedupe::herdr_execution_listed;

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
            view: herdr_endpoints().view(stored)?,
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

/// Per sessions row: whether a Codex pass read it, the execution it read and its result.
type Results = BTreeMap<i64, (bool, String, Reconnect)>;

/// The latest pass: its result per sessions row with the execution it read, and the input holds
/// it counted.
struct Pass {
    results: Results,
    holds: Option<Result<usize, String>>,
}

const NO_PASS: Pass = Pass {
    results: BTreeMap::new(),
    holds: None,
};

#[cfg(not(test))]
static RECONNECTS: std::sync::Mutex<Pass> = std::sync::Mutex::new(NO_PASS);

#[cfg(not(test))]
fn with_pass<R>(use_it: impl FnOnce(&mut Pass) -> R) -> R {
    use_it(
        &mut RECONNECTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

#[cfg(test)]
thread_local! {
    /// Tests see only the passes run on their own thread.
    static RECONNECTS: std::cell::RefCell<Pass> = const { std::cell::RefCell::new(NO_PASS) };
    /// Passes on this thread that got past the endpoint switch.
    pub(crate) static PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The in-process Herdr server, for tests that drive a pass through its callers.
#[cfg(test)]
pub(crate) use crate::services::session_host::herdr_socket_rig_tests::HerdrRig;

#[cfg(test)]
fn with_pass<R>(use_it: impl FnOnce(&mut Pass) -> R) -> R {
    RECONNECTS.with_borrow_mut(use_it)
}

/// Whether a Codex pass reads `row`; every other row, an unreadable one included, is the
/// Claude pass's.
fn codex_row(row: &HostedRecord) -> bool {
    matches!(row, HostedRecord::Known(record) if record.owner.provider == "codex")
}

/// After a restart, reconnects this node's Bound Herdr rows of `provider`'s pass; a settled one is
/// not read again. Without a local endpoint, or in the Codex pass off Herdr, it reads nothing.
pub(in crate::services::discord) fn reconnect_restarted_herdr_panes(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
) {
    let codex = *provider == ProviderKind::Codex;
    if codex && !crate::services::turn_host::herdr_turn_switched_on_for(provider) {
        return;
    }
    if herdr_endpoints().is_empty() {
        return;
    }
    #[cfg(test)]
    PASSES.with(|passes| passes.set(passes.get() + 1));
    // Counted here, off the async runtime, so a health request reads no hold files.
    let holds = crate::services::claude::herdr_turn::input_holds().map(|holds| holds.len());
    with_pass(|pass| pass.holds = Some(holds));
    let (Some(pool), Some(node), Ok(runtime)) = (
        pool,
        crate::config::session_hosts::local_node(),
        Handle::try_current(),
    ) else {
        return;
    };
    let rows = match runtime.block_on(list_local_herdr_rows_pg(pool, &node, LIVE)) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "herdr reconnect: rows unreadable");
            return;
        }
    };
    let before = with_pass(|pass| pass.results.clone());
    let mut after = BTreeMap::new();
    for row in rows.iter().filter(|row| codex_row(&row.record) == codex) {
        let record = match &row.record {
            HostedRecord::Known(record) => record,
            _ => {
                after.insert(row.session_id(), (codex, String::new(), Reconnect::Unknown));
                continue;
            }
        };
        let nonce = record.execution_nonce.clone();
        let seen = before
            .get(&row.session_id())
            .filter(|(_, seen, _)| *seen == nonce);
        let result = match seen.map(|(_, _, result)| *result) {
            Some(result) if result.settled() => result,
            // Only this pass's own refusal listed the pane; any other listing is this process's.
            Some(result) if result != Reconnect::Pending => {
                reconnect_unless_pending(&runtime, pool, record)
            }
            _ if herdr_execution_listed(&record.owner.logical_key) => continue,
            _ => reconnect_unless_pending(&runtime, pool, record),
        };
        after.insert(row.session_id(), (codex, nonce, result));
    }
    with_pass(|pass| {
        // The other pass's rows keep their results; a row gone from the listing is dropped.
        let listed = |id: &i64| rows.iter().any(|row| row.session_id() == *id);
        pass.results
            .retain(|id, (read_by_codex, ..)| *read_by_codex != codex && listed(id));
        pass.results.extend(after);
    });
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

/// The input holds the latest pass counted, `None` before any pass.
type HeldInputs = Option<Result<usize, String>>;

/// A node with a local endpoint: the latest pass's counts and held inputs, without file reads.
/// `None` without one.
pub(crate) fn local_reconnect_health() -> Option<(ReconnectCounts, HeldInputs)> {
    if herdr_endpoints().is_empty() {
        return None;
    }
    Some((reconnect_counts(), with_pass(|pass| pass.holds.clone())))
}

pub(crate) fn reconnect_counts() -> ReconnectCounts {
    let results: Vec<Reconnect> = with_pass(|pass| pass.results.values().map(|r| r.2).collect());
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
