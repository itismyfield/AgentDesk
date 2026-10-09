//! Replay keeps native byte coordinates beside the original unit and piece identity.

use super::{Rotation, SourceId, SpoolFrame, StoreError, UnitDeriver};
use crate::services::tui_o::shadow::CapturedRecord;
use crate::services::tui_o::shadow::capture::same_file;
use crate::services::tui_o::store::ChannelStore;
use crate::services::tui_o::writer::pieces::OwedWork;
use crate::services::tui_prompt_dedupe::codex_verified::provenance::dormant::OwedOrigin;

pub(crate) fn origin(
    rotation: &Rotation,
    source: &SourceId,
    record: &CapturedRecord,
) -> Option<OwedOrigin> {
    if record.end <= record.start || record.end - record.start != record.line.len() as u64 + 1 {
        return None;
    }
    let mut matching = rotation.codex_spans.iter().filter(|span| {
        same_file(&span.execution.source, source)
            && record.start < span.end.unwrap_or(u64::MAX)
            && span.start < record.end
    });
    match (matching.next(), matching.next()) {
        (Some(span), None)
            if record.start >= span.start
                && span.end.is_some_and(|end| record.end <= end)
                && rotation.valid_codex_provenance(span.delivery_channel_id) =>
        {
            Some(OwedOrigin {
                span: span.clone(),
                range: record.start..record.end,
            })
        }
        _ => None,
    }
}

pub(crate) fn replay(
    store: &mut ChannelStore,
    source: &SourceId,
    deriver: &mut UnitDeriver,
) -> Result<Vec<OwedWork>, StoreError> {
    let rotation = store.rotation()?;
    let valid = rotation.valid_codex_provenance(store.init().channel);
    let mut work = Vec::new();
    store.for_each_frame(source, |frame| {
        if let SpoolFrame::Record(record) = frame {
            let origin = if valid {
                origin(&rotation, source, &record)
            } else {
                None
            };
            work.extend(deriver.derive(&record).into_iter().map(|derived| OwedWork {
                derived,
                origin: origin.clone(),
            }));
        }
    })?;
    Ok(work)
}
