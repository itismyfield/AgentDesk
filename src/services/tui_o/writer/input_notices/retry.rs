//! Final notice refusals retain their queue position and wait before a fresh admission.
use super::{AlarmSink, ChannelWriter, DeliveryLease, Derived, DiscordPort, PieceWork};
use crate::services::tui_o::shadow::UnitKind;

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> ChannelWriter<P, L, A> {
    pub(in crate::services::tui_o::writer) fn notice_delivery_ready(
        &mut self,
        item: &Derived,
    ) -> bool {
        // Missing evidence has no WorkCache entry; its wait must survive that cache's census.
        if let Derived::Piece(piece) = item
            && self.notice_missing.contains(&piece.unit_key)
        {
            let evidence = piece.notice_origin.as_deref().is_some_and(|origin| {
                self.store.ledger().input_notices().evidence.contains_key(&(
                    origin.record.source.clone(),
                    origin.record.start,
                    origin.record.end,
                ))
            });
            let settled = super::super::owed::acked(self.store.ledger(), item);
            if !evidence && !settled {
                return false;
            }
            self.notice_missing.remove(&piece.unit_key);
            return true;
        }
        if super::super::owed::acked(self.store.ledger(), item) {
            return true;
        }
        match item {
            Derived::Piece(piece) if piece.unit_key.kind == UnitKind::InputNotice => {
                piece.notice_origin.as_deref().is_none_or(|origin| {
                    self.notice_cache.delivery_ready(&(
                        origin.record.source.clone(),
                        origin.record.start,
                        origin.record.end,
                    ))
                })
            }
            _ => true,
        }
    }

    pub(super) fn defer_notice_delivery(&mut self, piece: &PieceWork) {
        if piece.unit_key.kind == UnitKind::InputNotice
            && let Some(origin) = piece.notice_origin.as_deref()
        {
            let id = (
                origin.record.source.clone(),
                origin.record.start,
                origin.record.end,
            );
            if !self
                .store
                .ledger()
                .input_notices()
                .evidence
                .contains_key(&id)
            {
                self.notice_missing.insert(piece.unit_key.clone());
            } else {
                self.notice_missing.remove(&piece.unit_key);
                self.notice_cache.defer_delivery(id);
            }
        }
    }
}
