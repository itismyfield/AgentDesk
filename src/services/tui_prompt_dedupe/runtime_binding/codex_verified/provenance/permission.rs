//! Test-only native coordinates preserve historical proof and the store's cursor identity.

use super::ExecutionProofRef;
use crate::services::codex_tui::rollout_index::strict_parent_session;
use crate::services::tui_o::shadow::capture::{SourceCapture, same_file};
use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource, MAX_READ_BYTES, SourceId};
use crate::services::tui_o::store::spool::Cursor;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use crate::services::tui_prompt_dedupe::binding_events;
use crate::services::tui_prompt_dedupe::runtime_binding::codex_policy;

#[derive(Clone, Debug)]
pub(crate) struct HistoricalNativeIdentity {
    execution: ExecutionProofRef,
    cursor: Cursor,
}

impl HistoricalNativeIdentity {
    pub(crate) fn from_historical(
        context: &BindingContext,
        event: &binding_events::BindingEvent,
        cursors: &[Cursor],
    ) -> Result<Self, &'static str> {
        if context.schema != 1
            || context.source_policy.as_deref() != Some("verified")
            || !matches!(context.launch_mode.as_str(), "fresh" | "resume")
        {
            return Err("native_identity_context_unavailable");
        }
        let execution = codex_policy::historical_execution(context, event)?;
        if context
            .expected_native_session_id
            .as_ref()
            .is_some_and(|id| id != &execution.source.session_id)
        {
            return Err("native_identity_session_mismatch");
        }
        let mut matching = cursors
            .iter()
            .filter(|cursor| same_file(&cursor.source, &execution.source));
        let cursor = match (matching.next(), matching.next()) {
            (Some(cursor), None) => cursor.clone(),
            (None, _) => return Err("canonical_native_cursor_unavailable"),
            _ => return Err("canonical_native_cursor_ambiguous"),
        };
        strict_parent_session(&cursor.source.path, &cursor.source.session_id)
            .map_err(|_| "native_identity_session_mismatch")?;
        verify_checkpoint(&cursor.source, cursor.captured_through, &cursor.prefix_hash)?;
        Ok(Self { execution, cursor })
    }

    pub(crate) fn execution(&self) -> &ExecutionProofRef {
        &self.execution
    }

    pub(crate) fn canonical_source(&self) -> &SourceId {
        &self.cursor.source
    }

    pub(crate) fn canonical_cursor(&self) -> &Cursor {
        &self.cursor
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NativeSubmitFloor {
    execution: ExecutionProofRef,
    canonical_source: SourceId,
    through: u64,
    prefix_hash: String,
}

impl NativeSubmitFloor {
    /// Only a read of the proven native source creates this coordinate; rows cannot retag it.
    pub(crate) fn capture_before_submit(
        execution: &ExecutionProofRef,
        cursor: &Cursor,
    ) -> Result<Self, &'static str> {
        if execution.owner_runtime_root.trim().is_empty()
            || execution.tmux_session.trim().is_empty()
            || execution.execution_nonce.trim().is_empty()
            || execution.proof_seq == 0
            || !same_file(&execution.source, &cursor.source)
        {
            return Err("native_floor_execution_mismatch");
        }
        verify_checkpoint(&cursor.source, cursor.captured_through, &cursor.prefix_hash)?;
        strict_parent_session(&cursor.source.path, &cursor.source.session_id)
            .map_err(|_| "native_identity_session_mismatch")?;
        let empty_hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let mut capture = SourceCapture::reopen(cursor.source.clone(), 0, empty_hash)
            .map_err(|_| "native_floor_source_unavailable")?;
        let through = capture
            .file_len()
            .map_err(|_| "native_floor_source_unavailable")?;
        while capture.read_through() < through {
            let before = capture.read_through();
            if matches!(
                capture.poll((through - before).min(MAX_READ_BYTES)),
                CaptureOutcome::Anomaly(_)
            ) {
                return Err("native_floor_prefix_changed");
            }
            if capture.read_through() == before {
                return Err("native_floor_source_unavailable");
            }
        }
        if capture.captured_through() != through {
            return Err("incomplete_native_floor_record");
        }
        let prefix_hash = capture.prefix_hash();
        verify_checkpoint(&cursor.source, cursor.captured_through, &cursor.prefix_hash)?;
        verify_checkpoint(&cursor.source, through, &prefix_hash)?;
        Ok(Self {
            execution: execution.clone(),
            canonical_source: cursor.source.clone(),
            through,
            prefix_hash,
        })
    }

    pub(crate) fn verify_for(&self, execution: &ExecutionProofRef) -> Result<u64, &'static str> {
        if self.execution != *execution {
            return Err("native_floor_execution_mismatch");
        }
        verify_checkpoint(&self.canonical_source, self.through, &self.prefix_hash)?;
        Ok(self.through)
    }

    pub(crate) fn canonical_source(&self) -> &SourceId {
        &self.canonical_source
    }
}

fn verify_checkpoint(
    source: &SourceId,
    through: u64,
    prefix_hash: &str,
) -> Result<(), &'static str> {
    let mut capture = SourceCapture::reopen(source.clone(), through, prefix_hash)
        .map_err(|_| "native_floor_prefix_changed")?;
    if !capture
        .verify_prefix()
        .map_err(|_| "native_floor_source_unavailable")?
        || matches!(capture.poll(0), CaptureOutcome::Anomaly(_))
    {
        return Err("native_floor_prefix_changed");
    }
    Ok(())
}

#[cfg(test)]
#[path = "permission_tests.rs"]
mod tests;
