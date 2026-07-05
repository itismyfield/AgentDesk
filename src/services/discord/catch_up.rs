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
const CATCH_UP_RETRY_FETCH_FAILURE_LIMIT: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services) struct CatchUpRetryState {
    checkpoint: u64,
    fetch_failures: u8,
    armed_at: Instant,
}

impl CatchUpRetryState {
    fn new(checkpoint: u64) -> Self {
        Self {
            checkpoint,
            fetch_failures: 0,
            armed_at: Instant::now(),
        }
    }

    fn after_fetch_failure(self) -> Option<Self> {
        let fetch_failures = self.fetch_failures.saturating_add(1);
        (fetch_failures <= CATCH_UP_RETRY_FETCH_FAILURE_LIMIT).then_some(Self {
            checkpoint: self.checkpoint,
            fetch_failures,
            armed_at: self.armed_at,
        })
    }
}

#[async_trait::async_trait]
trait CatchUpDiscordApi: Sync {
    async fn current_user_id(&self) -> Result<Option<u64>, String>;

    async fn resolve_runtime_channel_binding_status(
        &self,
        channel_id: ChannelId,
    ) -> RuntimeChannelBindingStatus;

    async fn fetch_messages(
        &self,
        channel_id: ChannelId,
        request: serenity::builder::GetMessages,
    ) -> Result<Vec<serenity::Message>, String>;

    async fn cleanup_recovered_catch_up_hourglass(
        &self,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        message_id: MessageId,
    );
}

struct SerenityCatchUpDiscordApi<'a> {
    http: &'a Arc<serenity::Http>,
}

#[async_trait::async_trait]
impl CatchUpDiscordApi for SerenityCatchUpDiscordApi<'_> {
    async fn current_user_id(&self) -> Result<Option<u64>, String> {
        self.http
            .get_current_user()
            .await
            .map(|user| Some(user.id.get()))
            .map_err(|err| err.to_string())
    }

    async fn resolve_runtime_channel_binding_status(
        &self,
        channel_id: ChannelId,
    ) -> RuntimeChannelBindingStatus {
        resolve_runtime_channel_binding_status(self.http, channel_id).await
    }

    async fn fetch_messages(
        &self,
        channel_id: ChannelId,
        request: serenity::builder::GetMessages,
    ) -> Result<Vec<serenity::Message>, String> {
        channel_id
            .messages(self.http, request)
            .await
            .map_err(|err| err.to_string())
    }

    async fn cleanup_recovered_catch_up_hourglass(
        &self,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        message_id: MessageId,
    ) {
        reaction_cleanup::cleanup_recovered_catch_up_hourglass(
            self.http, shared, channel_id, message_id,
        )
        .await;
    }
}

mod classification;
mod phase2;

use classification::{CatchUpClassification, CatchUpScanStats};
#[cfg(test)]
use phase2::catch_up_enqueue_accepted;
use phase2::{
    Phase2EnqueueCommit, Phase2RecoveryStats, advance_phase2_checkpoint,
    catch_up_remaining_queue_capacity, classify_phase2_enqueue_commit,
    log_catch_up_enqueue_not_accepted, phase2_retry_after_checkpoint,
};

pub(in crate::services::discord) fn should_trigger_catch_up_retry(queue_len: usize) -> bool {
    queue_len <= CATCH_UP_RETRY_QUEUE_THRESHOLD
}

pub(in crate::services::discord) fn take_catch_up_retry_checkpoint_after_queue_drain(
    shared: &SharedData,
    channel_id: ChannelId,
    queue_len_after: usize,
) -> Option<CatchUpRetryState> {
    if !should_trigger_catch_up_retry(queue_len_after) {
        return None;
    }
    shared
        .catch_up_retry_pending
        .remove(&channel_id)
        .map(|(_, checkpoint)| checkpoint)
}

fn arm_catch_up_retry_pending(shared: &SharedData, channel_id: ChannelId, retry_after: u64) -> u64 {
    arm_catch_up_retry_state(shared, channel_id, CatchUpRetryState::new(retry_after))
        .checkpoint
}

fn arm_catch_up_retry_state(
    shared: &SharedData,
    channel_id: ChannelId,
    retry_state: CatchUpRetryState,
) -> CatchUpRetryState {
    let mut pending = shared
        .catch_up_retry_pending
        .entry(channel_id)
        .or_insert(retry_state);
    *pending = merge_catch_up_retry_state(Some(*pending), retry_state);
    *pending
}

fn merge_catch_up_retry_state(
    existing: Option<CatchUpRetryState>,
    retry_state: CatchUpRetryState,
) -> CatchUpRetryState {
    let Some(existing) = existing else {
        return retry_state;
    };
    CatchUpRetryState {
        checkpoint: merge_catch_up_retry_checkpoint(
            Some(existing.checkpoint),
            retry_state.checkpoint,
        ),
        // A merged older checkpoint inherits the most exhausted budget so the
        // same old backlog cannot gain unbounded retries through fresh arms.
        fetch_failures: existing.fetch_failures.max(retry_state.fetch_failures),
        armed_at: existing.armed_at.min(retry_state.armed_at),
    }
}

fn merge_catch_up_retry_pending_into(
    retry_checkpoints: &mut HashMap<ChannelId, CatchUpRetryState>,
    shared: &SharedData,
) -> usize {
    let pending_channels: Vec<ChannelId> = shared
        .catch_up_retry_pending
        .iter()
        .map(|entry| *entry.key())
        .collect();
    let mut consumed = 0usize;
    for channel_id in pending_channels {
        let Some((_, retry_state)) = shared.catch_up_retry_pending.remove(&channel_id) else {
            continue;
        };
        let merged = merge_catch_up_retry_state(
            retry_checkpoints.get(&channel_id).copied(),
            retry_state,
        );
        retry_checkpoints.insert(channel_id, merged);
        consumed += 1;
    }
    consumed
}

