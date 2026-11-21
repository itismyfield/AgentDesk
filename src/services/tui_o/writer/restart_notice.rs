//! The one sender of the restart notice: one ledger piece per episode, created again only after a
//! certain non-send. A history miss stays unknown, so the notice may be missing but never doubled.

use super::deliver::ChannelWriter;
use super::pieces::PieceWork;
use super::{AlarmSink, DeliveryLease, DiscordPort, Withhold};
use crate::services::tui_o::shadow::{UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::{LedgerState, PieceOutcome};
use crate::services::tui_o::store::restart_notice::{self, Intent};

pub(crate) const TEXT: &str = "재시작으로 턴이 끊겼습니다.";
const QUOTE_CHARS: usize = 60;

/// Names the cut request by its first line, without mentions or markdown that could reformat it.
pub(crate) fn text(request: &str) -> String {
    let line = request.lines().map(str::trim).find(|line| !line.is_empty());
    let clean = |c: char| match c {
        '`' | '*' | '_' | '~' | '|' | '>' | '<' => None,
        '@' => Some('＠'),
        c => Some(c),
    };
    let quote: String = line.unwrap_or("").chars().filter_map(clean).collect();
    let quote = quote.trim();
    match quote.chars().count() {
        0 => TEXT.to_string(),
        n if n > QUOTE_CHARS => {
            let cut: String = quote.chars().take(QUOTE_CHARS).collect();
            format!("{TEXT} (요청: {}…)", cut.trim_end())
        }
        _ => format!("{TEXT} (요청: {quote})"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Notice {
    Delivered(u64),
    /// Discord refused the POST; a retry changes nothing.
    Refused(u16),
    /// The POST may exist but history could not prove it; no second CREATE.
    Unknown,
    /// The channel's output authority refused it before any request; nothing was posted.
    Withheld(String),
    /// Not yet admitted, or the open attempt is still settling.
    Pending,
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
        // The cut marker an earlier relay left on this episode's own card is the notice shown.
        if let Some(card) = &intent.card {
            match self.port().message(intent.channel, card.msg_id).await {
                Ok(Some(seen))
                    if seen.author_id == self.port().bot_id() && seen.content == card.marker =>
                {
                    return Notice::Delivered(seen.id);
                }
                Ok(_) => {}
                Err(_) => return Notice::Pending,
            }
        }
        let origin = intent.origin.as_ref();
        match self
            .port()
            .output_admits(intent.channel, &intent.text, origin)
            .await
        {
            Ok(()) => {}
            Err(Withhold::Refused(reason)) => return Notice::Withheld(reason),
            Err(Withhold::Later(_)) => return Notice::Pending,
        }
        let piece = PieceWork {
            unit_key: key.clone(),
            index: 0,
            payload: intent.text.clone(),
            output_origin: intent.origin.clone(),
            notice_origin: None,
        };
        self.send_piece(&piece, None, None, None).await;
        settled(self.store().ledger(), &key).unwrap_or(Notice::Pending)
    }

    /// Each recorded episode in order; the first one still pending holds the rest for a later poll.
    pub(crate) async fn deliver_restart_notices(&mut self) {
        let Some(root) = self
            .store()
            .restart_notice_root()
            .map(|root| root.to_path_buf())
        else {
            return;
        };
        for intent in restart_notice::pending(&root, self.channel()) {
            let notice = self.post_restart_notice(&intent).await;
            if notice == Notice::Pending {
                return;
            }
            tracing::info!(
                channel = intent.channel,
                ?notice,
                "[tui_o] restart notice settled"
            );
            if let Err(error) = restart_notice::settle(&root, &intent) {
                tracing::warn!(channel = intent.channel, %error, "[tui_o] restart notice kept");
                return;
            }
        }
    }
}
