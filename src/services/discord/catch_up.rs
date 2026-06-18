//! #3479 item-2 giant-file decomposition: catch-up subsystem extracted
//! verbatim from `discord/mod.rs`. Startup/restart-gap message recovery —
//! REST-scans configured & checkpointed channels for messages that arrived
//! during the restart window, classifies them, and enqueues the eligible ones.
//! Behavior-preserving move only; logic is unchanged.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use crate::services::provider::ProviderKind;

use super::*;

const CATCH_UP_RETRY_QUEUE_THRESHOLD: usize = MAX_INTERVENTIONS_PER_CHANNEL / 2;

pub(in crate::services::discord) fn should_trigger_catch_up_retry(queue_len: usize) -> bool {
    queue_len <= CATCH_UP_RETRY_QUEUE_THRESHOLD
}

pub(in crate::services::discord) fn take_catch_up_retry_checkpoint_after_queue_drain(
    shared: &SharedData,
    channel_id: ChannelId,
    queue_len_after: usize,
) -> Option<u64> {
    if !should_trigger_catch_up_retry(queue_len_after) {
        return None;
    }
    shared
        .catch_up_retry_pending
        .remove(&channel_id)
        .map(|(_, checkpoint)| checkpoint)
}

fn catch_up_checkpoint_for_scan(
    disk_checkpoint: u64,
    live_checkpoint: Option<u64>,
    retry_checkpoint: Option<u64>,
) -> u64 {
    retry_checkpoint.unwrap_or_else(|| {
        live_checkpoint
            .map(|checkpoint| disk_checkpoint.max(checkpoint))
            .unwrap_or(disk_checkpoint)
    })
}

#[derive(Debug, Clone)]
struct CatchUpChannelCandidate {
    channel_id: ChannelId,
    fallback_name: Option<String>,
    checkpoint_path: Option<PathBuf>,
    disk_checkpoint: Option<u64>,
}

fn insert_configured_catch_up_candidate(
    candidates: &mut BTreeMap<u64, CatchUpChannelCandidate>,
    provider: &ProviderKind,
    owner_provider: &ProviderKind,
    channel_id: u64,
    fallback_name: Option<String>,
) -> bool {
    if owner_provider != provider {
        return false;
    }

    use std::collections::btree_map::Entry;
    match candidates.entry(channel_id) {
        Entry::Occupied(mut entry) => {
            if entry.get().fallback_name.is_none() {
                entry.get_mut().fallback_name = fallback_name;
            }
            false
        }
        Entry::Vacant(entry) => {
            entry.insert(CatchUpChannelCandidate {
                channel_id: ChannelId::new(channel_id),
                fallback_name,
                checkpoint_path: None,
                disk_checkpoint: None,
            });
            true
        }
    }
}

fn catch_up_candidate_allowed_for_bot(
    settings: &DiscordBotSettings,
    provider: &ProviderKind,
    candidate: &CatchUpChannelCandidate,
) -> bool {
    if candidate.disk_checkpoint.is_some() {
        return settings::bot_settings_allow_channel(settings, candidate.channel_id, false);
    }

    settings::validate_bot_channel_routing(
        settings,
        provider,
        candidate.channel_id,
        candidate.fallback_name.as_deref(),
        false,
    )
    .is_ok()
}

fn collect_catch_up_channel_candidates(
    dir: &Path,
    provider: &ProviderKind,
) -> BTreeMap<u64, CatchUpChannelCandidate> {
    let mut candidates = BTreeMap::new();

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(channel_id_raw) = stem.parse::<u64>() else {
                continue;
            };
            let Ok(last_id_str) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(disk_checkpoint) = last_id_str.trim().parse::<u64>() else {
                continue;
            };

            candidates.insert(
                channel_id_raw,
                CatchUpChannelCandidate {
                    channel_id: ChannelId::new(channel_id_raw),
                    fallback_name: None,
                    checkpoint_path: Some(path),
                    disk_checkpoint: Some(disk_checkpoint),
                },
            );
        }
    }

    let mut configured_added = 0usize;
    for binding in settings::list_registered_channel_bindings() {
        if insert_configured_catch_up_candidate(
            &mut candidates,
            provider,
            &binding.owner_provider,
            binding.channel_id,
            binding.fallback_name,
        ) {
            configured_added += 1;
        }
    }

    if configured_added > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔍 catch-up: added {configured_added} configured channel(s) without checkpoint for recent-message scan"
        );
    }

    candidates
}

