use super::super::*;

/// codex review round-6 P2 (#1332): outcome of filtering loaded
/// queued-placeholder mappings against the live mailbox queue.
///
/// `live` is the surviving set ready to be inserted into
/// `SharedData::queued_placeholders`. `channels_with_stale` is the unique
/// channel ids that had at least one mapping pruned — the bootstrap path
/// rewrites their on-disk snapshot so the next restart does not resurrect
/// the stale rows. `stale_count` is purely informational for the FLUSH
/// log line.
///
/// codex review round-7 P2 (#1332): `stale_cards` carries the
/// `(channel_id, user_msg_id, placeholder_msg_id)` tuples for every
/// mapping the filter pruned. The bootstrap caller, after rewriting the
/// disk snapshot, walks these tuples and best-effort calls
/// `delete_message` on Discord — without this, the visible
/// `📬 메시지 대기 중` cards would stay forever (the mapping that owned
/// them was just pruned, so no future dispatch / queue-exit event can
/// reach them). Per-message failures are logged and otherwise tolerated:
/// the bot may not have a fully-initialised gateway at the exact
/// startup moment, in which case the unreachable cards remain visible
/// until the user dismisses them — strictly less severe than the bug
/// report (`📬` cards stuck forever even when the bot has been online
/// for hours).
pub(in crate::services::discord) struct FilteredQueuedPlaceholders {
    pub(in crate::services::discord) live: Vec<((ChannelId, MessageId), MessageId)>,
    pub(in crate::services::discord) channels_with_stale: std::collections::HashSet<ChannelId>,
    pub(in crate::services::discord) stale_count: usize,
    pub(in crate::services::discord) stale_cards: Vec<(ChannelId, MessageId, MessageId)>,
}

/// codex review round-6 P2 (#1332): drop any restored queued-placeholder
/// mapping whose `(channel_id, user_msg_id)` is no longer present in the
/// live mailbox queue snapshot. This runs AFTER the restart pending-queue
/// restore (which rebuilds `intervention_queue` from disk) and BEFORE
/// `kickoff_idle_queues`, so the live set captures both pending-queue
/// restored items and any catch-up message that landed earlier in the
/// startup pipeline.
///
/// A mapping is "stale" when startup skipped or superseded its source
/// message before placeholder restoration ran — for instance, the channel
/// is no longer owned, the sender is no longer allowed, the item was
/// pruned as a duplicate, or it overflowed the queue cap. Without this
/// filter, the `📬 메시지 대기 중` card and its sidecar row would never
/// reach a dispatch or queue-exit event, leaving them stale forever.
pub(in crate::services::discord) fn filter_restored_queued_placeholders(
    loaded: std::collections::HashMap<(ChannelId, MessageId), MessageId>,
    live_queue_ids: &std::collections::HashMap<ChannelId, std::collections::HashSet<u64>>,
) -> FilteredQueuedPlaceholders {
    let mut live: Vec<((ChannelId, MessageId), MessageId)> = Vec::new();
    let mut channels_with_stale: std::collections::HashSet<ChannelId> =
        std::collections::HashSet::new();
    let mut stale_count = 0usize;
    let mut stale_cards: Vec<(ChannelId, MessageId, MessageId)> = Vec::new();
    for ((channel_id, user_msg_id), placeholder_msg_id) in loaded {
        let in_live_queue = live_queue_ids
            .get(&channel_id)
            .map(|ids| ids.contains(&user_msg_id.get()))
            .unwrap_or(false);
        if in_live_queue {
            live.push(((channel_id, user_msg_id), placeholder_msg_id));
        } else {
            stale_count += 1;
            channels_with_stale.insert(channel_id);
            // codex review round-7 P2 (#1332): retain the tuple so the
            // bootstrap caller can issue a best-effort
            // `delete_message` after the disk rewrite. Round-6 dropped
            // the placeholder id at this point, which left every
            // pruned `📬` card visible forever.
            stale_cards.push((channel_id, user_msg_id, placeholder_msg_id));
            tracing::debug!(
                channel_id = channel_id.get(),
                user_msg_id = user_msg_id.get(),
                placeholder_msg_id = placeholder_msg_id.get(),
                "queued_placeholder restore: pruning stale mapping with no live queue entry"
            );
        }
    }
    FilteredQueuedPlaceholders {
        live,
        channels_with_stale,
        stale_count,
        stale_cards,
    }
}

