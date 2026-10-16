//! Restart marking: every live row gets the restart marker so the next process re-attaches it.

use super::{
    InflightRestartMode, inflight_runtime_root, inflight_state_path,
    load_inflight_states_for_probe_from_root, set_inflight_restart_mode_under_lock,
};
use crate::services::provider::ProviderKind;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// One marking pass: rows marked, rows whose marker could not be written, rows left alone for
/// their policy receipt, and whether the scan could not see every row.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub(in crate::services::discord) struct RestartMarkReport {
    pub(in crate::services::discord) marked: usize,
    pub(in crate::services::discord) failed: Vec<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(in crate::services::discord) skipped_receipt: Vec<u64>,
    pub(in crate::services::discord) incomplete: bool,
}

/// One row's marking result; a receipt row is skipped, which is neither a mark nor a failure.
#[derive(Debug)]
pub(super) enum MarkOutcome {
    Marked,
    SkippedPolicyReceipt,
    Failed,
}

/// Marks every readable row and reports the rest; one bad row never leaves later rows unmarked.
pub(super) fn mark_restart_mode_report(
    provider: &ProviderKind,
    restart_mode: InflightRestartMode,
) -> RestartMarkReport {
    let Some(root) = inflight_runtime_root() else {
        return RestartMarkReport {
            incomplete: true,
            ..RestartMarkReport::default()
        };
    };
    // The scan only discovers rows; each marker re-reads its row under the lock and changes only
    // the restart fields, so a frontier a draining watcher advanced meanwhile is kept.
    let load = load_inflight_states_for_probe_from_root(&root, provider);
    let mut report = RestartMarkReport {
        incomplete: !load.complete,
        ..RestartMarkReport::default()
    };
    for state in load.states {
        let path = inflight_state_path(&root, provider, state.channel_id);
        match set_inflight_restart_mode_under_lock(&path, restart_mode) {
            MarkOutcome::Marked => report.marked += 1,
            MarkOutcome::SkippedPolicyReceipt => report.skipped_receipt.push(state.channel_id),
            MarkOutcome::Failed => report.failed.push(state.channel_id),
        }
    }
    report
}

/// Runs the filesystem marking pass on a blocking worker.
pub(in crate::services::discord) async fn mark_restart_mode_blocking(
    provider: ProviderKind,
    restart_mode: InflightRestartMode,
) -> RestartMarkReport {
    let owned = provider.clone();
    let report =
        match tokio::task::spawn_blocking(move || mark_restart_mode_report(&owned, restart_mode))
            .await
        {
            Ok(report) => report,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => panic!("restart marking worker cancelled: {error}"),
        };
    if !report.skipped_receipt.is_empty() {
        tracing::info!(
            provider = provider.as_str(),
            skipped = ?report.skipped_receipt,
            "restart marking left policy receipt rows unmarked"
        );
    }
    let short = report.incomplete || !report.failed.is_empty();
    publish_short_pass(&provider, short.then(|| report.clone()));
    // A row left unmarked is retired by the next boot, so a short pass is an error event even
    // when the caller goes on with its exit.
    if short {
        let root = inflight_runtime_root().map(|root| root.display().to_string());
        crate::services::observability::record_invariant_check(
            false,
            crate::services::observability::InvariantViolation {
                provider: Some(provider.as_str()),
                channel_id: None,
                dispatch_id: None,
                session_key: None,
                turn_id: None,
                invariant: RESTART_MARK_INVARIANT,
                code_location: "src/services/discord/inflight/restart_mark.rs:mark_restart_mode_blocking",
                message: "restart marking left inflight rows unmarked",
                details: serde_json::json!({
                    "marked": report.marked,
                    "failed": report.failed,
                    "incomplete": report.incomplete,
                    "root": root,
                }),
            },
        );
    }
    report
}

/// Invariant name of the event a short restart-marking pass emits.
const RESTART_MARK_INVARIANT: &str = "restart_marking_reaches_every_row";

/// Latest short pass per provider, read by the health snapshot; a complete pass clears it.
static SHORT_PASSES: Mutex<BTreeMap<String, RestartMarkReport>> = Mutex::new(BTreeMap::new());

/// A provider whose latest restart-marking pass left rows unmarked.
#[derive(Debug, Clone, serde::Serialize)]
pub(in crate::services::discord) struct ShortPass {
    provider: String,
    #[serde(flatten)]
    report: RestartMarkReport,
}

fn publish_short_pass(provider: &ProviderKind, pass: Option<RestartMarkReport>) {
    let mut passes = SHORT_PASSES.lock().unwrap_or_else(|e| e.into_inner());
    match pass {
        Some(report) => passes.insert(provider.as_str().to_string(), report),
        None => passes.remove(provider.as_str()),
    };
}

/// Every provider's latest short pass, for diagnostics only: neither a degraded reason nor a
/// restart condition, since marking runs during every deploy.
pub(in crate::services::discord) fn short_passes() -> Vec<ShortPass> {
    let passes = SHORT_PASSES.lock().unwrap_or_else(|e| e.into_inner());
    passes
        .iter()
        .map(|(provider, report)| ShortPass {
            provider: provider.clone(),
            report: report.clone(),
        })
        .collect()
}

#[cfg(test)]
static WRITE_FAULTS: Mutex<Vec<std::path::PathBuf>> = Mutex::new(Vec::new());

/// Fails one row's marker write after its lock is held, until dropped.
#[cfg(test)]
pub(in crate::services::discord) struct WriteFaultForTest(std::path::PathBuf);

#[cfg(test)]
impl WriteFaultForTest {
    pub(in crate::services::discord) fn new(path: std::path::PathBuf) -> Self {
        WRITE_FAULTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(path.clone());
        Self(path)
    }
}

#[cfg(test)]
impl Drop for WriteFaultForTest {
    fn drop(&mut self) {
        WRITE_FAULTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|path| path != &self.0);
    }
}

#[cfg(test)]
pub(super) fn write_fault_for_test(path: &std::path::Path) -> bool {
    WRITE_FAULTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|fault| fault == path)
}

/// Details of the latest short-pass event for this inflight root, if any.
#[cfg(test)]
pub(in crate::services::discord) fn short_pass_event_for_test() -> Option<serde_json::Value> {
    let root = inflight_runtime_root()?.display().to_string();
    crate::services::observability::events::recent(usize::MAX)
        .into_iter()
        .rev()
        .find(|event| {
            event.event_type == "invariant_violation"
                && event.payload["invariant"] == RESTART_MARK_INVARIANT
                && event.payload["details"]["root"] == root.as_str()
        })
        .map(|event| event.payload["details"].clone())
}

#[cfg(test)]
#[path = "restart_mark_tests.rs"]
mod tests;
