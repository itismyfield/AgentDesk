//! Current binding targets use Bound order; Resolved only fills its original Pending.

use super::{BindingEvent, BindingRecord, BindingTarget, Logged, SourceId};

pub(super) enum Current {
    Known(SourceId),
    Unknown,
}

impl Current {
    pub(super) fn of(logged: Option<&Logged>, checkpoint: Option<u64>) -> Self {
        let Some(((raw, named), checkpoint)) = logged.zip(checkpoint) else {
            return Self::Unknown;
        };
        if raw
            .iter()
            .enumerate()
            .any(|(index, event)| event.seq != index as u64 + 1)
        {
            return Self::Unknown;
        }
        let mut current = None;
        for event in named {
            if !matches!(&event.record, BindingRecord::Bound { .. }) {
                continue;
            }
            match bound_target(raw, event, named) {
                BoundTarget::Source(source) => current = Some(source.clone()),
                BoundTarget::Waiting if event.seq > checkpoint => return Self::Unknown,
                BoundTarget::Skipped | BoundTarget::Waiting => {}
            }
        }
        current.map_or(Self::Unknown, Self::Known)
    }

    pub(super) fn historical(&self, source: &SourceId) -> bool {
        matches!(self, Self::Known(current) if current != source)
    }
}

pub(super) enum BoundTarget<'a> {
    Source(&'a SourceId),
    Skipped,
    Waiting,
}

/// Raw rows decide whether a Pending was dropped; named rows supply the stored target.
pub(super) fn bound_target<'a>(
    raw: &[BindingEvent],
    event: &'a BindingEvent,
    named: &'a [BindingEvent],
) -> BoundTarget<'a> {
    let BindingRecord::Bound { new, .. } = &event.record else {
        return BoundTarget::Skipped;
    };
    if let BindingTarget::Source(source) = new {
        return BoundTarget::Source(source);
    }
    let resolved = named.iter().rev().find_map(|later| match &later.record {
        BindingRecord::Resolved {
            resolves_seq,
            source,
        } if *resolves_seq == event.seq => Some(source),
        _ => None,
    });
    if let Some(source) = resolved {
        return BoundTarget::Source(source);
    }
    let skipped = raw
        .iter()
        .find(|row| row.seq == event.seq)
        .is_some_and(|pending| {
            raw.iter().any(|later| {
                super::super::adoption::supersedes(pending, later)
                    || matches!(&later.record, BindingRecord::Rejected { closes: Some(seq), .. }
                    if *seq == pending.seq && later.seq > pending.seq)
            })
        });
    if skipped {
        BoundTarget::Skipped
    } else {
        BoundTarget::Waiting
    }
}

#[cfg(test)]
#[path = "current_tests.rs"]
mod tests;