/// codex review round-7 P2 (#1332): best-effort cleanup of the visible
/// `📬 메시지 대기 중` Discord cards whose persisted mapping the round-6
/// filter pruned. Runs AFTER `kickoff_idle_queues` so the gateway-driven
/// HTTP path has had a chance to settle. Per-message failures are logged
/// (including 404 / 403, e.g. the channel has been deleted or the bot
/// can no longer see it) and otherwise tolerated — leaving a card
/// undismissed is strictly better than crashing bootstrap.
///
/// The deletion is dispatched through the small
/// `StalePlaceholderDeleter` indirection so unit tests can substitute a
/// recorder without spinning up a real serenity HTTP client.
pub(in crate::services::discord) async fn delete_stale_queued_placeholder_cards(
    http: &Arc<serenity::Http>,
    stale_cards: &[(ChannelId, MessageId, MessageId)],
) {
    let deleter = SerenityStalePlaceholderDeleter { http: http.clone() };
    delete_stale_queued_placeholder_cards_with(&deleter, stale_cards).await;
}

pub(in crate::services::discord) trait StalePlaceholderDeleter:
    Send + Sync
{
    fn delete<'a>(
        &'a self,
        channel_id: ChannelId,
        placeholder_msg_id: MessageId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
}

struct SerenityStalePlaceholderDeleter {
    http: Arc<serenity::Http>,
}

impl StalePlaceholderDeleter for SerenityStalePlaceholderDeleter {
    fn delete<'a>(
        &'a self,
        channel_id: ChannelId,
        placeholder_msg_id: MessageId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            channel_id
                .delete_message(&self.http, placeholder_msg_id)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }
}

pub(in crate::services::discord) async fn delete_stale_queued_placeholder_cards_with(
    deleter: &dyn StalePlaceholderDeleter,
    stale_cards: &[(ChannelId, MessageId, MessageId)],
) {
    if stale_cards.is_empty() {
        return;
    }
    let mut deleted = 0usize;
    let mut failed = 0usize;
    for (channel_id, user_msg_id, placeholder_msg_id) in stale_cards {
        match deleter.delete(*channel_id, *placeholder_msg_id).await {
            Ok(_) => {
                deleted += 1;
                tracing::debug!(
                    channel_id = channel_id.get(),
                    user_msg_id = user_msg_id.get(),
                    placeholder_msg_id = placeholder_msg_id.get(),
                    "queued_placeholder restore: deleted stale 📬 card",
                );
            }
            Err(error) => {
                failed += 1;
                tracing::warn!(
                    channel_id = channel_id.get(),
                    user_msg_id = user_msg_id.get(),
                    placeholder_msg_id = placeholder_msg_id.get(),
                    "queued_placeholder restore: failed to delete stale 📬 card ({error}); leaving in place",
                );
            }
        }
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🧹 STALE-PLACEHOLDER: deleted {deleted}/{} stale 📬 card(s) on bootstrap (failed {failed})",
        stale_cards.len(),
    );
}

/// codex review round-6 P2 (#1332): snapshot every mailbox in `shared` and
/// collect the union of `intervention.message_id` + every
/// `intervention.source_message_ids` entry per channel. The result is the
/// set of user message ids the queued-placeholder filter accepts as
/// "still live" on this channel.
pub(in crate::services::discord) async fn collect_live_queue_message_ids(
    shared: &SharedData,
) -> std::collections::HashMap<ChannelId, std::collections::HashSet<u64>> {
    let mut by_channel: std::collections::HashMap<ChannelId, std::collections::HashSet<u64>> =
        std::collections::HashMap::new();
    let snapshots = shared.mailboxes.snapshot_all().await;
    for (channel_id, snapshot) in snapshots {
        let ids = crate::services::discord::queued_message_ids(&snapshot);
        if !ids.is_empty() {
            by_channel.insert(channel_id, ids);
        }
    }
    by_channel
}
