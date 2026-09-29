//! Discord side of the O writer: POSTs and history reads through the gateway's shared HTTP client,
//! and one hold on the channel's shared delivery lease per piece.

use std::future::Future;
use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateMessage, GetMessages, MessageId};

use super::super::{DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, SharedData, lease_now_ms};
use crate::services::tui_o::writer::{
    DeliveryLease, DiscordPort, PostOutcome, SeenMessage, confirm,
};

/// Covers a POST (60 s) and its settlement (30 s) with room to spare.
const O_LEASE_DEADLINE_MS: u64 = 180_000;

/// Uses the gateway's cached client, so O shares Legacy's rate limiter and route buckets.
pub(crate) struct GatewayPort {
    http: Arc<serenity::Http>,
    bot_id: u64,
}

impl GatewayPort {
    /// Built where the channel actor is spawned; the writer is dormant until then.
    #[allow(dead_code)]
    pub(crate) fn new(http: Arc<serenity::Http>, bot_id: u64) -> Self {
        Self { http, bot_id }
    }
}

fn seen(message: serenity::Message) -> SeenMessage {
    let (id, author_id) = (message.id.get(), message.author.id.get());
    SeenMessage {
        id,
        author_id,
        content: message.content,
    }
}

fn http_status(error: &serenity::Error) -> Option<u16> {
    let serenity::Error::Http(http) = error else {
        return None;
    };
    Some(http.status_code()?.as_u16())
}

/// 400, 403 and 404 mean the channel refuses this bot; any other failure may still have posted.
fn refusal(status: Option<u16>) -> Option<u16> {
    status.filter(|status| matches!(status, 400 | 403 | 404))
}

impl DiscordPort for GatewayPort {
    fn bot_id(&self) -> u64 {
        self.bot_id
    }

    fn post(
        &self,
        channel: u64,
        content: String,
    ) -> impl Future<Output = PostOutcome> + Send + 'static {
        let http = Arc::clone(&self.http);
        async move {
            let message = CreateMessage::new().content(content);
            match ChannelId::new(channel).send_message(&*http, message).await {
                Ok(message) => PostOutcome::Created(seen(message)),
                Err(error) => refusal(http_status(&error)).map_or_else(
                    || PostOutcome::Uncertain(error.to_string()),
                    PostOutcome::Refused,
                ),
            }
        }
    }

    fn history_after(
        &self,
        channel: u64,
        after: u64,
    ) -> impl Future<Output = Result<Vec<SeenMessage>, String>> + Send {
        let http = Arc::clone(&self.http);
        async move {
            let limit = u8::try_from(confirm::HISTORY_PAGE).unwrap_or(u8::MAX);
            let page = GetMessages::new()
                .after(MessageId::new(after.max(1)))
                .limit(limit);
            let messages = ChannelId::new(channel).messages(&*http, page).await;
            Ok(messages
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(seen)
                .collect())
        }
    }

    /// Unproven until a permission check exists, so a read without a candidate stays Unresolved.
    fn history_readable(&self, _channel: u64) -> bool {
        false
    }
}

/// The per-channel delivery lease cells Legacy's watcher and sink contend on.
pub(crate) struct ChannelLeases {
    cell_for: Box<dyn Fn(ChannelId) -> Arc<DeliveryLeaseCell> + Send + Sync>,
}

impl ChannelLeases {
    /// Built where the channel actor is spawned; the writer is dormant until then.
    #[allow(dead_code)]
    pub(crate) fn from_shared(shared: Arc<SharedData>) -> Self {
        Self {
            cell_for: Box::new(move |channel| shared.delivery_lease(channel)),
        }
    }
}

/// Releases the lease when the piece's result is recorded or the writer gives up on it.
pub(crate) struct OLeaseHold {
    cell: Arc<DeliveryLeaseCell>,
    key: DeliveryLeaseKey,
    serial: u64,
}

impl Drop for OLeaseHold {
    fn drop(&mut self) {
        let (holder, serial) = (
            LeaseHolder::OWriter {
                serial: self.serial,
            },
            self.serial,
        );
        self.cell
            .release(holder, self.key.clone(), serial, serial + 1);
    }
}

impl DeliveryLease for ChannelLeases {
    type Held = OLeaseHold;

    fn try_acquire(&self, channel: u64, serial: u64) -> Option<OLeaseHold> {
        let channel_id = ChannelId::new(channel);
        let cell = (self.cell_for)(channel_id);
        let key = DeliveryLeaseKey::new(channel_id, 0, 0, Some("o_writer"), Some(serial));
        let deadline = lease_now_ms().saturating_add(O_LEASE_DEADLINE_MS);
        let holder = LeaseHolder::OWriter { serial };
        let won = cell.try_acquire(key.clone(), holder, serial, serial + 1, deadline);
        won.then(|| OLeaseHold { cell, key, serial })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn o_holds_the_same_cell_legacy_contends_on_and_releases_it_after_each_piece() {
        let channel = ChannelId::new(7);
        let cell = Arc::new(DeliveryLeaseCell::new(channel));
        let shared_cell = Arc::clone(&cell);
        let leases = ChannelLeases {
            cell_for: Box::new(move |_| Arc::clone(&shared_cell)),
        };
        let watcher = LeaseHolder::Watcher { instance_id: 1 };
        let legacy_key = DeliveryLeaseKey::new(channel, 1, 99, None, None);
        let held = leases.try_acquire(7, 3).expect("the cell starts unleased");
        assert!(
            leases.try_acquire(7, 4).is_none(),
            "a second O piece shares the one lease"
        );
        assert!(
            !cell.try_acquire(legacy_key.clone(), watcher, 0, 10, u64::MAX),
            "Legacy got in while O held it"
        );
        drop(held);
        assert!(cell.try_acquire(legacy_key, watcher, 0, 10, u64::MAX));
        assert!(
            leases.try_acquire(7, 5).is_none(),
            "O got in while Legacy held it"
        );
    }

    #[test]
    fn only_400_403_and_404_count_as_refused() {
        for status in [400, 403, 404] {
            assert_eq!(refusal(Some(status)), Some(status));
        }
        for status in [None, Some(401), Some(429), Some(500), Some(502), Some(503)] {
            assert_eq!(refusal(status), None, "{status:?} may still have posted");
        }
        assert_eq!(http_status(&serenity::Error::Other("timeout")), None);
    }

    /// Posts to Discord. Run only on an approved test channel, with `ADK_O_ROUND_TRIP_TOKEN`
    /// (that bot's token) and `ADK_O_ROUND_TRIP_CHANNEL` set.
    #[tokio::test]
    #[ignore = "posts to Discord; run only against an approved test channel"]
    async fn round_trip_on_an_approved_test_channel() {
        use crate::services::tui_o::writer::round_trip::{RoundTrip, round_trip};
        let token = std::env::var("ADK_O_ROUND_TRIP_TOKEN").expect("ADK_O_ROUND_TRIP_TOKEN");
        let channel = std::env::var("ADK_O_ROUND_TRIP_CHANNEL").expect("ADK_O_ROUND_TRIP_CHANNEL");
        let channel: u64 = channel.parse().expect("a channel id");
        let http = Arc::new(serenity::Http::new(&token));
        let bot_id = http.get_current_user().await.expect("bot user").id.get();
        let trips = round_trip(&GatewayPort::new(http, bot_id), channel).await;
        for trip in &trips {
            let (case, posted, matched) = (&trip.case, trip.posted, trip.matched());
            eprintln!(
                "{case}: posted={posted:?} matched={matched} error={:?}",
                trip.error
            );
        }
        assert!(trips.iter().all(RoundTrip::matched));
    }
}