/// #1227: page size for catch-up REST fetch. Bumped from 10 → 50 because the
/// previous size was overrun by bursty bot output and silently dropped buried
/// user messages.
const CATCH_UP_FETCH_LIMIT: u8 = 50;

/// Filter outcome categories for the catch-up REST scan. Used both at runtime
/// (to emit per-channel breakdown logs even when nothing was recovered) and in
/// unit tests as a pure-function check on the buried-user-message regression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum CatchUpClassification {
    /// Eligible user/allowed-bot message that should be enqueued.
    Recover,
    /// System message kind (thread-created / slash-command etc.) — silently dropped.
    SystemKind,
    /// Authored by this bot (self) — must not re-enqueue our own output.
    SelfAuthored,
    /// Already present in the live mailbox / known set — duplicate.
    Duplicate,
    /// Older than the catch-up max-age window — too late to safely replay.
    TooOld,
    /// Empty content (whitespace only).
    Empty,
    /// Authored by a non-allowed bot or an allowed bot without DISPATCH prefix.
    NotAllowed,
}

/// Per-channel running tally of [`CatchUpClassification`] outcomes — fed into
/// the always-on breakdown log. Keeping this separate from the recovery loop
/// keeps the filter-stats accounting honest and unit-testable.
#[derive(Debug, Default, Clone, Copy)]
pub(in crate::services::discord) struct CatchUpScanStats {
    pub returned: usize,
    pub recovered: usize,
    pub system_kind: usize,
    pub self_authored: usize,
    pub duplicate: usize,
    pub too_old: usize,
    pub empty: usize,
    pub not_allowed: usize,
}

impl CatchUpScanStats {
    pub(in crate::services::discord) fn record(&mut self, outcome: CatchUpClassification) {
        match outcome {
            CatchUpClassification::Recover => self.recovered += 1,
            CatchUpClassification::SystemKind => self.system_kind += 1,
            CatchUpClassification::SelfAuthored => self.self_authored += 1,
            CatchUpClassification::Duplicate => self.duplicate += 1,
            CatchUpClassification::TooOld => self.too_old += 1,
            CatchUpClassification::Empty => self.empty += 1,
            CatchUpClassification::NotAllowed => self.not_allowed += 1,
        }
    }
}

/// Plain inputs to the catch-up filter, decoupled from `serenity::Message` so
/// we can unit test the regression scenario without a Discord runtime.
#[derive(Debug, Clone)]
pub(in crate::services::discord) struct CatchUpMessageView {
    pub message_id: u64,
    pub author_id: u64,
    pub author_is_bot: bool,
    pub is_processable_kind: bool,
    pub age_secs: i64,
    pub trimmed_text: String,
}

/// Pure classifier for the catch-up filter pipeline. Mirrors the order of
/// checks inside the per-message loop in [`catch_up_missed_messages`] so a
/// regression there is caught here. Critically, this function does NOT apply
/// any limit/page-size logic — that decision lives at the REST fetch site
/// (see `CATCH_UP_FETCH_LIMIT`). This means a "buried user message" test must
/// assert against the full fetched page, not a single classification call.
pub(in crate::services::discord) fn classify_catch_up_message(
    msg: &CatchUpMessageView,
    bot_user_id: Option<u64>,
    existing_ids: &std::collections::HashSet<u64>,
    max_age_secs: i64,
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
) -> CatchUpClassification {
    if !msg.is_processable_kind {
        return CatchUpClassification::SystemKind;
    }
    if Some(msg.author_id) == bot_user_id {
        return CatchUpClassification::SelfAuthored;
    }
    if existing_ids.contains(&msg.message_id) {
        return CatchUpClassification::Duplicate;
    }
    if msg.age_secs > max_age_secs {
        return CatchUpClassification::TooOld;
    }
    if msg.trimmed_text.is_empty() {
        return CatchUpClassification::Empty;
    }
    if !is_allowed_turn_sender(
        allowed_bot_ids,
        announce_bot_id,
        msg.author_id,
        msg.author_is_bot,
        &msg.trimmed_text,
    ) {
        return CatchUpClassification::NotAllowed;
    }
    CatchUpClassification::Recover
}

