//! The one sender of the restart notice: one ledger piece per episode, created again only after a
//! certain non-send. A history miss stays unknown, so the notice may be missing but never doubled.

use super::deliver::ChannelWriter;
use super::pieces::PieceWork;
use super::{AlarmSink, DeliveryLease, DiscordPort};
use crate::services::tui_o::shadow::{UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::{LedgerState, PieceOutcome};
use crate::services::tui_o::store::restart_notice::{self, Intent};

pub(crate) const TEXT: &str = "재시작으로 턴이 끊겼습니다.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Notice {
    Delivered(u64),
    /// Discord refused the POST; a retry changes nothing.
    Refused(u16),
    /// The POST may exist but history could not prove it; no second CREATE.
    Unknown,
    /// Not yet admitted, or the open attempt is still settling.
    Pending,
}

#[derive(Default)]
pub(super) struct Delivery {
    pending: bool,
    settle_retry: Option<Intent>,
}

impl Delivery {
    pub(super) fn pending(&self) -> bool {
        self.pending
    }
}

pub(crate) fn unit_key(intent: &Intent) -> UnitKey {
    UnitKey {
        channel_id: intent.channel,
        provider: intent.provider,
        native_key: format!("restart_notice:{}", intent.key()),
        kind: UnitKind::Body,
    }
}

/// `None` when the episode may still be sent: no attempt, or only a certain non-send.
fn settled(ledger: &LedgerState, key: &UnitKey) -> Option<Notice> {
    let (_, piece) = ledger.latest_piece(key, 0)?;
    Some(match piece.outcome.as_ref() {
        None => Notice::Pending,
        Some(PieceOutcome::Posted(msg_id)) => Notice::Delivered(*msg_id),
        Some(PieceOutcome::Rejected(status)) => Notice::Refused(*status),
        Some(PieceOutcome::NotSent) => return None,
        Some(PieceOutcome::NotFound | PieceOutcome::Ambiguous(_) | PieceOutcome::Unresolved(_)) => {
            Notice::Unknown
        }
    })
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> ChannelWriter<P, L, A> {
    /// Under the channel lease and durable Prepared; no local-retry budget and no re-post intent.
    pub(crate) async fn post_restart_notice(&mut self, intent: &Intent) -> Notice {
        if intent.channel != self.channel() || self.settle_open().await.is_err() {
            return Notice::Pending;
        }
        let key = unit_key(intent);
        if let Some(notice) = settled(self.store().ledger(), &key) {
            return notice;
        }
        let piece = PieceWork {
            unit_key: key.clone(),
            index: 0,
            payload: TEXT.to_string(),
            output_origin: None,
            notice_origin: None,
            reply: None,
        };
        self.send_piece(&piece, None, None, None).await;
        settled(self.store().ledger(), &key).unwrap_or(Notice::Pending)
    }

    /// Each recorded episode in order; the first one still pending holds the rest for a later poll.
    pub(super) async fn deliver_restart_notices(&mut self, work: &mut Delivery, ready: bool) {
        let Some(root) = self
            .store()
            .restart_notice_root()
            .map(|root| root.to_path_buf())
        else {
            return;
        };
        if let Some(intent) = &work.settle_retry {
            if !ready || !settle(&root, intent) {
                return;
            }
            work.settle_retry = None;
        }
        let (intents, unreadable) = restart_notice::scan(&root, self.channel());
        work.pending = unreadable || !intents.is_empty();
        if !ready {
            return;
        }
        for intent in intents {
            let notice = self.post_restart_notice(&intent).await;
            if notice == Notice::Pending {
                return;
            }
            tracing::info!(
                channel = intent.channel,
                ?notice,
                "[tui_o] restart notice settled"
            );
            if !settle(&root, &intent) {
                work.settle_retry = Some(intent);
                return;
            }
        }
        work.pending = unreadable;
    }
}

fn settle(root: &std::path::Path, intent: &Intent) -> bool {
    match restart_notice::settle(root, intent) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(channel = intent.channel, %error, "[tui_o] restart notice kept");
            false
        }
    }
}
