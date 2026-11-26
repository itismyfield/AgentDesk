use super::*;

use crate::services::discord::runtime_store::usage_footer;
use crate::services::tui_o::shadow::UnitKind;

async fn apply_preference(pool: Option<&sqlx::PgPool>, piece: &PieceWork) -> PieceWork {
    let mut piece = piece.clone();
    if piece.unit_key.kind == UnitKind::Tool
        && piece
            .unit_key
            .native_key
            .ends_with(super::super::turn_summary::TURN_SUMMARY_SUFFIX)
        && let Some((first_line, _)) = piece.payload.split_once("\n-# ")
        && !usage_footer::enabled(pool, piece.unit_key.channel_id).await
    {
        if let Some(origin) = &mut piece.output_origin {
            origin.retain_payload_prefix(&piece.payload, first_line);
        }
        piece.payload.truncate(first_line.len());
    }
    piece
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> ChannelWriter<P, L, A> {
    pub(super) async fn deliver_piece(
        &mut self,
        piece: &PieceWork,
        admission: Option<Admission<'_>>,
    ) -> Step {
        let ledger = self.store.ledger();
        if ledger.excluded(&piece.unit_key).is_some() {
            return Step::Done;
        }
        let disposition = self
            .receipts
            .disposition(ledger, &piece.unit_key, piece.index);
        let (piece, approved) = match disposition {
            PieceDisposition::Blocked { serial, status } => {
                tracing::warn!(channel = self.channel, serial, status, unit_key = ?piece.unit_key,
                    "[tui_o] output blocked; fix the refusal and use /clear for a fresh session");
                return self.stop(WriterAlarm::Blocked { status });
            }
            PieceDisposition::Settled => return Step::Done,
            PieceDisposition::Open { .. } => {
                return self.stop(WriterAlarm::LedgerViolation {
                    detail: "open piece after settling".into(),
                });
            }
            PieceDisposition::Authorized { rejected_serial } => {
                let Some(original) = ledger.piece(rejected_serial) else {
                    return self.stop(WriterAlarm::LedgerViolation {
                        detail: "approved piece missing".into(),
                    });
                };
                if original.payload != piece.payload {
                    tracing::warn!(channel = self.channel, rejected_serial, unit_key = ?piece.unit_key,
                        "[tui_o] rederived payload differs; sending ledger payload");
                }
                (
                    PieceWork {
                        output_origin: ledger.output_origin(rejected_serial).cloned(),
                        unit_key: original.unit_key.clone(),
                        index: original.piece_index,
                        payload: original.payload.clone(),
                        notice_origin: original.notice_origin.clone(),
                        reply: ledger
                            .reply_to(rejected_serial)
                            .map(super::super::reply_anchor::ReplyRef::Fixed),
                    },
                    Some(rejected_serial),
                )
            }
            PieceDisposition::Fresh => (
                apply_preference(self.port.usage_footer_pool(), piece).await,
                None,
            ),
        };
        let (serial, anchor_id) = (ledger.next_serial(), ledger.anchor());
        let mut retry_owner = OriginalGate::SendLocal;
        if approved.is_none()
            && let Some(mut hook) = self.repost.take()
        {
            let record = PieceRecord {
                notice_origin: piece.notice_origin.clone(),
                local_retry: None,
                outcome_at: None,
                unit_key: piece.unit_key.clone(),
                piece_index: piece.index,
                payload: piece.payload.clone(),
                anchor_id,
                epoch: 0,
                prepared_at: chrono::Utc::now(),
                outcome: None,
            };
            let gate = hook.original(serial, record).await;
            self.repost = Some(hook);
            retry_owner = gate;
            match gate {
                OriginalGate::SendLocal | OriginalGate::SendPgManaged => {}
                // Re-post owns an admitted piece's sends; its original is never sent again.
                OriginalGate::Admitted => return Step::Done,
                // Unknown admission holds the piece and retries it on a later poll.
                OriginalGate::Unknown => return Step::LeaseBusy,
            }
        }
        let local_retry =
            (approved.is_none() && admission.is_none() && retry_owner == OriginalGate::SendLocal)
                .then_some(LocalRetryMark::Budget);
        self.send_piece(&piece, admission, local_retry, approved)
            .await
    }
}
