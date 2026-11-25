//! Re-settles an Unresolved image or notice attempt from its own stored payload and anchor,
//! never by posting again; the result lands on the same serial.

use crate::services::tui_o::store::ledger::{LedgerEntry, PieceOutcome};
use crate::services::tui_o::writer::confirm::{self, Verdict};
use crate::services::tui_o::writer::deliver::{ChannelWriter, Step};
use crate::services::tui_o::writer::{AlarmSink, DeliveryLease, DiscordPort, WriterAlarm};

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> ChannelWriter<P, L, A> {
    pub(in crate::services::tui_o::writer) async fn reconfirm(&mut self, serial: u64) -> Step {
        let ledger = self.store().ledger();
        let Some(attempt) = ledger.piece(serial) else {
            return Step::Done;
        };
        if !matches!(attempt.outcome, Some(PieceOutcome::Unresolved(_))) {
            return Step::Done;
        }
        let (payload, anchor) = (attempt.payload.clone(), attempt.anchor_id);
        let unsettled = |outcome: &Option<PieceOutcome>| {
            matches!(
                outcome,
                Some(
                    PieceOutcome::NotFound
                        | PieceOutcome::Ambiguous(_)
                        | PieceOutcome::Unresolved(_)
                )
            )
        };
        let earlier_same_payload = (0..serial)
            .filter_map(|earlier| ledger.piece(earlier))
            .any(|piece| unsettled(&piece.outcome) && piece.payload == payload);
        let posted = (0..serial)
            .rev()
            .filter_map(|earlier| match ledger.piece(earlier)?.outcome {
                Some(PieceOutcome::Posted(msg_id)) => Some(msg_id),
                _ => None,
            });
        let proof_ids: Vec<u64> = posted.take(confirm::PROOF_IDS).collect();
        let channel = self.channel();
        let verdict = confirm::settle(
            self.port(),
            channel,
            anchor,
            &payload,
            earlier_same_payload,
            &proof_ids,
        )
        .await;
        let (entry, alarm) = match verdict {
            // The ledger accepts it behind newer posts only as this serial's late confirmation.
            Verdict::Posted(msg_id) => (
                LedgerEntry::Posted {
                    serial,
                    msg_id,
                    confirmation: None,
                },
                None,
            ),
            Verdict::Ambiguous(candidates) => (
                LedgerEntry::Ambiguous { serial, candidates },
                Some(WriterAlarm::Ambiguous { serial }),
            ),
            Verdict::NotFound => (
                LedgerEntry::NotFound { serial },
                Some(WriterAlarm::NotFound { serial }),
            ),
            Verdict::Unresolved(reason) => {
                let alarm = WriterAlarm::Unresolved {
                    serial,
                    reason: reason.clone(),
                };
                (LedgerEntry::Unresolved { serial, reason }, Some(alarm))
            }
        };
        if let Err(step) = self.record(entry) {
            return step;
        }
        if let Some(alarm) = alarm {
            self.alarm(alarm);
        }
        Step::Done
    }
}
