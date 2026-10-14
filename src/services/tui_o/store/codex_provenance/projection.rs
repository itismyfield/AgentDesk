//! Owned, read-only evidence retains the canonical cursor independently of submit floors.

use super::submit_floor::NativeFloor;
use super::{ChannelStore, Sidecar, ValidatedCodexProvenance};
use crate::services::tui_o::shadow::capture::{SourceCapture, same_file};
use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource};
use crate::services::tui_o::store::spool::Cursor;
use crate::services::tui_prompt_dedupe::codex_verified::provenance::decisions::{
    CheckpointReason, PersistenceReason as P,
};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::{
    CodexEpisodeSpan, ExecutionProofRef,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReadBackProjection {
    execution: ExecutionProofRef,
    cursor: Cursor,
    evidence: ValidatedCodexProvenance,
}

impl ReadBackProjection {
    pub(crate) fn execution(&self) -> &ExecutionProofRef {
        &self.execution
    }
    pub(crate) fn cursor(&self) -> &Cursor {
        &self.cursor
    }
    pub(crate) fn evidence(&self) -> &ValidatedCodexProvenance {
        &self.evidence
    }
    pub(crate) fn checkpoint_reason(&self) -> CheckpointReason {
        if self.evidence.floors().iter().any(|floor| matches!(floor,
            super::submit_floor::SubmitFloor::NativeFloor(native)
            if native.execution() == &self.execution && native.checkpoint().1 > self.cursor.captured_through))
        { CheckpointReason::FloorAheadOfCursor }
        else { CheckpointReason::NoProcessedWitness }
    }
}

pub(crate) fn floor_names(floor: &NativeFloor, span: &CodexEpisodeSpan) -> bool {
    floor.names(span)
}

fn verify(
    source: &crate::services::tui_o::shadow::SourceId,
    through: u64,
    hash: &str,
) -> Result<(), P> {
    let mut capture = SourceCapture::reopen(source.clone(), through, hash).map_err(|error| {
        if error.kind() == std::io::ErrorKind::InvalidData {
            P::PrefixChanged
        } else {
            P::SourceChanged
        }
    })?;
    if !capture.verify_prefix().map_err(|_| P::SourceChanged)?
        || matches!(capture.poll(0), CaptureOutcome::Anomaly(_))
    {
        return Err(P::PrefixChanged);
    }
    Ok(())
}

impl ChannelStore {
    pub(crate) fn codex_read_back(
        &self,
        execution: &ExecutionProofRef,
    ) -> Result<ReadBackProjection, P> {
        if self.io_failure().is_some() {
            return Err(P::StorageFailed);
        }
        let evidence = match self.codex_provenance() {
            Sidecar::Present(read) => read,
            Sidecar::Missing => return Err(P::MissingEvidence),
            Sidecar::Unavailable(_) => return Err(P::ReadBackMismatch),
        };
        let names = evidence.spans().iter().any(|span| &span.execution == execution)
            || evidence.denies().iter().any(|deny| &deny.execution == execution)
            || evidence.floors().iter().any(|floor| matches!(floor,
                super::submit_floor::SubmitFloor::NativeFloor(native) if native.execution() == execution));
        if !names {
            return Err(P::IdentityMismatch);
        }
        let mut cursors = self
            .cursors()
            .filter(|cursor| same_file(&cursor.source, &execution.source));
        let cursor = match (cursors.next(), cursors.next()) {
            (Some(cursor), None) => cursor.clone(),
            (None, _) => return Err(P::MissingEvidence),
            _ => return Err(P::IdentityMismatch),
        };
        verify(&cursor.source, cursor.captured_through, &cursor.prefix_hash)?;
        for floor in evidence.floors() {
            if let super::submit_floor::SubmitFloor::NativeFloor(native) = floor
                && native.execution() == execution
            {
                let (source, through, hash) = native.checkpoint();
                if !same_file(source, &cursor.source) {
                    return Err(P::IdentityMismatch);
                }
                verify(source, through, hash)?;
            }
        }
        Ok(ReadBackProjection {
            execution: execution.clone(),
            cursor,
            evidence,
        })
    }
}
