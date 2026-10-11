//! Test-only verified work: each owed item keeps the stored source and native range it came
//! from, and gains an origin only from a fresh read of the durable sidecar.

use std::collections::BTreeMap;
use std::ops::Range;

use super::pieces::{Derived, OwedWork};
use super::rotation::Owe;
use super::rotation::provenance::origin;
use crate::services::tui_o::shadow::identity::{RecordFact, classify};
use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider, SourceId, UnitKey};
use crate::services::tui_o::store::codex_provenance::Sidecar;
use crate::services::tui_o::store::spool::SpoolFrame;
use crate::services::tui_o::store::{ChannelStore, StoreError};

pub(crate) struct OwedLog {
    channel: u64,
    provider: ShadowProvider,
    pub work: Vec<OwedWork>,
    /// Every source and range a unit key was owed from, a repeat that derived nothing included.
    pub associations: BTreeMap<UnitKey, Vec<(SourceId, Range<u64>)>>,
}

impl OwedLog {
    pub(crate) fn new(channel: u64, provider: ShadowProvider) -> Self {
        let (work, associations) = (Vec::new(), BTreeMap::new());
        Self {
            channel,
            provider,
            work,
            associations,
        }
    }
}

impl Owe for OwedLog {
    fn owe(&mut self, source: &SourceId, record: &CapturedRecord, derived: Vec<Derived>) {
        let range = record.start..record.end;
        let value = serde_json::from_slice(&record.line).unwrap_or_default();
        for fact in classify(self.provider, &value) {
            if let RecordFact::Unit(native_key, kind, _) = fact {
                let (channel_id, provider) = (self.channel, self.provider);
                let key = UnitKey {
                    channel_id,
                    provider,
                    native_key,
                    kind,
                };
                let association = (source.clone(), range.clone());
                self.associations.entry(key).or_default().push(association);
            }
        }
        self.work
            .extend(derived.into_iter().map(|derived| OwedWork {
                derived,
                source: source.clone(),
                range: range.clone(),
                origin: None,
            }));
    }
}

/// Re-reads the sidecar and each work's spooled record at its original coordinates; a sidecar
/// that is not Present leaves every origin unset.
pub(crate) fn reevaluate(
    store: &mut ChannelStore,
    work: &mut [OwedWork],
) -> Result<(), StoreError> {
    work.iter_mut().for_each(|item| item.origin = None);
    let Sidecar::Present(snapshot) = store.codex_provenance() else {
        return Ok(());
    };
    let mut sources: Vec<SourceId> = Vec::new();
    for item in work.iter() {
        if !sources.contains(&item.source) {
            sources.push(item.source.clone());
        }
    }
    for source in sources {
        store.for_each_frame(&source, |frame| {
            let SpoolFrame::Record(record) = frame else {
                return;
            };
            let range = record.start..record.end;
            let at = |item: &&mut OwedWork| item.source == source && item.range == range;
            for item in work.iter_mut().filter(at) {
                item.origin = origin(&snapshot, &source, &record);
            }
        })?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "owed_tests.rs"]
pub(crate) mod tests;
