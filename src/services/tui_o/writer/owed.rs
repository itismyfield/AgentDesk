//! Verified owed work: each item keeps the stored source and native range it came from, gains an
//! origin only from a fresh validated sidecar read, and posts only under its exact permit.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

use super::deliver::{ChannelWriter, Step};
use super::pieces::{Derived, OwedWork};
use super::rotation::Owe;
use super::rotation::provenance::origin;
use super::{AlarmSink, DeliveryLease, DiscordPort};
use crate::services::tui_o::shadow::identity::{RecordFact, classify};
use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider, SourceId, UnitKey};
use crate::services::tui_o::store::codex_provenance::Sidecar;
use crate::services::tui_o::store::ledger::{LedgerState, PieceOutcome};
use crate::services::tui_o::store::spool::{SpoolFrame, source_key};
use crate::services::tui_o::store::{ChannelStore, StoreError};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::admission;

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

/// What one re-evaluation read: no origin is kept unless the whole read succeeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reevaluation {
    Missing,
    Unavailable(&'static str),
    Present { attributed: usize },
}

#[cfg(test)]
thread_local! {
    /// Spooled records matched against owed work, one per record read.
    pub(crate) static PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Reads the sidecar, then each work's source spool once, matching records to work by their
/// original coordinates; origins change only after every source was read.
pub(crate) fn reevaluate(
    store: &mut ChannelStore,
    work: &mut [OwedWork],
) -> Result<Reevaluation, StoreError> {
    let snapshot = match store.codex_provenance() {
        Sidecar::Present(snapshot) => snapshot,
        Sidecar::Missing => return Ok(demote(work, Reevaluation::Missing)),
        Sidecar::Unavailable(reason) => return Ok(demote(work, Reevaluation::Unavailable(reason))),
    };
    // Sources in the order work first names them, each with its ranges indexed to work.
    type WorkByRange = HashMap<(u64, u64), Vec<usize>>;
    let mut by_source: Vec<(SourceId, WorkByRange)> = Vec::new();
    for (at, item) in work.iter().enumerate() {
        let slot = match by_source
            .iter()
            .position(|(source, _)| *source == item.source)
        {
            Some(slot) => slot,
            None => {
                by_source.push((item.source.clone(), HashMap::new()));
                by_source.len() - 1
            }
        };
        let range = (item.range.start, item.range.end);
        by_source[slot].1.entry(range).or_default().push(at);
    }
    let mut found = vec![None; work.len()];
    for (source, ranges) in &by_source {
        let scanned = store.for_each_frame(source, |frame| {
            let SpoolFrame::Record(record) = frame else {
                return;
            };
            #[cfg(test)]
            PROBES.with(|probes| probes.set(probes.get() + 1));
            for &at in ranges
                .get(&(record.start, record.end))
                .into_iter()
                .flatten()
            {
                found[at] = origin(&snapshot, source, &record);
            }
        });
        if let Err(error) = scanned {
            demote(work, Reevaluation::Missing);
            return Err(error);
        }
    }
    let attributed = found.iter().flatten().count();
    for (item, origin) in work.iter_mut().zip(found) {
        item.origin = origin;
    }
    Ok(Reevaluation::Present { attributed })
}

fn demote(work: &mut [OwedWork], outcome: Reevaluation) -> Reevaluation {
    work.iter_mut().for_each(|item| item.origin = None);
    outcome
}

fn unit_key(derived: &Derived) -> Option<&UnitKey> {
    match derived {
        Derived::Piece(piece) => Some(&piece.unit_key),
        Derived::Excluded { unit_key, .. } => Some(unit_key),
        Derived::Blocked { .. } => None,
    }
}

/// Only a recorded Posted or a legitimate Excluded settles work; any other outcome is kept debt.
pub(super) fn acked(ledger: &LedgerState, derived: &Derived) -> bool {
    match derived {
        Derived::Piece(piece) => {
            let latest = ledger.latest_piece(&piece.unit_key, piece.index);
            let posted =
                latest.is_some_and(|(_, p)| matches!(p.outcome, Some(PieceOutcome::Posted(_))));
            posted || ledger.excluded(&piece.unit_key).is_some()
        }
        Derived::Excluded { unit_key, .. } => ledger.excluded(unit_key).is_some(),
        Derived::Blocked { .. } => false,
    }
}

/// One FIFO pass. A piece posts only under its exact episode permit; work of an episode it
/// cannot identify stops the pass, and work behind held work of its episode or unit key waits.
pub(crate) async fn deliver_owed<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
    writer: &mut ChannelWriter<P, L, A>,
    log: &mut OwedLog,
) {
    if reevaluate(writer.store(), &mut log.work).is_err() {
        return;
    }
    let (mut held_units, mut held_episodes) = (HashSet::new(), HashSet::new());
    let mut at = 0;
    while at < log.work.len() {
        let work = &log.work[at];
        let unit = unit_key(&work.derived).cloned();
        let episode = (work.origin.as_ref()).map(|o| {
            (
                o.span.episode.user_message_id,
                o.span.episode.turn_nonce.clone(),
            )
        });
        let behind = unit.as_ref().is_some_and(|unit| held_units.contains(unit))
            || episode.as_ref().is_some_and(|e| held_episodes.contains(e));
        let record = match (&work.derived, &work.origin) {
            _ if behind => Err("held_behind_earlier_work"),
            (Derived::Piece(_), None) => return,
            (Derived::Piece(_), Some(origin)) => admission::lookup(origin).map(Some),
            _ => Ok(None),
        };
        let step = match &record {
            Ok(record) => {
                let admitted = record.as_deref().zip(work.origin.as_ref());
                writer.deliver_admitted(&work.derived, admitted).await
            }
            Err(_) => Step::Withheld,
        };
        match step {
            Step::Done if acked(writer.store().ledger(), &log.work[at].derived) => {
                log.work.remove(at);
                continue;
            }
            Step::Done | Step::Withheld => {}
            Step::LeaseBusy | Step::NoGateway | Step::Stopped => return,
        }
        held_units.extend(unit);
        held_episodes.extend(episode);
        at += 1;
    }
}

/// Spool keys still owed: each work's own source and every source its unit key was read from.
pub(crate) fn protected(log: &OwedLog) -> HashSet<String> {
    let mut keys: HashSet<String> = log.work.iter().map(|w| source_key(&w.source)).collect();
    for unit in log.work.iter().filter_map(|work| unit_key(&work.derived)) {
        let sources = log.associations.get(unit).into_iter().flatten();
        keys.extend(sources.map(|(source, _)| source_key(source)));
    }
    keys
}

#[cfg(all(test, unix))]
#[path = "owed_tests.rs"]
pub(crate) mod tests;
