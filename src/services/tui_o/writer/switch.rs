//! Which channels begin the O era. A channel whose binding log binds none of its switch-time
//! sources would halt at its first poll, so it stays out of the era and is reported instead.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use super::binding::BindingEvents;
use super::rotation::binding_baseline;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::same_file;
use crate::services::tui_o::store::{Initialized, OEra, OStore, StoreError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Excluded {
    pub channel: u64,
    pub reason: String,
}

/// Seals the era over the channels whose binding log already names a switch-time source; a
/// sealed era is returned unchanged with nothing excluded.
pub fn begin_era_checked<B: BindingEvents>(
    store: &OStore,
    channels: &[u64],
    switch_at: DateTime<Utc>,
    bindings: &B,
    mut init_for: impl FnMut(u64) -> Result<Initialized, StoreError>,
) -> Result<(OEra, Vec<Excluded>), StoreError> {
    if let Some(era) = store.read_era()? {
        return Ok((era, Vec::new()));
    }
    let (mut inits, mut excluded) = (HashMap::new(), Vec::new());
    for &channel in channels {
        let init = match store.read_init(channel)? {
            Some(init) => init,
            None => init_for(channel)?,
        };
        match baseline_gap(bindings, channel, &init) {
            Some(reason) => excluded.push(Excluded { channel, reason }),
            None => {
                inits.insert(channel, init);
            }
        }
    }
    let included: Vec<u64> = channels
        .iter()
        .copied()
        .filter(|channel| inits.contains_key(channel))
        .collect();
    let era = store.begin_era(&included, switch_at, |channel| {
        let init = inits.remove(&channel);
        init.ok_or_else(|| StoreError::Rejected("channel was not checked".into()))
    })?;
    Ok((era, excluded))
}

/// Why the channel's first poll would find no binding baseline, if it would.
fn baseline_gap<B: BindingEvents>(
    bindings: &B,
    channel: u64,
    init: &Initialized,
) -> Option<String> {
    if init.sources.is_empty() {
        return Some("no source is attached at the switch".into());
    }
    let events = match bindings.binding_events_since(channel, 0) {
        Ok(events) => events,
        Err(detail) => return Some(format!("binding log unreadable: {detail}")),
    };
    let attached =
        |source: &SourceId| (init.sources.iter()).any(|s| same_file(&s.source_id, source));
    let baseline = binding_baseline(&events, attached);
    baseline
        .is_none()
        .then(|| "no binding event binds a source attached at the switch".into())
}