/// Startup catch-up polling: fetch messages that arrived during the restart gap.
/// Uses saved last_message_ids to query Discord REST API for missed messages,
/// filters out bot messages and duplicates, and inserts into intervention queue.
pub(in crate::services::discord) async fn catch_up_missed_messages(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    catch_up_missed_messages_inner(http, shared, provider, &HashMap::new()).await;
}

pub(in crate::services::discord) async fn catch_up_missed_messages_inner(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    retry_checkpoints: &HashMap<ChannelId, u64>,
) {
    let Some(root) = runtime_store::last_message_root() else {
        return;
    };
    let dir = root.join(provider.as_str());

    let mut total_recovered = 0usize;
    let now = Instant::now();
    let max_age = std::time::Duration::from_secs(300); // Only catch up messages within 5 minutes
    let current_bot_user_id = match http.get_current_user().await {
        Ok(user) => Some(user.id.get()),
        Err(err) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ catch-up: failed to resolve current bot user id: {err}");
            None
        }
    };

    // #429: prune stale checkpoints before iterating — files older than
    // max_checkpoint_age were written by sessions that ended long before this
    // restart, so catch-up is pointless and the API calls are wasted.
    let max_checkpoint_age = std::time::Duration::from_secs(600); // 10 minutes
    let mut pruned = 0usize;
    if let Ok(prune_entries) = fs::read_dir(&dir) {
        for entry in prune_entries.flatten() {
            let path = entry.path();
            if let Ok(meta) = path.metadata() {
                if let Ok(modified) = meta.modified() {
                    if modified.elapsed().unwrap_or_default() > max_checkpoint_age {
                        let _ = fs::remove_file(&path);
                        pruned += 1;
                    }
                }
            }
        }
    }
    if pruned > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] 🧹 catch-up: pruned {pruned} stale checkpoint(s) (>10min old)");
    }

    let candidates = collect_catch_up_channel_candidates(&dir, provider);
    if candidates.is_empty() {
        return;
    }

    for candidate in candidates.values() {
        let Some(disk_last_id) = candidate.disk_checkpoint else {
            continue;
        };
        let channel_id = candidate.channel_id;
        let retry_checkpoint = retry_checkpoints.get(&channel_id).copied();
        let live_checkpoint = shared.last_message_ids.get(&channel_id).map(|entry| *entry);
        let last_id = catch_up_checkpoint_for_scan(disk_last_id, live_checkpoint, retry_checkpoint);
        let after_msg = MessageId::new(last_id);

        // #429: skip channels this bot cannot access.  Utility bots
        // (notify/announce) share the claude provider checkpoint dir but
        // have no channel read permissions → every API call fails slowly.
        {
            let settings = shared.settings.read().await;
            if !catch_up_candidate_allowed_for_bot(&settings, provider, candidate) {
                continue;
            }
        }

        match resolve_runtime_channel_binding_status(http, channel_id).await {
            RuntimeChannelBindingStatus::Owned => {}
            RuntimeChannelBindingStatus::Unowned => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⏭ catch-up: dropping stale checkpoint for unowned channel {} ({})",
                    channel_id,
                    candidate
                        .checkpoint_path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "no checkpoint".to_string())
                );
                if let Some(path) = candidate.checkpoint_path.as_ref() {
                    let _ = fs::remove_file(path);
                }
                continue;
            }
            RuntimeChannelBindingStatus::Unknown => continue,
        }

        // Fetch messages after last_id (Discord returns oldest first with after=)
        // #1227: limit was 10 — channels with bursty bot activity (streaming
        // replies + many short turns) routinely fill that window with bot
        // messages, pushing user messages outside the page. Discord applies
        // `limit` BEFORE author filtering; 50 keeps the single-page contract with
        // headroom for the realistic
        // bot:user ratio. Discord per-channel rate limit (5 req / 5 sec)
        // has plenty of margin for this 5x cost.
        let messages = match channel_id
            .messages(
                http,
                serenity::builder::GetMessages::new()
                    .after(after_msg)
                    .limit(CATCH_UP_FETCH_LIMIT),
            )
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                let msg = e.to_string();
                tracing::warn!(
                    "  [{ts}] ⚠ catch-up: failed to fetch messages for channel {channel_id}: {e}"
                );
                // #429: permanent errors — remove checkpoint to avoid retrying every restart
                if msg.contains("Missing Access") || msg.contains("Unknown Channel") {
                    if let Some(path) = candidate.checkpoint_path.as_ref() {
                        let _ = fs::remove_file(path);
                    }
                }
                continue;
            }
        };

        if messages.is_empty() {
            continue;
        }

        // Get bot's own user ID to filter out self-messages
        // Collect existing message IDs in queue for dedup
        let existing_ids = recovery_known_message_ids(&mailbox_snapshot(shared, channel_id).await);

        let allowed_bot_ids: Vec<u64> = {
            let settings = shared.settings.read().await;
            settings.allowed_bot_ids.clone()
        };
        let announce_bot_id = resolve_announce_bot_user_id(shared).await;
        let mut max_recovered_id: Option<u64> = None;
        let mut stats = CatchUpScanStats::default();
        stats.returned = messages.len();

        // Codex P2 on #1301: the 50-message fetch can exceed
        // `MAX_INTERVENTIONS_PER_CHANNEL` (30) on a long restart gap. Without
        // a cap `enqueue_intervention` would silently supersede older
        // queued entries while catch-up still advances the checkpoint to the
        // newest recovered id — meaning the evicted messages are lost. Cap
        // recovery to the queue's remaining capacity at scan-start; the
        // overflow stays unrecovered with the OLD checkpoint, so the next
        // catch-up cycle picks it up from the same `after` cursor.
        let queue_initial_len = mailbox_snapshot(shared, channel_id)
            .await
            .intervention_queue
            .len();
        let remaining_capacity = crate::services::turn_orchestrator::MAX_INTERVENTIONS_PER_CHANNEL
            .saturating_sub(queue_initial_len);

        for msg in &messages {
            let text = msg.content.trim().to_string();
            let msg_ts = msg.id.created_at();
            let age_secs = chrono::Utc::now()
                .signed_duration_since(*msg_ts)
                .num_seconds();
            let view = CatchUpMessageView {
                message_id: msg.id.get(),
                author_id: msg.author.id.get(),
                author_is_bot: msg.author.bot,
                is_processable_kind: router::should_process_turn_message(msg.kind),
                age_secs,
                trimmed_text: text.clone(),
            };
            let outcome = classify_catch_up_message(
                &view,
                current_bot_user_id,
                &existing_ids,
                max_age.as_secs() as i64,
                &allowed_bot_ids,
                announce_bot_id,
            );
            // Codex P2 round 2 on #1301: check the cap BEFORE recording the
            // recover, otherwise `stats.recovered` would tally a message we
            // refused to enqueue and the log would lie about the queue
            // contents. Stopping iteration keeps the checkpoint pinned at
            // the last actually-queued message — newer entries that we
            // declined are still > `after_msg` for the next pass.
            if outcome == CatchUpClassification::Recover && stats.recovered >= remaining_capacity {
                let retry_after = max_recovered_id.unwrap_or(last_id);
                shared
                    .catch_up_retry_pending
                    .insert(channel_id, retry_after);
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔁 catch-up: queue cap reached for channel {}; retry armed after checkpoint {}",
                    channel_id,
                    retry_after
                );
                break;
            }
            stats.record(outcome);
            if outcome != CatchUpClassification::Recover {
                continue;
            }

            reaction_cleanup::cleanup_recovered_catch_up_hourglass(http, channel_id, msg.id).await;
            mailbox_enqueue_intervention(
                shared,
                provider,
                channel_id,
                Intervention {
                    author_id: msg.author.id,
                    author_is_bot: msg.author.bot,
                    message_id: msg.id,
                    source_message_ids: vec![msg.id],
                    text: text.clone(),
                    mode: InterventionMode::Soft,
                    created_at: now,
                    reply_context: None,
                    has_reply_boundary: msg.message_reference.is_some(),
                    merge_consecutive: !msg.author.bot
                        && !text.starts_with('!')
                        && !text.starts_with('/')
                        && !text.starts_with("DISPATCH:"),
                    pending_uploads: Vec::new(),
                    voice_announcement: None,
                },
            )
            .await;
            // Track the newest actually-recovered message for checkpoint
            let mid = msg.id.get();
            if max_recovered_id.map(|m| mid > m).unwrap_or(true) {
                max_recovered_id = Some(mid);
            }
        }

        // #1227: emit a breakdown line for EVERY scanned channel — including
        // 0-recovery — so operator can distinguish "no missed messages" from
        // "limit too small" / "filter ate them" without re-reading the code.
        let ts = chrono::Local::now().format("%H:%M:%S");
        if stats.recovered > 0 {
            total_recovered += stats.recovered;
            tracing::info!(
                "  [{ts}] 🔍 CATCH-UP: recovered {} message(s) for channel {} \
                 (returned={} self={} dup={} too_old={} empty={} not_allowed={} system={})",
                stats.recovered,
                channel_id,
                stats.returned,
                stats.self_authored,
                stats.duplicate,
                stats.too_old,
                stats.empty,
                stats.not_allowed,
                stats.system_kind,
            );
        } else {
            tracing::info!(
                "  [{ts}] 🔍 catch-up scan: channel={} returned={} bot={} dup={} \
                 too_old={} empty={} not_allowed={} system={} recovered=0",
                channel_id,
                stats.returned,
                stats.self_authored,
                stats.duplicate,
                stats.too_old,
                stats.empty,
                stats.not_allowed,
                stats.system_kind,
            );
        }

        // Only advance checkpoint if we actually recovered messages
        if let Some(newest) = max_recovered_id {
            shared.last_message_ids.insert(channel_id, newest);
            if retry_checkpoint.is_some()
                && !shared.catch_up_retry_pending.contains_key(&channel_id)
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔁 catch-up: retry completed for channel {} at checkpoint {}",
                    channel_id,
                    newest
                );
            }
        }
    }

    if total_recovered > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔍 CATCH-UP: total {total_recovered} message(s) recovered across channels"
        );
    }

    // Phase 2: Scan for unanswered messages since last bot response.
    // Catches messages that were queued in-memory but lost on restart. This
    // intentionally also scans configured channels that currently have no
    // checkpoint file, because `/clear` or stale-checkpoint pruning can leave
    // an otherwise valid channel without a disk cursor during a restart gap.
    let mut phase2_recovered = 0usize;
    let allowed_bot_ids_phase2: Vec<u64> = {
        let settings = shared.settings.read().await;
        settings.allowed_bot_ids.clone()
    };
    let announce_bot_id_phase2 = resolve_announce_bot_user_id(shared).await;

    for candidate in candidates.values() {
        let channel_id = candidate.channel_id;

        {
            let settings = shared.settings.read().await;
            if !catch_up_candidate_allowed_for_bot(&settings, provider, candidate) {
                continue;
            }
        }

        match resolve_runtime_channel_binding_status(http, channel_id).await {
            RuntimeChannelBindingStatus::Owned => {}
            RuntimeChannelBindingStatus::Unowned | RuntimeChannelBindingStatus::Unknown => continue,
        }

        // Fetch last 20 messages (newest first — default Discord order)
        let recent = match channel_id
            .messages(http, serenity::builder::GetMessages::new().limit(20))
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                let msg = e.to_string();
                tracing::warn!(
                    "  [{ts}] ⚠ catch-up phase2: failed to fetch recent messages for channel {channel_id}: {e}"
                );
                if msg.contains("Missing Access") || msg.contains("Unknown Channel") {
                    if let Some(path) = candidate.checkpoint_path.as_ref() {
                        let _ = fs::remove_file(path);
                    }
                }
                continue;
            }
        };

        if recent.is_empty() {
            continue;
        }

        // Find the newest bot response (first bot message in newest-first order)
        let last_bot_idx = recent.iter().position(|m| {
            Some(m.author.id.get()) == current_bot_user_id && !m.content.trim().is_empty()
        });

        // Messages at indices 0..last_bot_idx are newer than the last bot response
        let unanswered_slice = match last_bot_idx {
            Some(0) => continue, // Latest message is from bot — nothing unanswered
            Some(idx) => &recent[..idx],
            None => continue, // No bot response found — skip (new/inactive channel)
        };

        // Collect existing queue IDs for dedup
        let mut existing_ids =
            recovery_known_message_ids(&mailbox_snapshot(shared, channel_id).await);
        let mut phase2_checkpoint = shared.last_message_ids.get(&channel_id).map(|v| *v);

        let mut channel_recovered = 0usize;

        // Iterate in reverse (oldest first) for chronological queue order
        for msg in unanswered_slice.iter().rev() {
            if !router::should_process_turn_message(msg.kind) {
                continue;
            }
            if Some(msg.author.id.get()) == current_bot_user_id {
                continue;
            }
            let text = msg.content.trim();
            if text.is_empty() {
                continue;
            }
            let mid = msg.id.get();
            if !is_allowed_turn_sender(
                &allowed_bot_ids_phase2,
                announce_bot_id_phase2,
                msg.author.id.get(),
                msg.author.bot,
                text,
            ) {
                continue;
            }
            let is_allowed_bot = msg.author.bot
                && (allowed_bot_ids_phase2.contains(&msg.author.id.get())
                    || announce_bot_id_phase2.is_some_and(|id| id == msg.author.id.get()));
            if !is_allowed_bot {
                let settings = shared.settings.read().await;
                if !discord_io::user_is_authorized(&settings, msg.author.id.get()) {
                    continue;
                }
            }
            if !should_phase2_recover_message(mid, phase2_checkpoint, &existing_ids) {
                continue;
            }
            // Skip messages older than 10 minutes (generous window for restart gap)
            let msg_age = chrono::Utc::now().signed_duration_since(*msg.id.created_at());
            if msg_age.num_seconds() > 600 {
                continue;
            }

            mailbox_enqueue_intervention(
                shared,
                provider,
                channel_id,
                Intervention {
                    author_id: msg.author.id,
                    author_is_bot: msg.author.bot,
                    message_id: msg.id,
                    source_message_ids: vec![msg.id],
                    text: text.to_string(),
                    mode: InterventionMode::Soft,
                    created_at: now,
                    reply_context: None,
                    has_reply_boundary: msg.message_reference.is_some(),
                    merge_consecutive: !msg.author.bot
                        && !text.starts_with('!')
                        && !text.starts_with('/')
                        && !text.starts_with("DISPATCH:"),
                    pending_uploads: Vec::new(),
                    voice_announcement: None,
                },
            )
            .await;
            existing_ids.insert(mid);
            phase2_checkpoint = Some(phase2_checkpoint.map_or(mid, |saved| saved.max(mid)));
            channel_recovered += 1;
        }

        if channel_recovered > 0 {
            phase2_recovered += channel_recovered;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🔍 CATCH-UP phase2: recovered {} unanswered message(s) for channel {}",
                channel_recovered,
                channel_id
            );
        }
    }

    if phase2_recovered > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔍 CATCH-UP phase2: total {phase2_recovered} unanswered message(s) recovered"
        );
    }
}

