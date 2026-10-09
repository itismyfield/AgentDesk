//! One bounded re-post dispatch: at most one counted POST. A confirmed 429 created nothing, so it
//! is not counted and is the only reason to send again within the dispatch.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use poise::serenity_prelude as serenity;
use serenity::{CreateEmbed, CreateEmbedFooter, CreateMessage};
use sha2::{Digest, Sha256};

use crate::services::tui_o::writer::deliver::POST_TIMEOUT;

/// Discord rejects a longer nonce.
const NONCE_LEN: usize = 25;
/// Discord's embed footer limit, which the note and marker share.
const FOOTER_MAX_CHARS: usize = 2048;
const REPOST_NOTE: &str = "재확인 후 추가 전달";

/// The identifiers every send of one piece carries: the full marker, and a nonce derived from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepostIds {
    nonce: String,
    marker: String,
}

impl RepostIds {
    /// `None` when the marker cannot travel intact; there is no re-post without one.
    pub(crate) fn for_piece(marker: &str) -> Option<Self> {
        let footer_chars = REPOST_NOTE.chars().count() + 1 + marker.chars().count();
        if marker.trim().is_empty() || footer_chars > FOOTER_MAX_CHARS {
            return None;
        }
        let mut nonce = String::from("r");
        nonce.push_str(&hex::encode(Sha256::digest(marker.as_bytes()))[..NONCE_LEN - 1]);
        Some(Self {
            nonce,
            marker: marker.to_owned(),
        })
    }

    pub(crate) fn nonce(&self) -> &str {
        &self.nonce
    }

    pub(crate) fn marker(&self) -> &str {
        &self.marker
    }
}

/// What one dispatch sends: the stored piece content unchanged, identifiers outside it.
#[derive(Clone, Debug)]
pub(crate) struct RepostEnvelope {
    pub(crate) channel: u64,
    content: String,
    ids: RepostIds,
    additional: bool,
}

impl RepostEnvelope {
    /// A piece's first post while re-posting is on: the content plus the shared nonce.
    pub(crate) fn original(channel: u64, content: String, ids: RepostIds) -> Self {
        Self {
            channel,
            content,
            ids,
            additional: false,
        }
    }

    /// A re-post: the same content and nonce, with the note and marker in an embed footer.
    pub(crate) fn additional(channel: u64, content: String, ids: RepostIds) -> Self {
        Self {
            additional: true,
            ..Self::original(channel, content, ids)
        }
    }

    pub(crate) fn message(&self) -> CreateMessage {
        let nonce = serenity::model::channel::Nonce::String(self.ids.nonce.clone());
        let message = CreateMessage::new()
            .content(self.content.clone())
            .nonce(nonce)
            .enforce_nonce(true);
        if !self.additional {
            return message;
        }
        let footer = format!("{REPOST_NOTE} {}", self.ids.marker);
        message.embed(CreateEmbed::new().footer(CreateEmbedFooter::new(footer)))
    }

    /// A re-post counts as identified only when the created message kept its marker.
    pub(crate) fn carries_marker(&self, created: &CreatedMessage) -> bool {
        !self.additional
            || created
                .footers
                .iter()
                .any(|footer| footer.ends_with(&self.ids.marker))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreatedMessage {
    pub(crate) id: u64,
    pub(crate) author_id: u64,
    pub(crate) content: String,
    pub(crate) footers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WireOutcome {
    Created(CreatedMessage),
    /// 400, 403 or 404: the channel refuses the bot.
    Refused(u16),
    /// Only confirmed 429s without a usable retry time: Discord created nothing.
    Throttled,
    /// A request left and its result is unknown; it may have posted.
    Uncertain(String),
    /// No counted request left: the guard withdrew it or it could not be built.
    Unsent(String),
    /// The dispatch overran its timeout; the request task was aborted and awaited.
    TimedOut,
}

#[derive(Default)]
struct WireCount {
    wire: AtomicU32,
    counted: AtomicU32,
    throttled: AtomicU32,
}

/// Checked right before each request leaves.
pub(crate) struct AttemptGuard {
    count: Arc<WireCount>,
    live: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl AttemptGuard {
    /// Refuses once a counted request went out or `live` (switch, holder, deadline) says stop.
    pub(crate) fn begin(&self) -> Result<(), String> {
        if self.count.counted.load(Ordering::SeqCst) > 0 {
            return Err("this dispatch already sent its counted POST".into());
        }
        if !(self.live)() {
            return Err("withdrawn before the request left".into());
        }
        self.count.wire.fetch_add(1, Ordering::SeqCst);
        self.count.counted.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// The request just sent drew a 429, so it leaves the count.
    pub(crate) fn throttled(&self) {
        self.count.counted.fetch_sub(1, Ordering::SeqCst);
        self.count.throttled.fetch_add(1, Ordering::SeqCst);
    }
}

/// `wire == counted + throttled`, and `counted <= 1`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DispatchReport {
    pub(crate) outcome: WireOutcome,
    pub(crate) wire: u32,
    pub(crate) counted: u32,
    pub(crate) throttled: u32,
}

pub(crate) trait BoundedTransport {
    fn create(
        &self,
        envelope: &RepostEnvelope,
        guard: AttemptGuard,
    ) -> impl Future<Output = WireOutcome> + Send + 'static;
}

/// Sends `envelope` once under the writer's POST timeout, which also bounds every 429 wait.
pub(crate) async fn send_bounded(
    transport: &impl BoundedTransport,
    envelope: &RepostEnvelope,
    live: Arc<dyn Fn() -> bool + Send + Sync>,
) -> DispatchReport {
    send_within(transport, envelope, live, POST_TIMEOUT).await
}

pub(crate) async fn send_within(
    transport: &impl BoundedTransport,
    envelope: &RepostEnvelope,
    live: Arc<dyn Fn() -> bool + Send + Sync>,
    timeout: Duration,
) -> DispatchReport {
    let count = Arc::new(WireCount::default());
    let guard = AttemptGuard {
        count: Arc::clone(&count),
        live,
    };
    let mut task = tokio::spawn(transport.create(envelope, guard));
    let outcome = match tokio::time::timeout(timeout, &mut task).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(join)) => WireOutcome::Uncertain(format!("post task ended: {join}")),
        Err(_) => {
            // Report only after the aborted request is gone.
            task.abort();
            let _ = (&mut task).await;
            WireOutcome::TimedOut
        }
    };
    DispatchReport {
        outcome,
        wire: count.wire.load(Ordering::SeqCst),
        counted: count.counted.load(Ordering::SeqCst),
        throttled: count.throttled.load(Ordering::SeqCst),
    }
}
