//! Restores applied binding relations without changing source boundaries or manufacturing proof.

use std::collections::BTreeMap;

use super::binding::{BindingEvent, BindingRecord, BindingTarget};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::store::rotation::{Rotation, Successor};
use crate::services::tui_o::store::spool::source_key;

pub(super) fn validate(
    events: &[BindingEvent],
    checkpoint: u64,
    channel: u64,
    provider: super::super::shadow::ShadowProvider,
) -> Result<(), super::WriterAlarm> {
    for (expected, event) in (1..).zip(events.iter().take_while(|e| e.seq <= checkpoint)) {
        if event.seq != expected {
            return Err(super::WriterAlarm::BindingGap {
                expected,
                found: event.seq,
            });
        }
        if event.channel_id != channel || event.provider != provider {
            return Err(super::WriterAlarm::Halted {
                detail: "historical binding names another channel or provider".into(),
            });
        }
    }
    Ok(())
}

pub(super) fn fold(
    events: &[BindingEvent],
    checkpoint: u64,
) -> BTreeMap<String, (&SourceId, Successor)> {
    let applied = &events[..events.partition_point(|event| event.seq <= checkpoint)];
    let mut hops = BTreeMap::new();
    for event in applied {
        let BindingRecord::Bound { old, new, .. } = &event.record else {
            continue;
        };
        let new = match new {
            BindingTarget::Source(source) => source,
            BindingTarget::Pending { .. } => {
                let resolved = applied.iter().find_map(|later| match &later.record {
                    BindingRecord::Resolved {
                        resolves_seq,
                        source,
                    } if *resolves_seq == event.seq => Some(source),
                    _ => None,
                });
                let Some(source) = resolved else { continue };
                source
            }
        };
        hops.remove(&source_key(new));
        if let Some(old) = old.as_ref().filter(|old| *old != new) {
            hops.insert(
                source_key(old),
                (
                    old,
                    Successor {
                        source: new.clone(),
                        seq: Some(event.seq),
                        tmux_session: Some(event.tmux_session.clone()),
                        drain_to: None,
                        proof: None,
                    },
                ),
            );
        }
    }
    hops
}

pub(super) fn restore(
    rotation: &mut Rotation,
    events: &[BindingEvent],
    checkpoint: u64,
    mut attached: impl FnMut(&SourceId) -> bool,
) -> bool {
    let mut changed = false;
    for (key, (old, mut next)) in fold(events, checkpoint) {
        if !rotation.successors.contains_key(&key) && attached(old) && attached(&next.source) {
            next.drain_to = std::fs::metadata(&old.path)
                .ok()
                .map(|metadata| metadata.len());
            rotation.successors.insert(key, next);
            changed = true;
        }
    }
    changed
}