#[cfg(test)]
mod catch_up_recovery_tests {
    use super::{
        CatchUpClassification, CatchUpMessageView, ChannelId, ProviderKind,
        classify_catch_up_message, insert_configured_catch_up_candidate,
    };
    use std::collections::{BTreeMap, HashSet};

    #[test]
    fn configured_channel_is_scanned_without_checkpoint_file() {
        let mut candidates = BTreeMap::new();

        assert!(insert_configured_catch_up_candidate(
            &mut candidates,
            &ProviderKind::Claude,
            &ProviderKind::Claude,
            1479671298497183835,
            Some("adk-cc".to_string()),
        ));

        let candidate = candidates.get(&1479671298497183835).unwrap();
        assert_eq!(candidate.channel_id, ChannelId::new(1479671298497183835));
        assert_eq!(candidate.fallback_name.as_deref(), Some("adk-cc"));
        assert!(candidate.checkpoint_path.is_none());
        assert!(candidate.disk_checkpoint.is_none());
    }

    #[test]
    fn configured_channel_metadata_does_not_replace_checkpoint_file() {
        let mut candidates = BTreeMap::new();
        candidates.insert(
            1479671298497183835,
            super::CatchUpChannelCandidate {
                channel_id: ChannelId::new(1479671298497183835),
                fallback_name: None,
                checkpoint_path: Some(std::path::PathBuf::from(
                    "runtime/last_message/claude/1479671298497183835.txt",
                )),
                disk_checkpoint: Some(1504812094456070174),
            },
        );

        assert!(!insert_configured_catch_up_candidate(
            &mut candidates,
            &ProviderKind::Claude,
            &ProviderKind::Claude,
            1479671298497183835,
            Some("adk-cc".to_string()),
        ));

        let candidate = candidates.get(&1479671298497183835).unwrap();
        assert_eq!(candidate.disk_checkpoint, Some(1504812094456070174));
        assert!(candidate.checkpoint_path.is_some());
        assert_eq!(candidate.fallback_name.as_deref(), Some("adk-cc"));
    }