fn rearm_catch_up_retry_after_fetch_failure(
    shared: &SharedData,
    channel_id: ChannelId,
    retry_state: CatchUpRetryState,
) -> Option<CatchUpRetryState> {
    let ts = chrono::Local::now().format("%H:%M:%S");
    let attempted_failures = retry_state.fetch_failures.saturating_add(1);
    let Some(next_retry_state) = retry_state.after_fetch_failure() else {
        tracing::warn!(
            "  [{ts}] ⚠ catch-up: retry scan for channel {} failed {} time(s); giving up after {} re-arm(s) at checkpoint {}",
            channel_id,
            attempted_failures,
            CATCH_UP_RETRY_FETCH_FAILURE_LIMIT,
            retry_state.checkpoint
        );
        return None;
    };
    let rearmed = arm_catch_up_retry_state(shared, channel_id, next_retry_state);
    tracing::warn!(
        "  [{ts}] 🔁 catch-up: retry scan failed for channel {}; re-armed after checkpoint {} (failure {}/{})",
        channel_id,
        rearmed.checkpoint,
        rearmed.fetch_failures,
        CATCH_UP_RETRY_FETCH_FAILURE_LIMIT
    );
    Some(rearmed)
}

fn merge_catch_up_retry_checkpoint(existing: Option<u64>, retry_after: u64) -> u64 {
    existing.map_or(retry_after, |checkpoint| checkpoint.min(retry_after))
}

