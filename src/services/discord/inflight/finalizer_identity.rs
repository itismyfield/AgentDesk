use std::fs;
use std::path::Path;

use super::InflightTurnState;
use super::store::{self, LockCause};
use crate::services::provider::ProviderKind;

pub(super) fn parse_inflight_state_content(content: &str) -> serde_json::Result<InflightTurnState> {
    parse_inflight_state_content_with_finalizer_backfill(content).map(|(state, _)| state)
}

pub(super) fn parse_inflight_state_content_with_finalizer_backfill(
    content: &str,
) -> serde_json::Result<(InflightTurnState, bool)> {
    let mut state = serde_json::from_str::<InflightTurnState>(content)?;
    if state.runtime_kind.is_none()
        && let Ok(raw_value) = serde_json::from_str::<serde_json::Value>(content)
        && let Some(raw_runtime) = raw_value.get("runtime_kind")
        && let Some(raw_str) = raw_runtime.as_str()
        && !raw_str.is_empty()
        && !matches!(
            raw_str,
            "legacy_tmux_wrapper" | "claude_tui" | "codex_tui" | "process_backend"
        )
    {
        state.runtime_kind_unknown_on_disk = true;
    }
    let finalizer_backfilled = state.ensure_finalizer_turn_id();
    Ok((state, finalizer_backfilled))
}

pub(super) fn read_inflight_state_content(path: &Path) -> Option<InflightTurnState> {
    let content = fs::read_to_string(path).ok()?;
    parse_inflight_state_content(&content).ok()
}

pub(super) fn backfill_finalizer_turn_id_under_lock(
    root: &Path,
    path: &Path,
    provider: &ProviderKind,
) -> Option<InflightTurnState> {
    backfill_finalizer_turn_id_observed(root, path, provider).state
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum BackfillOutcome {
    Unneeded,
    Foreign,
    ReceiptHeld,
    Persisted,
    Refused,
    Lock(LockCause),
    Read(String),
    Parse(String),
    Persist(String),
    Sync(String),
}

pub(super) struct BackfillReport {
    pub(super) state: Option<InflightTurnState>,
    pub(super) outcome: BackfillOutcome,
}

pub(super) fn backfill_finalizer_turn_id_observed(
    root: &Path,
    path: &Path,
    provider: &ProviderKind,
) -> BackfillReport {
    let failed = |outcome| BackfillReport {
        state: None,
        outcome,
    };
    let _lock = match store::lock_inflight_state_path_observed(path, false) {
        Ok(lock) => lock,
        Err((_, cause)) => return failed(BackfillOutcome::Lock(cause)),
    };
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => return failed(BackfillOutcome::Read(error.to_string())),
    };
    let (state, backfilled) = match parse_inflight_state_content_with_finalizer_backfill(&content) {
        Ok(parsed) => parsed,
        Err(error) => return failed(BackfillOutcome::Parse(error.to_string())),
    };
    let receipt_held = super::raw_before_start_receipt(&content);
    #[cfg(test)]
    let receipt_held = receipt_held && !report_tests::mutant("backfill-receipt");
    let outcome = if receipt_held {
        BackfillOutcome::ReceiptHeld
    } else if state.provider_kind().as_ref() != Some(provider) {
        BackfillOutcome::Foreign
    } else if backfilled {
        #[cfg(test)]
        report_tests::before_persist(path);
        match store::persist_under_lock_with_snapshot(
            root,
            path,
            &state,
            "src/services/discord/inflight/finalizer_identity.rs:backfill_finalizer_turn_id_under_lock",
        ) {
            Ok(Some(_)) => BackfillOutcome::Persisted,
            Ok(None) => BackfillOutcome::Refused,
            Err(error) => BackfillOutcome::Persist(error),
        }
    } else {
        BackfillOutcome::Unneeded
    };
    #[cfg(test)]
    let outcome = if (report_tests::mutant("backfill-none-success")
        && outcome == BackfillOutcome::Refused)
        || (report_tests::mutant("backfill-error-success")
            && matches!(outcome, BackfillOutcome::Persist(_)))
    {
        BackfillOutcome::Persisted
    } else {
        outcome
    };
    BackfillReport {
        state: Some(state),
        outcome,
    }
}

#[cfg(test)]
#[path = "finalizer_report_tests.rs"]
pub(super) mod report_tests;