    #[test]
    fn owner_message_is_not_self_authored_when_bot_identity_is_used() {
        let owner_user_id = 343742347365974026;
        let current_bot_id = 9001;
        let view = CatchUpMessageView {
            message_id: 1504813049431724053,
            author_id: owner_user_id,
            author_is_bot: false,
            is_processable_kind: true,
            age_secs: 60,
            trimmed_text: "야~~~".to_string(),
        };
        let existing = HashSet::new();

        assert_eq!(
            classify_catch_up_message(&view, Some(current_bot_id), &existing, 300, &[], None),
            CatchUpClassification::Recover
        );
        assert_eq!(
            classify_catch_up_message(&view, Some(owner_user_id), &existing, 300, &[], None),
            CatchUpClassification::SelfAuthored
        );
    }

    #[test]
    fn announce_bot_message_recovers_without_dispatch_marker() {
        // #3576: a catch-up scan must recover announce-authored trigger
        // traffic even without the DISPATCH:/monitor marker.
        let announce_id = 7777;
        let current_bot_id = 9001;
        let view = CatchUpMessageView {
            message_id: 1504813049431724054,
            author_id: announce_id,
            author_is_bot: true,
            is_processable_kind: true,
            age_secs: 60,
            trimmed_text: "PM triage: claude, please pick up #42".to_string(),
        };
        let existing = HashSet::new();

        // Without the announce_bot_id hint the bot message is NotAllowed
        // (no marker), proving the parameter is load-bearing.
        assert_eq!(
            classify_catch_up_message(&view, Some(current_bot_id), &existing, 300, &[], None),
            CatchUpClassification::NotAllowed
        );
        // With the announce_bot_id hint it recovers.
        assert_eq!(
            classify_catch_up_message(
                &view,
                Some(current_bot_id),
                &existing,
                300,
                &[],
                Some(announce_id),
            ),
            CatchUpClassification::Recover
        );
    }
}