fn catch_up_message_age_reference_time(
    scan_wall_time: chrono::DateTime<chrono::Utc>,
    scan_instant: Instant,
    retry_state: Option<CatchUpRetryState>,
) -> chrono::DateTime<chrono::Utc> {
    let Some(retry_state) = retry_state else {
        return scan_wall_time;
    };
    let elapsed_since_arm = scan_instant
        .checked_duration_since(retry_state.armed_at)
        .unwrap_or_default();
    let Ok(elapsed_since_arm) = chrono::Duration::from_std(elapsed_since_arm) else {
        return scan_wall_time;
    };
    scan_wall_time - elapsed_since_arm
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatchUpFetchMode {
    After(u64),
    Recent,
}

impl CatchUpFetchMode {
    fn checkpoint(self) -> Option<u64> {
        match self {
            Self::After(checkpoint) => Some(checkpoint),
            Self::Recent => None,
        }
    }
}

fn catch_up_fetch_mode_for_scan(
    candidate: &CatchUpChannelCandidate,
    live_checkpoint: Option<u64>,
    retry_checkpoint: Option<u64>,
) -> CatchUpFetchMode {
    if let Some(disk_checkpoint) = candidate.disk_checkpoint {
        return CatchUpFetchMode::After(catch_up_checkpoint_for_scan(
            disk_checkpoint,
            live_checkpoint,
            retry_checkpoint,
        ));
    }

    if let Some(retry_checkpoint) = retry_checkpoint {
        return CatchUpFetchMode::After(retry_checkpoint);
    }
    if let Some(live_checkpoint) = live_checkpoint {
        return CatchUpFetchMode::After(live_checkpoint);
    }

    CatchUpFetchMode::Recent
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
        return settings::bot_settings_allow_channel(
            settings,
            provider,
            candidate.channel_id,
            false,
        );
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

/// #3668 F3: max backward-pagination pages for the no-checkpoint `Recent` scan.
///
/// `Recent` mode (channels with no disk/live/retry checkpoint, e.g. after
/// `/clear` or stale-checkpoint pruning) previously fetched exactly one
/// `limit=50` page. Discord applies `limit` BEFORE author filtering and returns
/// newest-first, so a burst of newer bot/system noise can fill that single page
/// and push an age-window-eligible user message off the end — it is never
/// fetched, so catch-up silently fails to recover it. In `Recent` mode we now
/// page backward with `.before(cursor)` up to this budget, stopping early as
/// soon as a page's oldest message exceeds the age window (nothing older can
/// qualify). 4 pages × 50 = 200 messages of reach; well inside the Discord
/// per-channel rate limit (5 req / 5 s). The `After` (checkpoint) path is
/// unchanged — it already has a precise lower bound.
const CATCH_UP_RECENT_MAX_PAGES: u8 = 4;

/// #3668 F3: decide whether to fetch another backward page in `Recent` mode.
///
/// Pure so the budget / age-window early-exit contract is unit-testable without
/// a Discord runtime. Returns `true` only when (a) the page budget has room and
/// (b) the just-fetched page's oldest message is still within the age window
/// (i.e. an older page could still hold an eligible message). `oldest_age_secs`
/// is the age of the oldest message in the page just fetched; `None` means the
/// page was empty (no cursor to page before → stop).
fn should_fetch_older_recent_page(
    pages_fetched: u8,
    oldest_age_secs: Option<i64>,
    max_age_secs: i64,
) -> bool {
    if pages_fetched >= CATCH_UP_RECENT_MAX_PAGES {
        return false;
    }
    match oldest_age_secs {
        Some(age) => age <= max_age_secs,
        None => false,
    }
}

/// Inter-channel pacing for the catch-up REST sweep.
///
/// Startup recovery and the periodic backstop scan every configured channel
/// back-to-back (two REST round-trips each — binding-status + message fetch).
/// On a fresh restart, dozens of channels are scanned with no checkpoint at
/// once, and that tight, un-paced burst monopolises the async executor and
/// Discord REST budget right as every background DB consumer is also spinning
/// up — starving DB-bound tasks of the time they need to acquire a pooled
/// connection within `acquire_timeout`. Spacing the per-channel scans by a
/// small delay spreads the burst so the rest of the runtime keeps making
/// progress. `AGENTDESK_CATCH_UP_SCAN_PACE_MS` overrides the gap (0 disables —
/// used by tests and by operators who want the old unthrottled behaviour).
const CATCH_UP_SCAN_PACE_DEFAULT_MS: u64 = 100;

/// Pure parse of the pacing override. Missing or unparseable values fall back
/// to the default; `0` is honoured and yields a zero (no-op) gap. Kept env-free
/// so it is deterministically unit-testable without touching process globals.
fn parse_catch_up_scan_pace(raw: Option<&str>) -> std::time::Duration {
    let ms = raw
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(CATCH_UP_SCAN_PACE_DEFAULT_MS);
    std::time::Duration::from_millis(ms)
}

fn catch_up_scan_pace() -> std::time::Duration {
    parse_catch_up_scan_pace(
        std::env::var("AGENTDESK_CATCH_UP_SCAN_PACE_MS")
            .ok()
            .as_deref(),
    )
}

/// Whether to wait the pacing gap before the next per-channel scan. The first
/// scan in a sweep runs immediately (`already_scanned == false`); every later
/// scan is paced. Extracted as a named predicate so the first-immediate /
/// subsequent-paced contract is covered by a unit test.
fn should_pace_before_scan(already_scanned: bool) -> bool {
    already_scanned
}

/// Sleep the configured catch-up pacing gap before the next per-channel scan.
/// No-op when pacing is disabled (0 ms) so test runs stay fast.
async fn catch_up_scan_pace_gap() {
    let pace = catch_up_scan_pace();
    if !pace.is_zero() {
        tokio::time::sleep(pace).await;
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
    let mut retry_checkpoints = HashMap::new();
    let consumed = merge_catch_up_retry_pending_into(&mut retry_checkpoints, shared);
    if consumed > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔁 catch-up: sweep consumed {consumed} pending retry checkpoint(s)"
        );
    }
    catch_up_missed_messages_inner(http, shared, provider, &retry_checkpoints).await;
}

pub(in crate::services::discord) async fn catch_up_missed_messages_inner(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    retry_checkpoints: &HashMap<ChannelId, CatchUpRetryState>,
) {
    let api = SerenityCatchUpDiscordApi { http };
    catch_up_missed_messages_inner_with_api(&api, shared, provider, retry_checkpoints).await;
}

async fn catch_up_missed_messages_inner_with_api<A: CatchUpDiscordApi + ?Sized>(
    api: &A,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    retry_checkpoints: &HashMap<ChannelId, CatchUpRetryState>,
) {
    let Some(root) = runtime_store::last_message_root() else {
        return;
    };
    let dir = root.join(provider.as_str());

    let mut total_recovered = 0usize;
    let now = Instant::now();
    let max_age = std::time::Duration::from_secs(300); // Only catch up messages within 5 minutes
    let current_bot_user_id = match api.current_user_id().await {
        Ok(user_id) => user_id,
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

    let mut candidates = collect_catch_up_channel_candidates(&dir, provider);
    for channel_id in retry_checkpoints.keys().copied() {
        candidates
            .entry(channel_id.get())
            .or_insert_with(|| CatchUpChannelCandidate {
                channel_id,
                fallback_name: None,
                checkpoint_path: None,
                disk_checkpoint: None,
            });
    }
    if candidates.is_empty() {
        return;
    }

    // Pace successive per-channel REST scans so a many-channel sweep doesn't
    // fire as one tight burst (see `catch_up_scan_pace`). The first eligible
    // channel runs immediately; subsequent scans wait the configured gap.
    let mut paced_scan = false;
    for candidate in candidates.values() {
        let channel_id = candidate.channel_id;
        let retry_state = retry_checkpoints.get(&channel_id).copied();
        let retry_checkpoint = retry_state.map(|state| state.checkpoint);
        let live_checkpoint = shared.last_message_ids.get(&channel_id).map(|entry| *entry);
        let fetch_mode = catch_up_fetch_mode_for_scan(candidate, live_checkpoint, retry_checkpoint);
        let scan_checkpoint = fetch_mode.checkpoint();

        // #429: skip channels this bot cannot access.  Utility bots
        // (notify/announce) share the claude provider checkpoint dir but
        // have no channel read permissions → every API call fails slowly.
        {
            let settings = shared.settings.read().await;
            if !catch_up_candidate_allowed_for_bot(&settings, provider, candidate) {
                continue;
            }
        }

        if should_pace_before_scan(paced_scan) {
            catch_up_scan_pace_gap().await;
        }
        paced_scan = true;

        match api.resolve_runtime_channel_binding_status(channel_id).await {
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
            RuntimeChannelBindingStatus::Unknown => {
                if let Some(retry_state) = retry_state {
                    rearm_catch_up_retry_after_fetch_failure(shared, channel_id, retry_state);
                }
                continue;
            }
        }

        // Fetch messages after the saved cursor when one exists. Configured
        // channels can legitimately have no last_message checkpoint yet (for
        // example after `/clear` or stale-checkpoint pruning), so fall back to a
        // bounded recent-message scan instead of silently skipping the channel.
        // #1227: limit was 10 — channels with bursty bot activity (streaming
        // replies + many short turns) routinely fill that window with bot
        // messages, pushing user messages outside the page. Discord applies
        // `limit` BEFORE author filtering; 50 keeps the single-page contract with
        // headroom for the realistic
        // bot:user ratio. Discord per-channel rate limit (5 req / 5 sec)
        // has plenty of margin for this 5x cost.
        let mut request = serenity::builder::GetMessages::new().limit(CATCH_UP_FETCH_LIMIT);
        if let CatchUpFetchMode::After(last_id) = fetch_mode {
            request = request.after(MessageId::new(last_id));
        }
        let mut messages = match api.fetch_messages(channel_id, request).await {
            Ok(msgs) => msgs,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ catch-up: failed to fetch messages for channel {channel_id}: {e}"
                );
                // #429: permanent errors — remove checkpoint to avoid retrying every restart
                if e.contains("Missing Access") || e.contains("Unknown Channel") {
                    if let Some(path) = candidate.checkpoint_path.as_ref() {
                        let _ = fs::remove_file(path);
                    }
                } else if let Some(retry_state) = retry_state {
                    rearm_catch_up_retry_after_fetch_failure(shared, channel_id, retry_state);
                }
                continue;
            }
        };

        // #3668 F3: `Recent` mode (no checkpoint) has no lower bound, so a single
        // newest-first page can be fully consumed by newer bot/system noise and
        // bury an age-window-eligible user message past page 1. Page backward
        // with `.before(oldest)` up to `CATCH_UP_RECENT_MAX_PAGES`, stopping as
        // soon as a page's oldest message exceeds the age window. The `After`
        // path keeps its precise single-page contract and is left untouched.
        if matches!(fetch_mode, CatchUpFetchMode::Recent) {
            let mut pages_fetched: u8 = 1;
            loop {
                // The oldest message currently held is the smallest id (Discord
                // returns newest-first; ids are time-ordered snowflakes).
                let Some(oldest) = messages.iter().min_by_key(|msg| msg.id.get()) else {
                    break;
                };
                let oldest_id = oldest.id;
                let oldest_age_secs = chrono::Utc::now()
                    .signed_duration_since(*oldest_id.created_at())
                    .num_seconds();
                if !should_fetch_older_recent_page(
                    pages_fetched,
                    Some(oldest_age_secs),
                    max_age.as_secs() as i64,
                ) {
                    break;
                }
                if should_pace_before_scan(true) {
                    catch_up_scan_pace_gap().await;
                }
                let older_request = serenity::builder::GetMessages::new()
                    .limit(CATCH_UP_FETCH_LIMIT)
                    .before(oldest_id);
                match api.fetch_messages(channel_id, older_request).await {
                    Ok(older) if !older.is_empty() => {
                        pages_fetched += 1;
                        messages.extend(older);
                    }
                    Ok(_) => break, // no older messages → done
                    Err(e) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ catch-up: backward page fetch failed for channel {channel_id}: {e}"
                        );
                        break; // keep what we already have
                    }
                }
            }
        }

        if messages.is_empty() {
            continue;
        }
        messages.sort_by_key(|msg| msg.id.get());

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
        let remaining_capacity = catch_up_remaining_queue_capacity(queue_initial_len);

        for msg in &messages {
            let text = msg.content.trim().to_string();
            let msg_ts = msg.id.created_at();
            let age_reference = catch_up_message_age_reference_time(
                chrono::Utc::now(),
                Instant::now(),
                retry_state,
            );
            let age_secs = age_reference.signed_duration_since(*msg_ts).num_seconds();
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
                let retry_after = max_recovered_id
                    .or(scan_checkpoint)
                    .map(|checkpoint| arm_catch_up_retry_pending(shared, channel_id, checkpoint));
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔁 catch-up: queue cap reached for channel {}; retry armed after checkpoint {:?}",
                    channel_id,
                    retry_after
                );
                break;
            }
            if outcome != CatchUpClassification::Recover {
                stats.record(outcome);
                continue;
            }

            let enqueue = mailbox_enqueue_intervention(
                shared,
                provider,
                channel_id,
                Intervention {
                    author_id: msg.author.id,
                    author_is_bot: msg.author.bot,
                    message_id: msg.id,
                    queued_generation: crate::services::discord::runtime_store::load_generation(),
                    source_message_ids: vec![msg.id],
                    source_message_queued_generations: Vec::new(),
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
            let mid = msg.id.get();
            match classify_phase2_enqueue_commit(&enqueue) {
                Phase2EnqueueCommit::Accepted => {
                    stats.record(CatchUpClassification::Recover);
                    api.cleanup_recovered_catch_up_hourglass(shared, channel_id, msg.id)
                        .await;
                    if max_recovered_id.map(|m| mid > m).unwrap_or(true) {
                        max_recovered_id = Some(mid);
                    }
                }
                Phase2EnqueueCommit::Duplicate => {
                    stats.record(CatchUpClassification::Duplicate);
                    if max_recovered_id.map(|m| mid > m).unwrap_or(true) {
                        max_recovered_id = Some(mid);
                    }
                }
                Phase2EnqueueCommit::Deferred => {
                    log_catch_up_enqueue_not_accepted("phase1", channel_id, msg.id, &enqueue);
                    continue;
                }
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
            advance_last_message_checkpoint(shared, provider, channel_id, MessageId::new(newest));
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

    // Same per-channel pacing as phase 1 (see `catch_up_scan_pace`).
    let mut paced_scan_phase2 = false;
    for candidate in candidates.values() {
        let channel_id = candidate.channel_id;

        {
            let settings = shared.settings.read().await;
            if !catch_up_candidate_allowed_for_bot(&settings, provider, candidate) {
                continue;
            }
        }

        if should_pace_before_scan(paced_scan_phase2) {
            catch_up_scan_pace_gap().await;
        }
        paced_scan_phase2 = true;

        match api.resolve_runtime_channel_binding_status(channel_id).await {
            RuntimeChannelBindingStatus::Owned => {}
            RuntimeChannelBindingStatus::Unowned | RuntimeChannelBindingStatus::Unknown => continue,
        }

        // Fetch last 20 messages (newest first — default Discord order)
        let recent = match api
            .fetch_messages(channel_id, serenity::builder::GetMessages::new().limit(20))
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ catch-up phase2: failed to fetch recent messages for channel {channel_id}: {e}"
                );
                if e.contains("Missing Access") || e.contains("Unknown Channel") {
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

        // Messages at indices 0..last_bot_idx are newer than the last bot response.
        let (last_bot_response_id, unanswered_slice) = match last_bot_idx {
            Some(0) => continue, // Latest message is from bot — nothing unanswered
            Some(idx) => (recent[idx].id.get(), &recent[..idx]),
            None => continue, // No bot response found — skip (new/inactive channel)
        };

        // Collect existing queue IDs for dedup from the same snapshot used for
        // capacity so the initial phase2 claim boundary is internally coherent.
        let mailbox = mailbox_snapshot(shared, channel_id).await;
        let remaining_capacity =
            catch_up_remaining_queue_capacity(mailbox.intervention_queue.len());
        let mut existing_ids = recovery_known_message_ids(&mailbox);
        let mut phase2_checkpoint = shared.last_message_ids.get(&channel_id).map(|v| *v);
        let phase2_checkpoint_start = phase2_checkpoint;
        let mut max_recovered_id: Option<u64> = None;
        let mut stats = Phase2RecoveryStats {
            returned: recent.len(),
            discovered: unanswered_slice.len(),
            ..Phase2RecoveryStats::default()
        };
        let mut phase2_retry_after: Option<u64> = None;

        // Iterate in reverse (oldest first) for chronological queue order
        for msg in unanswered_slice.iter().rev() {
            if !router::should_process_turn_message(msg.kind) {
                stats.skipped += 1;
                continue;
            }
            if Some(msg.author.id.get()) == current_bot_user_id {
                stats.skipped += 1;
                continue;
            }
            let text = msg.content.trim();
            if text.is_empty() {
                stats.skipped += 1;
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
                stats.skipped += 1;
                continue;
            }
            let is_allowed_bot = msg.author.bot
                && (allowed_bot_ids_phase2.contains(&msg.author.id.get())
                    || announce_bot_id_phase2.is_some_and(|id| id == msg.author.id.get()));
            if !is_allowed_bot {
                let settings = shared.settings.read().await;
                if !discord_io::user_is_authorized(&settings, msg.author.id.get()) {
                    stats.skipped += 1;
                    continue;
                }
            }
            // Skip messages older than 10 minutes (generous window for restart gap)
            let msg_age = chrono::Utc::now().signed_duration_since(*msg.id.created_at());
            if msg_age.num_seconds() > 600 {
                stats.skipped += 1;
                continue;
            }

            stats.eligible += 1;
            if existing_ids.contains(&mid) {
                stats.duplicate += 1;
                phase2_checkpoint = advance_phase2_checkpoint(phase2_checkpoint, mid);
                continue;
            }
            if phase2_checkpoint.is_some_and(|saved| mid <= saved) {
                stats.skipped += 1;
                continue;
            }
            debug_assert!(should_phase2_recover_message(
                mid,
                phase2_checkpoint,
                &existing_ids
            ));

            if stats.enqueued >= remaining_capacity {
                let retry_after = phase2_retry_after_checkpoint(
                    max_recovered_id,
                    phase2_checkpoint,
                    last_bot_response_id,
                );
                let retry_after = arm_catch_up_retry_pending(shared, channel_id, retry_after);
                phase2_retry_after = Some(retry_after);
                stats.deferred += 1;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔁 catch-up phase2: queue cap reached for channel {}; retry armed after checkpoint {}",
                    channel_id,
                    retry_after
                );
                break;
            }

            let enqueue = mailbox_enqueue_intervention(
                shared,
                provider,
                channel_id,
                Intervention {
                    author_id: msg.author.id,
                    author_is_bot: msg.author.bot,
                    message_id: msg.id,
                    queued_generation: crate::services::discord::runtime_store::load_generation(),
                    source_message_ids: vec![msg.id],
                    source_message_queued_generations: Vec::new(),
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
            match classify_phase2_enqueue_commit(&enqueue) {
                Phase2EnqueueCommit::Accepted => {
                    existing_ids.insert(mid);
                    phase2_checkpoint = advance_phase2_checkpoint(phase2_checkpoint, mid);
                    max_recovered_id = advance_phase2_checkpoint(max_recovered_id, mid);
                    stats.enqueued += 1;
                }
                Phase2EnqueueCommit::Duplicate => {
                    existing_ids.insert(mid);
                    phase2_checkpoint = advance_phase2_checkpoint(phase2_checkpoint, mid);
                    stats.duplicate += 1;
                }
                Phase2EnqueueCommit::Deferred => {
                    log_catch_up_enqueue_not_accepted("phase2", channel_id, msg.id, &enqueue);
                    let retry_after = phase2_retry_after_checkpoint(
                        max_recovered_id,
                        phase2_checkpoint,
                        last_bot_response_id,
                    );
                    let retry_after = arm_catch_up_retry_pending(shared, channel_id, retry_after);
                    phase2_retry_after = Some(retry_after);
                    stats.deferred += 1;
                    break;
                }
            }
        }

        if let Some(newest) = max_recovered_id {
            advance_last_message_checkpoint(shared, provider, channel_id, MessageId::new(newest));
            phase2_recovered += stats.enqueued;
        }

        if stats.enqueued > 0 {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🔍 CATCH-UP phase2: recovered {} unanswered message(s) for channel {}",
                stats.enqueued,
                channel_id
            );
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔍 catch-up phase2 scan: channel={} returned={} discovered={} eligible={} duplicate={} skipped={} enqueued={} deferred={} checkpoint_start={:?} checkpoint_final={:?} retry_after={:?}",
            channel_id,
            stats.returned,
            stats.discovered,
            stats.eligible,
            stats.duplicate,
            stats.skipped,
            stats.enqueued,
            stats.deferred,
            phase2_checkpoint_start,
            phase2_checkpoint,
            phase2_retry_after,
        );
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
        CATCH_UP_RECENT_MAX_PAGES, CATCH_UP_RETRY_FETCH_FAILURE_LIMIT,
        CATCH_UP_SCAN_PACE_DEFAULT_MS, CatchUpChannelCandidate, CatchUpClassification,
        CatchUpDiscordApi, CatchUpFetchMode, CatchUpMessageView, CatchUpRetryState,
        ChannelId, MessageId,
        Phase2EnqueueCommit, ProviderKind, RuntimeChannelBindingStatus,
        advance_phase2_checkpoint, arm_catch_up_retry_pending, catch_up_enqueue_accepted,
        catch_up_fetch_mode_for_scan, catch_up_message_age_reference_time,
        catch_up_missed_messages_inner_with_api, catch_up_remaining_queue_capacity,
        classify_catch_up_message, classify_phase2_enqueue_commit,
        insert_configured_catch_up_candidate, merge_catch_up_retry_checkpoint,
        merge_catch_up_retry_pending_into, parse_catch_up_scan_pace,
        phase2_retry_after_checkpoint, rearm_catch_up_retry_after_fetch_failure,
        should_fetch_older_recent_page, should_pace_before_scan,
        take_catch_up_retry_checkpoint_after_queue_drain,
    };
    use crate::services::turn_orchestrator::EnqueueRefusalReason;
    use poise::serenity_prelude as serenity;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    struct ScopedRuntimeRoot {
        _lock: std::sync::MutexGuard<'static, ()>,
        temp: tempfile::TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl ScopedRuntimeRoot {
        fn path(&self) -> &std::path::Path {
            self.temp.path()
        }
    }

    impl Drop for ScopedRuntimeRoot {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                    None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
                }
            }
        }
    }

    fn scoped_runtime_root() -> ScopedRuntimeRoot {
        let lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        let temp = tempfile::tempdir().expect("create catch-up test runtime root");
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", temp.path());
        }
        ScopedRuntimeRoot {
            _lock: lock,
            temp,
            previous,
        }
    }

    fn write_last_message_checkpoint(
        root: &std::path::Path,
        provider: &ProviderKind,
        channel_id: ChannelId,
        checkpoint: u64,
    ) {
        let dir = root
            .join("runtime")
            .join("last_message")
            .join(provider.as_str());
        std::fs::create_dir_all(&dir).expect("create last_message provider dir");
        std::fs::write(dir.join(format!("{}.txt", channel_id.get())), checkpoint.to_string())
            .expect("write last_message checkpoint");
    }

    struct TestCatchUpDiscordApi {
        current_user_id: Option<u64>,
        binding_status: RuntimeChannelBindingStatus,
        fetch_error: Option<&'static str>,
    }

    impl TestCatchUpDiscordApi {
        fn transient_fetch_failure() -> Self {
            Self {
                current_user_id: Some(9001),
                binding_status: RuntimeChannelBindingStatus::Owned,
                fetch_error: Some("temporary test fetch failure"),
            }
        }

        fn unknown_binding() -> Self {
            Self {
                current_user_id: Some(9001),
                binding_status: RuntimeChannelBindingStatus::Unknown,
                fetch_error: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl CatchUpDiscordApi for TestCatchUpDiscordApi {
        async fn current_user_id(&self) -> Result<Option<u64>, String> {
            Ok(self.current_user_id)
        }

        async fn resolve_runtime_channel_binding_status(
            &self,
            _channel_id: ChannelId,
        ) -> RuntimeChannelBindingStatus {
            self.binding_status
        }

        async fn fetch_messages(
            &self,
            _channel_id: ChannelId,
            _request: serenity::builder::GetMessages,
        ) -> Result<Vec<serenity::Message>, String> {
            match self.fetch_error {
                Some(error) => Err(error.to_string()),
                None => Ok(Vec::new()),
            }
        }

        async fn cleanup_recovered_catch_up_hourglass(
            &self,
            _shared: &Arc<super::super::SharedData>,
            _channel_id: ChannelId,
            _message_id: MessageId,
        ) {
        }
    }

    #[test]
    fn scan_pace_parses_valid_zero_invalid_and_missing() {
        // Explicit value is honoured.
        assert_eq!(
            parse_catch_up_scan_pace(Some("250")).as_millis(),
            250,
            "valid value should be used verbatim"
        );
        // 0 is honoured and yields a no-op (zero) gap — the documented disable.
        assert!(
            parse_catch_up_scan_pace(Some("0")).is_zero(),
            "0 must disable pacing"
        );
        // Surrounding whitespace is tolerated.
        assert_eq!(parse_catch_up_scan_pace(Some("  75 ")).as_millis(), 75);
        // Unparseable and missing both fall back to the default.
        assert_eq!(
            parse_catch_up_scan_pace(Some("abc")).as_millis(),
            CATCH_UP_SCAN_PACE_DEFAULT_MS as u128,
            "garbage falls back to default"
        );
        assert_eq!(
            parse_catch_up_scan_pace(None).as_millis(),
            CATCH_UP_SCAN_PACE_DEFAULT_MS as u128,
            "missing env falls back to default"
        );
    }

    #[test]
    fn first_scan_runs_immediately_then_subsequent_scans_pace() {
        // Mirrors the loop contract: the very first eligible channel scans with
        // no delay; once one scan has happened, every later channel is paced.
        assert!(!should_pace_before_scan(false), "first scan must not wait");
        assert!(
            should_pace_before_scan(true),
            "subsequent scans must wait the pacing gap"
        );
    }

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
        assert_eq!(
            catch_up_fetch_mode_for_scan(candidate, None, None),
            CatchUpFetchMode::Recent
        );
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
        assert_eq!(
            catch_up_fetch_mode_for_scan(candidate, Some(1504812094456070175), None),
            CatchUpFetchMode::After(1504812094456070175)
        );
    }

    #[test]
    fn retry_checkpoint_overrides_live_cursor_for_recent_scan_retry() {
        let candidate = CatchUpChannelCandidate {
            channel_id: ChannelId::new(1479671298497183835),
            fallback_name: Some("adk-cc".to_string()),
            checkpoint_path: None,
            disk_checkpoint: None,
        };

        assert_eq!(
            catch_up_fetch_mode_for_scan(
                &candidate,
                Some(1504812094456070999),
                Some(1504812094456070000)
            ),
            CatchUpFetchMode::After(1504812094456070000)
        );
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

    // #3668 F3: backward-pagination budget / age-window early-exit contract for
    // the no-checkpoint `Recent` scan.
    #[test]
    fn recent_pagination_continues_while_within_age_window_and_under_budget() {
        // First page (pages_fetched=1) whose oldest message is still inside the
        // 300s window → a buried older user message may exist → fetch more.
        assert!(
            should_fetch_older_recent_page(1, Some(120), 300),
            "in-window oldest under budget should page backward"
        );
    }

    #[test]
    fn recent_pagination_stops_when_oldest_exceeds_age_window() {
        // The oldest message in the page is already older than the window —
        // nothing older can qualify, so stop without another fetch.
        assert!(
            !should_fetch_older_recent_page(1, Some(301), 300),
            "out-of-window oldest must early-exit"
        );
    }

    #[test]
    fn recent_pagination_stops_at_page_budget() {
        // Even fully within the age window, the budget caps backward reach so a
        // pathological channel cannot trigger unbounded fetches.
        assert!(
            !should_fetch_older_recent_page(CATCH_UP_RECENT_MAX_PAGES, Some(10), 300),
            "page budget must cap backward pagination"
        );
        // One below the budget still pages.
        assert!(
            should_fetch_older_recent_page(CATCH_UP_RECENT_MAX_PAGES - 1, Some(10), 300),
            "below-budget in-window page should continue"
        );
    }

    #[test]
    fn recent_pagination_stops_on_empty_page() {
        // No oldest message (empty page) → no cursor to page before → stop.
        assert!(
            !should_fetch_older_recent_page(1, None, 300),
            "empty page must stop pagination"
        );
    }

    #[test]
    fn after_mode_is_unchanged_by_recent_pagination() {
        // Regression guard: the `After` (checkpoint) fetch mode keeps its single
        // precise lower bound; the backward-pagination loop is gated on
        // `matches!(fetch_mode, Recent)` only.
        let candidate = CatchUpChannelCandidate {
            channel_id: ChannelId::new(99),
            fallback_name: None,
            checkpoint_path: None,
            disk_checkpoint: Some(1_500_000_000_000_000_000),
        };
        assert!(matches!(
            catch_up_fetch_mode_for_scan(&candidate, None, None),
            CatchUpFetchMode::After(_)
        ));
    }

    #[test]
    fn enqueue_acceptance_requires_queue_success_without_persistence_error() {
        let accepted = super::super::MailboxEnqueueOutcome {
            enqueued: true,
            merged: false,
            refusal_reason: None,
            persistence_error: None,
        };
        assert!(catch_up_enqueue_accepted(&accepted));

        let refused = super::super::MailboxEnqueueOutcome {
            enqueued: false,
            merged: false,
            refusal_reason: Some(
                crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdAlreadyQueued,
            ),
            persistence_error: None,
        };
        assert!(!catch_up_enqueue_accepted(&refused));

        let persistence_failed = super::super::MailboxEnqueueOutcome {
            enqueued: true,
            merged: false,
            refusal_reason: None,
            persistence_error: Some("disk unavailable".to_string()),
        };
        assert!(!catch_up_enqueue_accepted(&persistence_failed));
    }

    #[test]
    fn phase2_capacity_uses_remaining_mailbox_slots() {
        let max = crate::services::turn_orchestrator::MAX_INTERVENTIONS_PER_CHANNEL;

        assert_eq!(catch_up_remaining_queue_capacity(0), max);
        assert_eq!(catch_up_remaining_queue_capacity(max - 1), 1);
        assert_eq!(catch_up_remaining_queue_capacity(max), 0);
        assert_eq!(catch_up_remaining_queue_capacity(max + 10), 0);
    }

    #[test]
    fn phase2_retry_anchor_falls_back_to_last_bot_response_without_checkpoint() {
        assert_eq!(phase2_retry_after_checkpoint(None, None, 90), 90);
        assert_eq!(phase2_retry_after_checkpoint(None, Some(100), 90), 100);
        assert_eq!(phase2_retry_after_checkpoint(Some(110), Some(100), 90), 110);
        assert_eq!(phase2_retry_after_checkpoint(Some(120), Some(150), 90), 150);
    }

    #[test]
    fn retry_pending_preserves_oldest_unrecovered_checkpoint() {
        assert_eq!(merge_catch_up_retry_checkpoint(None, 150), 150);
        assert_eq!(merge_catch_up_retry_checkpoint(Some(100), 150), 100);
        assert_eq!(merge_catch_up_retry_checkpoint(Some(200), 150), 150);
    }

    #[test]
    fn periodic_sweep_consumes_pending_retry_checkpoints() {
        let shared = super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(1479671298497183835);
        let older_armed_at = Instant::now()
            .checked_sub(Duration::from_secs(30))
            .expect("test instant can subtract old arm age");
        let newer_armed_at = Instant::now()
            .checked_sub(Duration::from_secs(5))
            .expect("test instant can subtract fresh arm age");
        shared.catch_up_retry_pending.insert(
            channel_id,
            CatchUpRetryState {
                checkpoint: 1504812094456070200,
                fetch_failures: 2,
                armed_at: older_armed_at,
            },
        );

        let mut retry_checkpoints = HashMap::from([(
            channel_id,
            CatchUpRetryState {
                checkpoint: 1504812094456070300,
                fetch_failures: 1,
                armed_at: newer_armed_at,
            },
        )]);

        assert_eq!(
            merge_catch_up_retry_pending_into(&mut retry_checkpoints, &shared),
            1
        );
        assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));

        let merged = retry_checkpoints
            .get(&channel_id)
            .expect("pending retry should be merged into sweep retry map");
        assert_eq!(merged.checkpoint, 1504812094456070200);
        assert_eq!(merged.fetch_failures, 2);
        assert_eq!(merged.armed_at, older_armed_at);
    }

    #[test]
    fn retry_age_window_uses_original_arm_time() {
        let scan_instant = Instant::now();
        let armed_at = scan_instant
            .checked_sub(Duration::from_secs(240))
            .expect("test instant can subtract retry age");
        let scan_wall_time = chrono::Utc::now();
        let message_time = scan_wall_time - chrono::Duration::seconds(400);
        let retry_state = CatchUpRetryState {
            checkpoint: 1504812094456070130,
            fetch_failures: 1,
            armed_at,
        };

        let normal_reference = catch_up_message_age_reference_time(
            scan_wall_time,
            scan_instant,
            None,
        );
        assert_eq!(
            normal_reference
                .signed_duration_since(message_time)
                .num_seconds(),
            400
        );

        let retry_reference = catch_up_message_age_reference_time(
            scan_wall_time,
            scan_instant,
            Some(retry_state),
        );
        assert_eq!(
            retry_reference
                .signed_duration_since(message_time)
                .num_seconds(),
            160
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn catch_up_inner_rearms_pending_on_retry_fetch_failure() {
        let root = scoped_runtime_root();
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1479671298497183835);
        let checkpoint = 1504812094456070130;
        write_last_message_checkpoint(root.path(), &provider, channel_id, checkpoint);
        let armed_at = Instant::now()
            .checked_sub(Duration::from_secs(20))
            .expect("test instant can subtract retry age");
        let retry_state = CatchUpRetryState {
            checkpoint,
            fetch_failures: 1,
            armed_at,
        };
        let retry_checkpoints = HashMap::from([(channel_id, retry_state)]);

        catch_up_missed_messages_inner_with_api(
            &TestCatchUpDiscordApi::transient_fetch_failure(),
            &shared,
            &provider,
            &retry_checkpoints,
        )
        .await;

        let pending = shared
            .catch_up_retry_pending
            .get(&channel_id)
            .expect("retry fetch failure should re-arm pending retry");
        assert_eq!(pending.checkpoint, checkpoint);
        assert_eq!(pending.fetch_failures, 2);
        assert_eq!(pending.armed_at, armed_at);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn catch_up_inner_rearms_pending_on_retry_unknown_binding() {
        let root = scoped_runtime_root();
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1479671298497183835);
        let checkpoint = 1504812094456070130;
        write_last_message_checkpoint(root.path(), &provider, channel_id, checkpoint);
        let armed_at = Instant::now()
            .checked_sub(Duration::from_secs(20))
            .expect("test instant can subtract retry age");
        let retry_state = CatchUpRetryState {
            checkpoint,
            fetch_failures: 1,
            armed_at,
        };
        let retry_checkpoints = HashMap::from([(channel_id, retry_state)]);

        catch_up_missed_messages_inner_with_api(
            &TestCatchUpDiscordApi::unknown_binding(),
            &shared,
            &provider,
            &retry_checkpoints,
        )
        .await;

        let pending = shared
            .catch_up_retry_pending
            .get(&channel_id)
            .expect("unknown binding in retry mode should re-arm pending retry");
        assert_eq!(pending.checkpoint, checkpoint);
        assert_eq!(pending.fetch_failures, 2);
        assert_eq!(pending.armed_at, armed_at);
    }

    #[test]
    fn retry_fetch_failure_rearms_pending_for_next_drain() {
        let shared = super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(1479671298497183835);
        let checkpoint = 1504812094456070130;

        assert_eq!(
            arm_catch_up_retry_pending(&shared, channel_id, checkpoint),
            checkpoint
        );
        let first_retry = take_catch_up_retry_checkpoint_after_queue_drain(
            &shared,
            channel_id,
            0,
        )
        .expect("pending retry should be consumed by the first drain");
        assert_eq!(first_retry.checkpoint, checkpoint);
        assert_eq!(first_retry.fetch_failures, 0);
        assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));

        let rearmed = rearm_catch_up_retry_after_fetch_failure(&shared, channel_id, first_retry)
            .expect("transient retry fetch failure should re-arm the same backlog cursor");
        assert_eq!(rearmed.checkpoint, checkpoint);
        assert_eq!(rearmed.fetch_failures, 1);
        assert_eq!(rearmed.armed_at, first_retry.armed_at);

        let second_retry = take_catch_up_retry_checkpoint_after_queue_drain(
            &shared,
            channel_id,
            0,
        )
        .expect("subsequent drain should retry the over-cap backlog again");
        assert_eq!(second_retry, rearmed);
    }

    #[test]
    fn retry_fetch_failure_gives_up_after_bounded_attempts() {
        let shared = super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(1479671298497183835);
        let checkpoint = 1504812094456070130;
        let exhausted_retry = CatchUpRetryState {
            checkpoint,
            fetch_failures: CATCH_UP_RETRY_FETCH_FAILURE_LIMIT,
            armed_at: Instant::now(),
        };

        assert!(
            rearm_catch_up_retry_after_fetch_failure(&shared, channel_id, exhausted_retry).is_none(),
            "retry fetch failures must not re-arm forever"
        );
        assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
    }

    #[test]
    fn phase2_checkpoint_advances_only_to_known_recovered_or_duplicate_ids() {
        assert_eq!(advance_phase2_checkpoint(None, 100), Some(100));
        assert_eq!(advance_phase2_checkpoint(Some(100), 99), Some(100));
        assert_eq!(advance_phase2_checkpoint(Some(100), 101), Some(101));
    }

    #[test]
    fn phase2_enqueue_commit_classifies_source_id_duplicate_without_recovery() {
        let accepted = super::super::MailboxEnqueueOutcome {
            enqueued: true,
            merged: false,
            refusal_reason: None,
            persistence_error: None,
        };
        assert_eq!(
            classify_phase2_enqueue_commit(&accepted),
            Phase2EnqueueCommit::Accepted
        );

        let duplicate = super::super::MailboxEnqueueOutcome {
            enqueued: false,
            merged: false,
            refusal_reason: Some(EnqueueRefusalReason::SourceIdAlreadyQueued),
            persistence_error: None,
        };
        assert_eq!(
            classify_phase2_enqueue_commit(&duplicate),
            Phase2EnqueueCommit::Duplicate
        );

        let already_active = super::super::MailboxEnqueueOutcome {
            enqueued: false,
            merged: false,
            refusal_reason: Some(EnqueueRefusalReason::AlreadyActiveTurn),
            persistence_error: None,
        };
        assert_eq!(
            classify_phase2_enqueue_commit(&already_active),
            Phase2EnqueueCommit::Duplicate
        );
    }

    #[test]
    fn phase2_enqueue_commit_defers_retryable_actor_and_persistence_failures() {
        let actor_unreachable = super::super::MailboxEnqueueOutcome {
            enqueued: false,
            merged: false,
            refusal_reason: Some(EnqueueRefusalReason::ActorUnreachable),
            persistence_error: None,
        };
        assert_eq!(
            classify_phase2_enqueue_commit(&actor_unreachable),
            Phase2EnqueueCommit::Deferred
        );

        let last_item_dedup = super::super::MailboxEnqueueOutcome {
            enqueued: false,
            merged: false,
            refusal_reason: Some(EnqueueRefusalReason::LastItemDedup),
            persistence_error: None,
        };
        assert_eq!(
            classify_phase2_enqueue_commit(&last_item_dedup),
            Phase2EnqueueCommit::Deferred
        );

        let persistence_failed = super::super::MailboxEnqueueOutcome {
            enqueued: true,
            merged: false,
            refusal_reason: None,
            persistence_error: Some("disk unavailable".to_string()),
        };
        assert_eq!(
            classify_phase2_enqueue_commit(&persistence_failed),
            Phase2EnqueueCommit::Deferred
        );
    }
}
