//! Final notice refusals retain their queue position and wait before a fresh admission.
use super::{AlarmSink, ChannelWriter, DeliveryLease, Derived, DiscordPort, PieceWork};
use crate::services::tui_o::shadow::UnitKind;

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> ChannelWriter<P, L, A> {
    pub(in crate::services::tui_o::writer) fn notice_delivery_ready(&self, item: &Derived) -> bool {
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
            self.notice_cache.defer_delivery((
                origin.record.source.clone(),
                origin.record.start,
                origin.record.end,
            ));
        }
    }
}
