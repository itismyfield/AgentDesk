//! Notice debt is durable independently of raw retention and the FIFO Body path.

use super::super::input_notices::admission::{NoticeRefusal, admit_notice};
use super::super::pieces::PieceWork;
use super::*;
use crate::services::tui_o::shadow::{CapturedRecord, UnitKind};
use crate::services::tui_o::store::ledger::input_notices::{NoticeEntry, NoticeEvidence};
use crate::services::tui_o::store::ledger::{LedgerEntry, LedgerState};

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
impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents> Actor<P, L, A, B> {
    pub(super) fn persist_notice_evidence(&mut self) -> Vec<UnitKey> {
        if !self.deriver.input_notices_enabled() || self.writer.is_stopped() {
            return Vec::new();
        }
        let mut fresh = Vec::new();
        for evidence in self.deriver.take_notice_evidence() {
            if super::super::input_notices::evidence::before_migration_floor(
                self.writer.store().ledger().input_notices(),
                &evidence.record,
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
        let mut eligible = Vec::new();
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
            .reconcile_notice_migration(complete, &pending, &keys)
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
        ledger
            .input_notices()
            .evidence
            .values()
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
