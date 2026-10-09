//! Restart marking: every live row gets the restart marker so the next process re-attaches it.

use super::{
    InflightRestartMode, inflight_runtime_root, inflight_state_path,
    load_inflight_probe_from_root_excluding, set_inflight_restart_mode_under_lock,
};
use crate::services::discord::input_runtime::fence;
use crate::services::provider::ProviderKind;

/// One marking pass: rows marked, rows whose marker could not be written, and whether the scan
/// could not see every row.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct RestartMarkReport {
    pub(in crate::services::discord) marked: usize,
    pub(in crate::services::discord) failed: Vec<u64>,
    pub(in crate::services::discord) incomplete: bool,
}

/// Marks every readable row and reports the rest; one bad row never leaves later rows unmarked.
pub(super) fn mark_restart_mode_report(
    provider: &ProviderKind,
    restart_mode: InflightRestartMode,
) -> RestartMarkReport {
    let held = |channel| crate::services::turn_orchestrator::input_fence::held(provider, channel);
    let Some(root) = inflight_runtime_root() else {
        return RestartMarkReport {
            incomplete: true,
            ..RestartMarkReport::default()
        };
    };
    // The scan only discovers rows; each marker re-reads its row under the lock and changes only
    // the restart fields, so a frontier a draining watcher advanced meanwhile is kept.
    let load = load_inflight_probe_from_root_excluding(&root, provider, held);
    let mut report = RestartMarkReport {
        incomplete: !load.complete,
        ..RestartMarkReport::default()
    };
    for state in load.states {
        // A held input channel's row stays as its transition left it.
        if held(state.channel_id) {
            continue;
        }
        let path = inflight_state_path(&root, provider, state.channel_id);
        if set_inflight_restart_mode_under_lock(&path, restart_mode) {
            report.marked += 1;
        } else {
            report.failed.push(state.channel_id);
        }
    }
    report
}

/// Runs the marking pass on a blocking worker, where a protected channel's row writer is allowed.
pub(in crate::services::discord) async fn mark_restart_mode_blocking(
    provider: ProviderKind,
    restart_mode: InflightRestartMode,
) -> RestartMarkReport {
    let report = fence::effect::io({
        let provider = provider.clone();
        move || mark_restart_mode_report(&provider, restart_mode)
    })
    .await;
    for channel in &report.failed {
        fence::record_failure(&provider, *channel, &[], fence::Failure::Persistence);
    }
    // A row left unmarked is retired by the next boot, so a short pass is an error event even
    // when the caller goes on with its exit.
    if report.incomplete || !report.failed.is_empty() {
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
