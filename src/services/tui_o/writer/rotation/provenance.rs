//! A record's origin is read only from a validated sidecar snapshot, never from the boundary.

use super::SourceId;
use crate::services::tui_o::shadow::CapturedRecord;
use crate::services::tui_o::shadow::capture::same_file;
use crate::services::tui_o::store::codex_provenance::CodexProvenance;
use crate::services::tui_prompt_dedupe::codex_verified::provenance::dormant::OwedOrigin;

/// One complete record inside the only span on its file that its own terminal closed.
pub(crate) fn origin(
    snapshot: &CodexProvenance,
    source: &SourceId,
    record: &CapturedRecord,
) -> Option<OwedOrigin> {
    if record.end <= record.start || record.end - record.start != record.line.len() as u64 + 1 {
        return None;
    }
    let mut matching = snapshot.spans.iter().filter(|span| {
        same_file(&span.execution.source, source)
            && record.start < span.end.unwrap_or(u64::MAX)
            && span.start < record.end
    });
    match (matching.next(), matching.next()) {
        (Some(span), None)
            if record.start >= span.start && span.end.is_some_and(|end| record.end <= end) =>
        {
            Some(OwedOrigin {
                span: span.clone(),
                range: record.start..record.end,
            })
        }
        _ => None,
    }
}
