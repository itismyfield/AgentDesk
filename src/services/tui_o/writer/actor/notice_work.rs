//! Notice debt is durable independently of raw retention and the FIFO Body path.

use super::super::input_notices::admission::{NoticeRefusal, admit_notice};
use super::super::pieces::PieceWork;
use super::*;
use crate::services::tui_o::shadow::{CapturedRecord, SourceId, UnitKind};
use crate::services::tui_o::store::ledger::input_notices::{
    MigrationHistory, MigrationSnapshot, MigrationSource, NoticeEntry, NoticeEvidence,
};
use crate::services::tui_o::store::ledger::{LedgerEntry, LedgerState};
use sha2::Digest;

fn notice(item: &Derived) -> Option<&PieceWork> {
    match item {
        Derived::Piece(piece) if piece.unit_key.kind == UnitKind::InputNotice => Some(piece),
        _ => None,
    }
}
fn settled(ledger: &LedgerState, key: &UnitKey) -> bool {
    ledger.excluded(key).is_some()
        || ledger.latest_piece(key, 0).is_some_and(|(_, record)| {
            matches!(
                record.outcome,
                Some(crate::services::tui_o::store::ledger::PieceOutcome::Posted(
                    _
                ))
            )
        })
}
fn covers(kept: &NoticeEvidence, next: &NoticeEvidence) -> bool {
    kept.raw_record == next.raw_record
        && next
            .prompt
            .as_ref()
            .is_none_or(|v| kept.prompt.as_ref() == Some(v))
        && next
            .prompt_sha256
            .as_ref()
            .is_none_or(|v| kept.prompt_sha256.as_ref() == Some(v))
        && next
            .unit_key
            .as_ref()
            .is_none_or(|v| kept.unit_key.as_ref() == Some(v))
        && next
            .payload
            .as_ref()
            .is_none_or(|v| kept.payload.as_ref() == Some(v))
        && next
            .origin
            .as_ref()
            .is_none_or(|v| kept.origin.as_ref() == Some(v))
}
/// The newest Discord ID a message created at `at` can carry, so later notices fall above it.
fn snowflake_ceiling(at: chrono::DateTime<chrono::Utc>) -> u64 {
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let ms = u64::try_from(at.timestamp_millis() - DISCORD_EPOCH_MS).unwrap_or(0);
    (ms << 22) | ((1 << 22) - 1)
}
/// Every cursor and source bound past the checkpoint, live ones to their file end and retired ones
/// only to their cursor so a replayed spool stays below the floor, with the legacy history bounds.
fn frozen_cohort(
    store: &crate::services::tui_o::store::ChannelStore,
    bound: &[super::super::binding::BindingEvent],
    now: chrono::DateTime<chrono::Utc>,
) -> Result<MigrationSnapshot, String> {
    use super::super::binding::{BindingRecord, BindingTarget};
    let checkpoint = (store.binding_checkpoint())
        .map_err(|error| format!("input notice migration checkpoint: {error:?}"))?
        .unwrap_or(0);
    let mut frozen: Vec<(SourceId, u64, String, bool)> = (store.cursors())
        .map(|cursor| {
            let at = (cursor.captured_through, cursor.prefix_hash.clone());
            (cursor.source.clone(), at.0, at.1, cursor.retired)
        })
        .collect();
    // A source bound before the freeze but not attached yet starts where its attach will start.
    for event in bound {
        let source = match &event.record {
            BindingRecord::Bound {
                new: BindingTarget::Source(source),
                ..
            }
            | BindingRecord::Resolved { source, .. } => source,
            _ => continue,
        };
        if frozen.iter().any(|(known, ..)| known == source) {
            continue;
        }
        frozen.push((
            source.clone(),
            0,
            hex::encode(sha2::Sha256::digest(b"")),
            false,
        ));
    }
    let mut sources = Vec::new();
    for (source, lower, prefix_hash, retired) in frozen {
        let upper = if retired {
            lower
        } else {
            match std::fs::metadata(&source.path) {
                Ok(meta) => meta.len().max(lower),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => lower,
                Err(error) => return Err(format!("input notice migration source: {error}")),
            }
        };
        sources.push(MigrationSource {
            source,
            lower,
            upper,
            prefix_hash,
        });
    }
    // Legacy hook notices could precede their native row by a whole queued turn, so only the
    // era's first anchor bounds them from below.
    let lower = store.init().initial_anchor;
    Ok(MigrationSnapshot {
        sources,
        binding_checkpoint: bound.last().map_or(checkpoint, |event| event.seq),
        history: MigrationHistory::Fixed {
            lower,
            upper: snowflake_ceiling(now).max(lower),
        },
    })
}
impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents> Actor<P, L, A, B> {
    /// Freezes the legacy cohort once, before resume can capture past the old cursors; `false`
    /// holds resume, until a later poll reads the binding log or for good once the writer stopped.
    pub(super) fn start_notice_migration(&mut self) -> bool {
        if !self.deriver.input_notices_enabled()
            || self.writer.is_stopped()
            || self
                .writer
                .store()
                .ledger()
                .input_notices()
                .migration
                .is_some()
        {
            return !self.writer.is_stopped();
        }
        let now = self.utc_now();
        let checkpoint = self.writer.store().binding_checkpoint().ok().flatten();
        let channel = self.writer.channel();
        let bound = match self
            .bindings
            .binding_events_since(channel, checkpoint.unwrap_or(0))
        {
            Ok(bound) => bound,
            Err(detail) => {
                let alarm = WriterAlarm::BindingLogUnavailable { checkpoint, detail };
                self.writer.alarm(alarm);
                return false;
            }
        };
        let snapshot = match frozen_cohort(self.writer.store(), &bound, now) {
            Ok(snapshot) => snapshot,
            Err(detail) => {
                self.writer.stop(WriterAlarm::Halted { detail });
                return false;
            }
        };
        let entry = Box::new(NoticeEntry::MigrationStarted { snapshot });
        self.writer
            .record(LedgerEntry::InputNotice { entry })
            .is_ok()
    }

    pub(super) fn persist_notice_evidence(&mut self) -> Vec<UnitKey> {
        if !self.deriver.input_notices_enabled() || self.writer.is_stopped() {
            return Vec::new();
        }
        let mut fresh = Vec::new();
        for evidence in self.deriver.take_notice_evidence() {
            if super::super::input_notices::evidence::before_migration_floor(
                self.writer.store().ledger().input_notices(),
                &evidence,
            ) {
                continue;
            }
            if let Some(key) = &evidence.unit_key {
                fresh.push(key.clone());
            }
            let id = (
                evidence.record.source.clone(),
                evidence.record.start,
                evidence.record.end,
            );
            if self
                .writer
                .store()
                .ledger()
                .input_notices()
                .evidence
                .get(&id)
                .is_some_and(|kept| covers(kept, &evidence))
            {
                continue;
            }
            if self
                .writer
                .record(LedgerEntry::InputNotice {
                    entry: Box::new(NoticeEntry::Materialized {
                        evidence: Box::new(evidence),
                    }),
                })
                .is_err()
            {
                break;
            }
        }
        fresh
    }

    pub(super) async fn prepare_notices(&mut self, fresh: &[UnitKey]) {
        if !self.deriver.input_notices_enabled() || self.writer.is_stopped() {
            return;
        }
        let mut pending: Vec<_> = self
            .owed
            .iter()
            .filter_map(notice)
            .filter(|p| !fresh.contains(&p.unit_key))
            .map(|p| p.unit_key.clone())
            .collect();
        let ledger = self.writer.store().ledger();
        let state = ledger.input_notices();
        for key in state.evidence.values().filter_map(|e| e.unit_key.as_ref()) {
            if ledger.latest_piece(key, 0).is_some() && !pending.contains(key) {
                pending.push(key.clone());
            }
        }
        let evidence: Vec<_> = state
            .evidence
            .iter()
            .filter(|(id, e)| {
                !state.unresolved.contains_key(*id)
                    && e.unit_key.as_ref().is_none_or(|k| !settled(ledger, k))
            })
            .map(|(_, e)| e.clone())
            .collect();
        let (mut eligible, mut deferred) = (Vec::new(), Vec::new());
        for mut evidence in evidence {
            let record = CapturedRecord {
                start: evidence.record.start,
                end: evidence.record.end,
                line: evidence.raw_record.clone(),
            };
            if evidence.origin.is_none()
                && let Ok(origin) = self.sources.notice_origin(&evidence.record.source, &record)
            {
                evidence.origin = Some(Box::new(origin));
                if self
                    .writer
                    .record(LedgerEntry::InputNotice {
                        entry: Box::new(NoticeEntry::Materialized {
                            evidence: Box::new(evidence.clone()),
                        }),
                    })
                    .is_err()
                {
                    return;
                }
            }
            let (Some(unit_key), Some(payload), Some(origin)) =
                (evidence.unit_key, evidence.payload, evidence.origin)
            else {
                continue;
            };
            let piece = PieceWork {
                output_origin: None,
                unit_key,
                payload,
                index: 0,
                notice_origin: Some(origin),
            };
            match admit_notice(&piece, self.writer.store(), |_| ()) {
                Ok(()) => eligible.push(piece),
                Err(NoticeRefusal::Store(detail)) => {
                    self.writer.stop(WriterAlarm::Halted { detail });
                    return;
                }
                Err(NoticeRefusal::Violation(detail)) => {
                    self.writer.stop(WriterAlarm::LedgerViolation { detail });
                    return;
                }
                Err(NoticeRefusal::OwnershipPending) => deferred.push(piece.unit_key),
                Err(NoticeRefusal::Busy | NoticeRefusal::Withheld(_)) => {}
            }
        }

        let complete = {
            let store = self.writer.store();
            store
                .ledger()
                .input_notices()
                .migration
                .as_ref()
                .is_some_and(|snapshot| {
                    snapshot.sources.iter().all(|s| {
                        store
                            .cursor(&s.source)
                            .is_some_and(|c| c.captured_through >= s.upper)
                    })
                })
        };
        eligible.sort_by_key(|piece| {
            piece
                .notice_origin
                .as_ref()
                .map(|origin| (origin.binding_seq, origin.record.start))
        });
        let keys: Vec<_> = eligible.iter().map(|p| p.unit_key.clone()).collect();
        let Ok(ready) = self
            .writer
            .reconcile_notice_migration(complete, &pending, &keys, &deferred)
            .await
        else {
            return;
        };
        let ledger = self.writer.store().ledger();
        self.owed.retain(|item| {
            notice(item)
                .is_none_or(|p| ready.contains(&p.unit_key) && !settled(ledger, &p.unit_key))
        });
        let mut queued: HashSet<_> = self
            .owed
            .iter()
            .filter_map(notice)
            .map(|p| p.unit_key.clone())
            .collect();
        for piece in eligible {
            if ready.contains(&piece.unit_key)
                && !settled(ledger, &piece.unit_key)
                && queued.insert(piece.unit_key.clone())
            {
                self.owed.push_back(Derived::Piece(piece));
            }
        }
    }

    pub(super) fn notice_debt_extra(&mut self) -> usize {
        self.notice_debt(true)
    }

    /// The debt a delivery can still settle; a migration verdict stays audited in the ledger only,
    /// so it neither reads as a delivery stall nor holds a source the verdict no longer needs.
    pub(super) fn notice_debt_open(&mut self) -> usize {
        self.notice_debt(false)
    }

    fn notice_debt(&mut self, migration: bool) -> usize {
        if !self.deriver.input_notices_enabled() {
            return 0;
        }
        let queued: HashSet<_> = self
            .owed
            .iter()
            .filter_map(notice)
            .map(|p| p.unit_key.clone())
            .collect();
        let ledger = self.writer.store().ledger();
        let mut counted = queued;
        let state = ledger.input_notices();
        (state.evidence.iter())
            .filter(|(id, _)| migration || !state.unresolved.contains_key(*id))
            .map(|(_, e)| e)
            .filter(|e| match &e.unit_key {
                Some(key) => !settled(ledger, key) && counted.insert(key.clone()),
                None => true,
            })
            .count()
    }

    pub(super) fn notice_raw_independent(&mut self) -> bool {
        if !self.deriver.input_notices_enabled() {
            return self.owed.is_empty();
        }
        let ledger = self.writer.store().ledger();
        self.owed.iter().all(|item| {
            notice(item).is_some_and(|piece| {
                piece.notice_origin.as_ref().is_some_and(|origin| {
                    let id = (
                        origin.record.source.clone(),
                        origin.record.start,
                        origin.record.end,
                    );
                    ledger.input_notices().evidence.get(&id).is_some_and(|e| {
                        e.unit_key.as_ref() == Some(&piece.unit_key)
                            && e.payload.as_ref() == Some(&piece.payload)
                            && e.origin.as_ref() == Some(origin)
                    })
                })
            })
        })
    }

    pub(super) fn notice_only_owed(&self) -> bool {
        if !self.deriver.input_notices_enabled() {
            return self.owed.is_empty();
        }
        self.owed.iter().all(|item| notice(item).is_some())
    }
}

#[cfg(all(test, unix))]
#[path = "notice_work_tests.rs"]
mod tests;
