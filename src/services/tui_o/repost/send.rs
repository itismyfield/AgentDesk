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
pub struct RepostIds {
    nonce: String,
    marker: String,
}

impl RepostIds {
    /// `None` when the marker cannot travel intact; there is no re-post without one.
    pub fn for_piece(marker: &str) -> Option<Self> {
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

    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    pub(crate) fn marker(&self) -> &str {
        &self.marker
    }
}

/// What one dispatch sends: the stored piece content unchanged, identifiers outside it.
#[derive(Clone, Debug)]
pub struct RepostEnvelope {
    pub(crate) channel: u64,
    content: String,
    ids: RepostIds,
    additional: bool,
    #[cfg(any(test, feature = "test-support"))]
    pub output_origin:
        Option<crate::services::cluster::intake_router_hook::owner_record::output::Artifact>,
    #[cfg(not(any(test, feature = "test-support")))]
    pub(crate) output_origin:
        Option<crate::services::cluster::intake_router_hook::owner_record::output::Artifact>,
    /// The re-posted unit's push decision, made by the notification policy.
    pub(crate) silent: bool,
    /// The message the original replied to; a deleted one leaves the re-post unreferenced.
    pub(crate) reply_to: Option<u64>,
}

impl RepostEnvelope {
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_reply_to_for_test(&mut self, reply_to: Option<u64>) {
        self.reply_to = reply_to;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn original_for_test(channel: u64, content: String, ids: RepostIds) -> Self {
        Self::original(channel, content, ids)
    }

    /// A piece's first post while re-posting is on: the content plus the shared nonce.
    pub(crate) fn original(channel: u64, content: String, ids: RepostIds) -> Self {
        Self {
            channel,
            content,
            ids,
            additional: false,
            output_origin: None,
            silent: false,
            reply_to: None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn additional_for_test(channel: u64, content: String, ids: RepostIds) -> Self {
        Self::additional(channel, content, ids)
    }

    /// A re-post: the same content and nonce, with the note and marker in an embed footer.
    pub(crate) fn additional(channel: u64, content: String, ids: RepostIds) -> Self {
        Self {
            additional: true,
            ..Self::original(channel, content, ids)
        }
    }

    pub(crate) fn payload(&self) -> &str {
        &self.content
    }

    pub fn message(&self) -> CreateMessage {
        let nonce = serenity::model::channel::Nonce::String(self.ids.nonce.clone());
        let message = crate::services::discord::outbound::notification_policy::silence(
            CreateMessage::new(),
            self.silent,
        )
        .content(self.content.clone())
        .allowed_mentions(crate::services::message_outbox::mentions::relay_allowed_mentions())
        .nonce(nonce)
        .enforce_nonce(true);
        let message = match self.reply_to {
            Some(id) => message.reference_message(
                serenity::MessageReference::from((
                    serenity::ChannelId::new(self.channel),
                    serenity::MessageId::new(id),
                ))
                .fail_if_not_exists(false),
            ),
            None => message,
        };
        if !self.additional {
            return message;
        }
        let footer = format!("{REPOST_NOTE} {}", self.ids.marker);
        message.embed(CreateEmbed::new().footer(CreateEmbedFooter::new(footer)))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn carries_marker_for_test(&self, created: &CreatedMessage) -> bool {
        self.carries_marker(created)
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
pub struct CreatedMessage {
    #[cfg(any(test, feature = "test-support"))]
    pub id: u64,
    #[cfg(not(any(test, feature = "test-support")))]
    pub(crate) id: u64,
    #[cfg(any(test, feature = "test-support"))]
    pub author_id: u64,
    #[cfg(not(any(test, feature = "test-support")))]
    pub(crate) author_id: u64,
    #[cfg(any(test, feature = "test-support"))]
    pub content: String,
    #[cfg(not(any(test, feature = "test-support")))]
    pub(crate) content: String,
    #[cfg(any(test, feature = "test-support"))]
    pub footers: Vec<String>,
    #[cfg(not(any(test, feature = "test-support")))]
    pub(crate) footers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireOutcome {
    Created(CreatedMessage),
    Withheld(String),
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
pub struct AttemptGuard {
    count: Arc<WireCount>,
    live: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl AttemptGuard {
    /// Refuses once a counted request went out or `live` (holder, source, deadline) says stop.
    pub fn begin(&self) -> Result<(), String> {
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
    pub fn throttled(&self) {
        self.count.counted.fetch_sub(1, Ordering::SeqCst);
        self.count.throttled.fetch_add(1, Ordering::SeqCst);
    }
}

/// `wire == counted + throttled`, and `counted <= 1`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchReport {
    #[cfg(any(test, feature = "test-support"))]
    pub outcome: WireOutcome,
    #[cfg(not(any(test, feature = "test-support")))]
    pub(crate) outcome: WireOutcome,
    #[cfg(any(test, feature = "test-support"))]
    pub wire: u32,
    #[cfg(not(any(test, feature = "test-support")))]
    pub(crate) wire: u32,
    #[cfg(any(test, feature = "test-support"))]
    pub counted: u32,
    #[cfg(not(any(test, feature = "test-support")))]
    pub(crate) counted: u32,
    #[cfg(any(test, feature = "test-support"))]
    pub throttled: u32,
    #[cfg(not(any(test, feature = "test-support")))]
    pub(crate) throttled: u32,
}

pub trait BoundedTransport {
    fn create(
        &self,
        envelope: &RepostEnvelope,
        guard: AttemptGuard,
    ) -> impl Future<Output = WireOutcome> + Send + 'static;
}

/// The counts of one handed-over dispatch: the same cells its guard writes.
pub struct DispatchCounts(Arc<WireCount>);

impl DispatchCounts {
    #[cfg(any(test, feature = "test-support"))]
    pub fn report_for_test(&self, outcome: WireOutcome) -> DispatchReport {
        self.report(outcome)
    }

    pub(crate) fn report(&self, outcome: WireOutcome) -> DispatchReport {
        DispatchReport {
            outcome,
            wire: self.0.wire.load(Ordering::SeqCst),
            counted: self.0.counted.load(Ordering::SeqCst),
            throttled: self.0.throttled.load(Ordering::SeqCst),
        }
    }
}

/// One dispatch's request, built with its guard inside but not yet polled, so the caller can
/// poll it first under the gate and then own its timeout. Dropped unpolled, nothing was sent.
pub fn hand_over(
    transport: &impl BoundedTransport,
    envelope: &RepostEnvelope,
    live: Arc<dyn Fn() -> bool + Send + Sync>,
) -> (
    impl Future<Output = WireOutcome> + Send + 'static,
    DispatchCounts,
) {
    let count = Arc::new(WireCount::default());
    let guard = AttemptGuard {
        count: Arc::clone(&count),
        live,
    };
    (transport.create(envelope, guard), DispatchCounts(count))
}

/// Sends `envelope` once under the writer's POST timeout, which also bounds every 429 wait.
pub(crate) async fn send_bounded(
    transport: &impl BoundedTransport,
    envelope: &RepostEnvelope,
    live: Arc<dyn Fn() -> bool + Send + Sync>,
) -> DispatchReport {
    send_within(transport, envelope, live, POST_TIMEOUT).await
}

pub async fn send_within(
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
