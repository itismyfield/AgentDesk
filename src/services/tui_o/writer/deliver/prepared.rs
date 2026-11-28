//! What a piece's `Prepared` relies on besides its payload: the output origin and the reply
//! anchor, each written to the ledger before the `Prepared` itself.

use super::*;
pub(super) use crate::services::cluster::intake_router_hook::owner_record::output::Artifact;
use crate::services::tui_o::writer::reply_anchor::{Anchors, Reply, ReplyRef};

/// Returning an error comes before the POST, so the serial's POST was never created.
pub(super) fn append_origins(
    store: &mut ChannelStore,
    serial: u64,
    origin: Option<Artifact>,
    reply_to: Option<u64>,
) -> Result<(), String> {
    if let Some(artifact) = origin {
        store
            .append_ledger(LedgerEntry::OutputOrigin { serial, artifact })
            .map_err(|e| format!("output origin: {e:?}"))?;
    }
    if let Some(anchor) = reply_to {
        store
            .append_ledger(LedgerEntry::ReplyTo { serial, anchor })
            .map_err(|e| format!("reply anchor: {e:?}"))?;
    }
    Ok(())
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> ChannelWriter<P, L, A> {
    /// Shares the native reader's anchors, after the bindings already durable are read back.
    pub(crate) fn attach_reply_anchors(&mut self, anchors: Anchors) {
        match self.store.anchor_bindings() {
            Ok(bindings) => anchors.pinned(bindings),
            Err(error) => {
                tracing::warn!(channel = self.channel, %error, "[tui_o] anchor journal unread")
            }
        }
        self.reply_anchors = Some(anchors);
    }

    /// The anchor the piece replies to, durable before it is returned; `Err` keeps it owed.
    pub(super) async fn reply_for(&mut self, piece: &PieceWork) -> Result<Option<u64>, Step> {
        let coord = match &piece.reply {
            Some(ReplyRef::At(coord)) => coord,
            Some(ReplyRef::Fixed(anchor)) => return Ok(Some(*anchor)),
            None => return Ok(None),
        };
        let Some(anchors) = &self.reply_anchors else {
            return Ok(None);
        };
        let (binding, marks) = match anchors.resolve(coord) {
            Reply::Wait => return Err(Step::LeaseBusy),
            Reply::Unbound => return Ok(None),
            Reply::To {
                binding,
                record: false,
                marks,
            } => (binding, marks),
            Reply::To { binding, marks, .. } => {
                if let Err(error) = self.store.record_anchor(&binding) {
                    tracing::warn!(channel = self.channel, %error, "[tui_o] reply anchor held");
                    return Err(Step::LeaseBusy);
                }
                anchors.pinned([binding.clone()]);
                (binding, marks)
            }
        };
        for message in marks {
            self.port.mark_joined(self.channel, message).await;
        }
        tracing::info!(channel = self.channel, run_start = binding.run_start,
            anchor = binding.anchor, range = ?coord.range, "[tui_o] reply anchored");
        Ok(Some(binding.anchor))
    }

    /// The output origin and reply of the piece's latest serial, which a re-post repeats.
    pub(super) fn piece_origins(&self, key: &PieceKey) -> (Option<Artifact>, Option<u64>) {
        let ledger = self.store.ledger();
        let Some((serial, _)) = ledger.latest_piece(key.unit(), key.piece_index()) else {
            return (None, None);
        };
        (
            ledger.output_origin(serial).cloned(),
            ledger.reply_to(serial),
        )
    }
}
