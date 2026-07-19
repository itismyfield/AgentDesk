//! #3479 item-2 giant-file decomposition: catch-up subsystem extracted
//! verbatim from `discord/mod.rs`. Startup/restart-gap message recovery —
//! REST-scans configured & checkpointed channels for messages that arrived
//! during the restart window, classifies them, and enqueues the eligible ones.
//! Behavior-preserving move only; logic is unchanged.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{
    INTERVENTION_DEDUP_WINDOW, SourceMessageQueuedGeneration,
};

use super::*;

const CATCH_UP_RETRY_QUEUE_THRESHOLD: usize = MAX_INTERVENTIONS_PER_CHANNEL / 2;
const CATCH_UP_RETRY_FETCH_FAILURE_LIMIT: u8 = 4;
// #4156: a channel whose fetch keeps SUCCEEDING but whose messages keep being
// Deferred (persistently busy / repeated ActorUnreachable) re-arms the catch-up
// retry every scan. Without a bound that cycle is unbounded and unlogged. Cap
// the consecutive Deferred re-arms (independently of the fetch-failure budget)
// and emit a giving-up WARN at the cap, mirroring the fetch-failure path.
const CATCH_UP_RETRY_DEFERRED_REARM_LIMIT: u8 = 8;

fn catch_up_source_generation(
    message_id: MessageId,
    queued_generation: u64,
    author_id: u64,
    author_is_bot: bool,
    allowed_bot_ids: &[u64],
    announce_resolution: health::UtilityBotUserIdResolution,
) -> SourceMessageQueuedGeneration {
    let announce_identity_excludes_human = match announce_resolution {
        health::UtilityBotUserIdResolution::Resolved(announce_bot_id) => {
            announce_bot_id == author_id
        }
        health::UtilityBotUserIdResolution::Unconfigured => false,
        health::UtilityBotUserIdResolution::Unavailable => true,
    };
    let is_genuine_human = !author_is_bot
        && !allowed_bot_ids.contains(&author_id)
        && !announce_identity_excludes_human;

    if is_genuine_human {
        SourceMessageQueuedGeneration::user_instruction(message_id, queued_generation)
    } else {
        SourceMessageQueuedGeneration::new(message_id, queued_generation)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services) struct CatchUpRetryState {
    checkpoint: u64,
    fetch_failures: u8,
    // #4156: consecutive Deferred re-arms (fetch succeeded, enqueue deferred).
    // Tracked separately from `fetch_failures` because a Deferred re-arm is not
    // a fetch error; both budgets share the merge's most-exhausted semantics.
    deferred_rearms: u8,
    armed_at: Instant,
}

impl CatchUpRetryState {
    fn new(checkpoint: u64) -> Self {
        Self {
            checkpoint,
            fetch_failures: 0,
            deferred_rearms: 0,
            armed_at: Instant::now(),
        }
    }

    fn after_fetch_failure(self) -> Option<Self> {
        let fetch_failures = self.fetch_failures.saturating_add(1);
        (fetch_failures <= CATCH_UP_RETRY_FETCH_FAILURE_LIMIT).then_some(Self {
            checkpoint: self.checkpoint,
            fetch_failures,
            deferred_rearms: self.deferred_rearms,
            armed_at: self.armed_at,
        })
    }

    // #4156: advance the Deferred re-arm budget. Returns `None` once the cap is
    // exhausted so the caller stops re-arming (the backlog then ages out or a
    // fresh catch-up trigger restarts the cycle), matching `after_fetch_failure`.
    fn after_deferred_rearm(self, checkpoint: u64) -> Option<Self> {
        let deferred_rearms = self.deferred_rearms.saturating_add(1);
        (deferred_rearms <= CATCH_UP_RETRY_DEFERRED_REARM_LIMIT).then_some(Self {
            checkpoint,
            fetch_failures: self.fetch_failures,
            deferred_rearms,
            // Preserve the original arm time so the arm-time age window
            // (`catch_up_message_age_reference_time`) is NOT reset each cycle.
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

    fn enqueue_too_old_notice(
        &self,
        pool: Option<sqlx::PgPool>,
        request: CatchUpTooOldOutboxRequest,
    ) -> Option<tokio::task::JoinHandle<()>> {
        pool.map(|pool| too_old_notice::spawn_outbox(pool, request))
    }

    fn record_too_old_dead_letter(
        &self,
        pool: Option<&sqlx::PgPool>,
        record: crate::db::relay_dead_letter::RelayDeadLetterRecord,
    ) -> Option<tokio::task::JoinHandle<()>> {
        crate::db::relay_dead_letter::record_detached(pool, record)
    }

    async fn utility_bot_user_ids(
        &self,
        shared: &SharedData,
    ) -> (
        health::UtilityBotUserIdResolution,
        health::UtilityBotUserIdResolution,
    ) {
        let Some(registry) = shared.health_registry() else {
            return (
                health::UtilityBotUserIdResolution::Unconfigured,
                health::UtilityBotUserIdResolution::Unconfigured,
            );
        };
        (
            registry
                .utility_bot_user_id_resolution(super::bot_role::UtilityBotRole::Announce)
                .await,
            registry
                .utility_bot_user_id_resolution(super::bot_role::UtilityBotRole::Notify)
                .await,
        )
    }
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
mod settled_ledger_consult;
mod too_old_notice;

#[cfg(test)]
#[path = "catch_up/classification_order_tests.rs"]
mod classification_order_tests;

use classification::{
    CatchUpClassification, CatchUpClassificationDecision, CatchUpMessageView, CatchUpScanStats,
    classify_catch_up_message, classify_catch_up_message_with_utility_resolution,
    classify_phase2_message_with_utility_resolution,
};
#[cfg(test)]
use phase2::catch_up_enqueue_accepted;
use phase2::{
    Phase2EnqueueCommit, Phase2RecoveryStats, advance_phase2_checkpoint,
    catch_up_remaining_queue_capacity, classify_phase2_enqueue_commit,
    log_catch_up_enqueue_not_accepted, phase2_retry_after_checkpoint,
};
use too_old_notice::{
    CATCH_UP_TOO_OLD_NOTICE_MAX_ITEMS, CatchUpTooOldDrop, CatchUpTooOldOutboxRequest,
    actionable_drop as catch_up_too_old_drop, catch_up_too_old_snippet,
    notice as catch_up_too_old_notice,
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
    arm_catch_up_retry_state(shared, channel_id, CatchUpRetryState::new(retry_after)).checkpoint
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
        // #4156: same most-exhausted rule for the Deferred re-arm budget.
        deferred_rearms: existing.deferred_rearms.max(retry_state.deferred_rearms),
        armed_at: existing.armed_at.min(retry_state.armed_at),
    }
}

fn collect_catch_up_retry_pending_channels(shared: &SharedData) -> HashSet<ChannelId> {
    shared
        .catch_up_retry_pending
        .iter()
        .map(|entry| *entry.key())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatchUpRetryScanDecision {
    Proceed(Option<CatchUpRetryState>),
    SkipConsumed,
}

fn consume_catch_up_retry_state_for_scan(
    shared: &SharedData,
    channel_id: ChannelId,
    retry_checkpoints: &HashMap<ChannelId, CatchUpRetryState>,
    pending_retry_channels: &HashSet<ChannelId>,
) -> CatchUpRetryScanDecision {
    let mut retry_state = retry_checkpoints.get(&channel_id).copied();
    if !pending_retry_channels.contains(&channel_id) {
        return CatchUpRetryScanDecision::Proceed(retry_state);
    }

    let Some((_, pending_retry_state)) = shared.catch_up_retry_pending.remove(&channel_id) else {
        // A concurrent drain-path take consumed the pending entry. A caller-
        // supplied `retry_checkpoints` entry was handed to THIS scan though —
        // losing the DashMap race must not discard it.
        return match retry_state {
            Some(state) => CatchUpRetryScanDecision::Proceed(Some(state)),
            None => CatchUpRetryScanDecision::SkipConsumed,
        };
    };
    retry_state = Some(merge_catch_up_retry_state(retry_state, pending_retry_state));
    CatchUpRetryScanDecision::Proceed(retry_state)
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

// #4156: re-arm the catch-up retry after a fetch-succeeded-but-enqueue-Deferred
// scan, carrying forward the prior scan's Deferred budget + original arm time
// (`prior`, the state consumed for THIS scan). Returns the armed checkpoint, or
// `None` once the Deferred re-arm cap is exhausted — at which point it emits a
// giving-up WARN and does NOT re-arm, so a persistently-busy channel can no
// longer spin an unbounded, unlogged re-arm cycle.
fn rearm_catch_up_retry_after_defer(
    shared: &SharedData,
    channel_id: ChannelId,
    retry_after: u64,
    threaded_prior: Option<CatchUpRetryState>,
) -> Option<u64> {
    // The prior Deferred budget can live in the state consumed for THIS scan
    // (both phases thread it in via `threaded_prior`: phase-1 from the entry it
    // just removed, phase-2 from `consumed_retry_states_this_cycle`) AND/OR still
    // in the pending map (e.g. phase-1 re-armed before phase-2 ran). Take the
    // most-exhausted of the two so neither phase resets the other's budget.
    let map_prior = shared
        .catch_up_retry_pending
        .get(&channel_id)
        .map(|entry| *entry.value());
    let prior = match (threaded_prior, map_prior) {
        (Some(threaded), Some(mapped)) => Some(merge_catch_up_retry_state(Some(threaded), mapped)),
        (Some(state), None) | (None, Some(state)) => Some(state),
        (None, None) => None,
    };
    let base = prior.unwrap_or_else(|| CatchUpRetryState::new(retry_after));
    let ts = chrono::Local::now().format("%H:%M:%S");
    let attempted_rearms = base.deferred_rearms.saturating_add(1);
    let Some(next_retry_state) = base.after_deferred_rearm(retry_after) else {
        tracing::warn!(
            "  [{ts}] ⚠ catch-up: channel {} deferred {} time(s); giving up after {} re-arm(s) at checkpoint {}",
            channel_id,
            attempted_rearms,
            CATCH_UP_RETRY_DEFERRED_REARM_LIMIT,
            retry_after
        );
        return None;
    };
    let rearmed = arm_catch_up_retry_state(shared, channel_id, next_retry_state);
    Some(rearmed.checkpoint)
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

fn prune_stale_checkpoint_files(dir: &Path, max_checkpoint_age: std::time::Duration) -> usize {
    let mut pruned = 0usize;
    if let Ok(prune_entries) = fs::read_dir(dir) {
        for entry in prune_entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("txt") {
                continue;
            }
            let Ok(meta) = path.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let Ok(modified) = meta.modified() else {
                continue;
            };
            if modified.elapsed().unwrap_or_default() <= max_checkpoint_age {
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                pruned += 1;
            }
        }
    }
    pruned
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecentPageDecision {
    FetchOlder,
    Complete,
    IncompleteBudget,
}

fn recent_page_decision(
    pages_fetched: u8,
    oldest_age_secs: Option<i64>,
    max_age_secs: i64,
) -> RecentPageDecision {
    match oldest_age_secs {
        None => RecentPageDecision::Complete,
        Some(age) if age > max_age_secs => RecentPageDecision::Complete,
        Some(_) if pages_fetched >= CATCH_UP_RECENT_MAX_PAGES => {
            RecentPageDecision::IncompleteBudget
        }
        Some(_) => RecentPageDecision::FetchOlder,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatchUpFetchCompleteness {
    Complete,
    Incomplete,
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

/// #4443: leading marker of the aggregate restart-gap notice. Shared between
/// the notice builder and the catch-up classifiers so a reworded notice cannot
/// silently break the self-recollection guard.
const CATCH_UP_TOO_OLD_NOTICE_PREFIX: &str = "⚠️ 재시작 공백으로";

/// #4443: true when a message is our own restart-gap notice reposted through
/// an allowed sender bot. Both catch-up phases must classify these out:
/// re-collecting one nests it inside the next notice (one level per restart,
/// every channel) and phase2 would hand a young one to the agent as input.
/// Prefix + bot-author scoped so a human quoting the marker still recovers.
fn is_restart_gap_notice(author_is_bot: bool, text: &str) -> bool {
    author_is_bot && text.starts_with(CATCH_UP_TOO_OLD_NOTICE_PREFIX)
}

fn advance_catch_up_settled_frontier(frontier: Option<u64>, message_id: u64) -> Option<u64> {
    Some(frontier.map_or(message_id, |settled| settled.max(message_id)))
}

fn catch_up_intervention_created_at(
    scan_wall_time: chrono::DateTime<chrono::Utc>,
    scan_instant: Instant,
    message_id: MessageId,
) -> Instant {
    let message_created_at = message_id.created_at();
    match scan_wall_time
        .signed_duration_since(*message_created_at)
        .to_std()
    {
        Ok(age) => scan_instant.checked_sub(age).unwrap_or(scan_instant),
        Err(_) => scan_instant,
    }
}

fn catch_up_message_id_gap(last_id: MessageId, current_id: MessageId) -> Option<Duration> {
    let last_created_at = last_id.created_at();
    let current_created_at = current_id.created_at();
    current_created_at
        .signed_duration_since(*last_created_at)
        .to_std()
        .ok()
}

fn catch_up_last_item_dedup_is_checkpoint_safe(
    last: Option<&Intervention>,
    message_id: MessageId,
) -> bool {
    last.and_then(|last| catch_up_message_id_gap(last.message_id, message_id))
        .is_some_and(|gap| gap <= INTERVENTION_DEDUP_WINDOW)
}

/// Startup catch-up polling: fetch messages that arrived during the catch-up
/// gap. Uses saved last_message_ids to query Discord REST API, classifies
/// sender eligibility/age/duplicates, and enqueues only safe recoveries.
pub(in crate::services::discord) async fn catch_up_missed_messages(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let pending_retry_channels = collect_catch_up_retry_pending_channels(shared);
    let pending_count = pending_retry_channels.len();
    if pending_count > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔁 catch-up: sweep observed {pending_count} pending retry checkpoint(s)"
        );
    }
    let api = SerenityCatchUpDiscordApi { http };
    run_catch_up_sweep(
        CatchUpDeps::new(&api, shared, provider)
            .with_pending_retry_channels(&pending_retry_channels),
    )
    .await;
}

/// #4165 queue-drain retry entry: the same sweep as [`catch_up_missed_messages`]
/// but seeded with caller-owned retry checkpoints (and no pending-channel
/// collection) so a single channel can be re-scanned after its queue drains.
pub(in crate::services::discord) async fn catch_up_missed_messages_for_retry(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    retry_checkpoints: &HashMap<ChannelId, CatchUpRetryState>,
) {
    let api = SerenityCatchUpDiscordApi { http };
    run_catch_up_sweep(
        CatchUpDeps::new(&api, shared, provider).with_retry_checkpoints(retry_checkpoints),
    )
    .await;
}

/// #4233: injected dependencies for one catch-up sweep. Collapses the former
/// 4-level wrapper chain (`catch_up_missed_messages` → `_inner` →
/// `_inner_with_api` → `_inner_with_api_and_pending_retry_channels`), where each
/// added injection point spawned a longer function name. Production builds this
/// via [`CatchUpDeps::new`] (empty retry/pending defaults); the retry-drain
/// entry and tests override the api / retry-checkpoint / pending-channel seams
/// with the `with_*` builders.
struct CatchUpDeps<'a, A: CatchUpDiscordApi + ?Sized> {
    api: &'a A,
    shared: &'a Arc<SharedData>,
    provider: &'a ProviderKind,
    retry_checkpoints: &'a HashMap<ChannelId, CatchUpRetryState>,
    pending_retry_channels: &'a HashSet<ChannelId>,
}

/// Empty defaults so [`CatchUpDeps::new`] can hand out `'static` references for
/// the common "no injected retry checkpoints / no pending channels" path.
static CATCH_UP_EMPTY_RETRY_CHECKPOINTS: LazyLock<HashMap<ChannelId, CatchUpRetryState>> =
    LazyLock::new(HashMap::new);
static CATCH_UP_EMPTY_PENDING_RETRY_CHANNELS: LazyLock<HashSet<ChannelId>> =
    LazyLock::new(HashSet::new);

impl<'a, A: CatchUpDiscordApi + ?Sized> CatchUpDeps<'a, A> {
    /// Default seam: no injected retry checkpoints and no pending retry
    /// channels. Override with the `with_*` builders below.
    fn new(api: &'a A, shared: &'a Arc<SharedData>, provider: &'a ProviderKind) -> Self {
        Self {
            api,
            shared,
            provider,
            retry_checkpoints: &CATCH_UP_EMPTY_RETRY_CHECKPOINTS,
            pending_retry_channels: &CATCH_UP_EMPTY_PENDING_RETRY_CHANNELS,
        }
    }

    /// Inject the caller-owned per-channel retry checkpoints for this sweep
    /// (queue-drain retry entry + retry-mode tests).
    fn with_retry_checkpoints(
        mut self,
        retry_checkpoints: &'a HashMap<ChannelId, CatchUpRetryState>,
    ) -> Self {
        self.retry_checkpoints = retry_checkpoints;
        self
    }

    /// Inject the channels observed carrying a pending retry checkpoint
    /// (startup sweep + pending-map race tests).
    fn with_pending_retry_channels(
        mut self,
        pending_retry_channels: &'a HashSet<ChannelId>,
    ) -> Self {
        self.pending_retry_channels = pending_retry_channels;
        self
    }
}

async fn run_catch_up_sweep<A: CatchUpDiscordApi + ?Sized>(deps: CatchUpDeps<'_, A>) {
    let CatchUpDeps {
        api,
        shared,
        provider,
        retry_checkpoints,
        pending_retry_channels,
    } = deps;
    let Some(root) = runtime_store::last_message_root() else {
        return;
    };
    let dir = root.join(provider.as_str());

    let mut total_recovered = 0usize;
    let scan_instant = Instant::now();
    let scan_wall_time = chrono::Utc::now();
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
    let pruned = prune_stale_checkpoint_files(&dir, max_checkpoint_age);
    if pruned > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] 🧹 catch-up: pruned {pruned} stale checkpoint(s) (>10min old)");
    }

    let mut candidates = collect_catch_up_channel_candidates(&dir, provider);
    for channel_id in retry_checkpoints
        .keys()
        .copied()
        .chain(pending_retry_channels.iter().copied())
    {
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

    // #4156: phase-1 CONSUMES (removes) each channel's pending retry entry
    // before scanning, so by the time phase-2 runs later in the same cycle the
    // Deferred budget no longer lives in the pending map. Stash the consumed
    // state per channel here so phase-2's re-arm can recover the accumulated
    // budget instead of resetting it to 1 (which would leave phase-2-origin
    // defers in an unbounded, unlogged re-arm cycle).
    let mut consumed_retry_states_this_cycle: HashMap<ChannelId, CatchUpRetryState> =
        HashMap::new();
    // Phase 2 must not bypass an incomplete unbounded Recent scan and persist a
    // newer recovery across the same unknown lower gap.
    let mut incomplete_recent_channels: HashSet<ChannelId> = HashSet::new();

    // Pace successive per-channel REST scans so a many-channel sweep doesn't
    // fire as one tight burst (see `catch_up_scan_pace`). The first eligible
    // channel runs immediately; subsequent scans wait the configured gap.
    let mut paced_scan = false;
    for candidate in candidates.values() {
        let channel_id = candidate.channel_id;

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

        let retry_state = match consume_catch_up_retry_state_for_scan(
            shared,
            channel_id,
            retry_checkpoints,
            pending_retry_channels,
        ) {
            CatchUpRetryScanDecision::Proceed(retry_state) => retry_state,
            CatchUpRetryScanDecision::SkipConsumed => continue,
        };
        // #4156: remember the budget this scan consumed so phase-2 can carry it
        // forward even when phase-1 does not itself re-arm (which would leave the
        // pending map empty by phase-2 time).
        if let Some(state) = retry_state {
            consumed_retry_states_this_cycle.insert(channel_id, state);
        }
        paced_scan = true;
        let retry_checkpoint = retry_state.map(|state| state.checkpoint);
        let live_checkpoint = shared.last_message_ids.get(&channel_id).map(|entry| *entry);
        let fetch_mode = catch_up_fetch_mode_for_scan(candidate, live_checkpoint, retry_checkpoint);
        let scan_checkpoint = fetch_mode.checkpoint();

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
                // A transient binding-resolution failure happened before the
                // unbounded Recent gap could be read at all. Do not let phase 2
                // establish a newer durable frontier across that unknown lower
                // bound in the same sweep. `After` has a precise lower bound and
                // retains its existing independent phase-2 behavior.
                if matches!(fetch_mode, CatchUpFetchMode::Recent) {
                    incomplete_recent_channels.insert(channel_id);
                }
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
                // The first Recent page is part of the same unbounded
                // pagination proof as every backward page below. If it fails,
                // phase 2 must not recover a newer last-20 item and turn the
                // unknown lower gap into `After(newer)` in this sweep.
                if matches!(fetch_mode, CatchUpFetchMode::Recent) {
                    incomplete_recent_channels.insert(channel_id);
                }
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
        let mut fetch_completeness = CatchUpFetchCompleteness::Complete;
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
                match recent_page_decision(
                    pages_fetched,
                    Some(oldest_age_secs),
                    max_age.as_secs() as i64,
                ) {
                    RecentPageDecision::Complete => break,
                    RecentPageDecision::IncompleteBudget => {
                        fetch_completeness = CatchUpFetchCompleteness::Incomplete;
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ catch-up: Recent pagination budget exhausted for channel {channel_id}; preserving the unbounded gap"
                        );
                        break;
                    }
                    RecentPageDecision::FetchOlder => {}
                }
                if should_pace_before_scan(true) {
                    catch_up_scan_pace_gap().await;
                }
                let older_request = serenity::builder::GetMessages::new()
                    .limit(CATCH_UP_FETCH_LIMIT)
                    .before(oldest_id);
                match api.fetch_messages(channel_id, older_request).await {
                    Ok(mut older) if !older.is_empty() => {
                        // A non-advancing/overlapping REST page is not proof that
                        // the unbounded Recent gap is complete. Keep only ids
                        // below the requested cursor and retry on a later sweep
                        // if the cursor did not move at all.
                        older.retain(|message| message.id.get() < oldest_id.get());
                        if older.is_empty() {
                            fetch_completeness = CatchUpFetchCompleteness::Incomplete;
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ catch-up: Recent pagination cursor did not advance for channel {channel_id}; preserving the unbounded gap"
                            );
                            break;
                        }
                        pages_fetched += 1;
                        messages.extend(older);
                    }
                    Ok(_) => break, // no older messages → done
                    Err(e) => {
                        fetch_completeness = CatchUpFetchCompleteness::Incomplete;
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ catch-up: backward page fetch failed for channel {channel_id}: {e}"
                        );
                        break;
                    }
                }
            }
        }

        // A partial newest-first Recent prefix is not contiguous with the
        // unknown lower bound. Processing it could durably checkpoint terminal
        // noise past an unseen recoverable message, so leave the whole batch to
        // the periodic Recent backstop instead.
        if fetch_completeness == CatchUpFetchCompleteness::Incomplete {
            incomplete_recent_channels.insert(channel_id);
            continue;
        }

        if messages.is_empty() {
            continue;
        }
        messages.sort_by_key(|msg| msg.id.get());

        // Get bot's own user ID to filter out self-messages
        // Collect existing message IDs in queue for dedup
        let existing_ids = recovery_known_message_ids(&mailbox_snapshot(shared, channel_id).await);
        // #4564: the durable completed-turn ledger for this channel, read once per
        // scan (mirrors `existing_ids`). Suppresses the false restart-gap TooOld
        // notice for inbound messages that already reached terminal delivery.
        let settled_ids = settled_ledger_consult::settled_ids(provider, channel_id);

        let allowed_bot_ids: Vec<u64> = {
            let settings = shared.settings.read().await;
            settings.allowed_bot_ids.clone()
        };
        let (announce_resolution, notify_resolution) = api.utility_bot_user_ids(shared).await;
        let announce_bot_id = announce_resolution.user_id();
        let notify_bot_id = notify_resolution.user_id();
        // Newest message that this oldest-first scan has durably settled.
        // Non-recover classifications settle immediately; Recover settles only
        // after an accepted/safe-dedup enqueue. The current cap/defer item never
        // enters this frontier, so persisting it cannot skip recoverable work.
        let mut max_settled_id: Option<u64> = None;
        let mut stats = CatchUpScanStats::default();
        stats.returned = messages.len();
        // #4260/#4453: actionable human TooOld drops accumulated for one
        // aggregate resend notice. Bot TooOld rows stay internal DLQ evidence.
        let mut too_old_drops: Vec<CatchUpTooOldDrop> = Vec::new();
        // Newest actionable human TooOld id, retained only for the aggregate
        // notice's batch-specific outbox dedupe key. Automation never owns a
        // user-facing batch identity.
        let mut max_actionable_too_old_id: Option<u64> = None;

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
            let outcome = match classify_catch_up_message_with_utility_resolution(
                &view,
                current_bot_user_id,
                &existing_ids,
                &settled_ids,
                max_age.as_secs() as i64,
                &allowed_bot_ids,
                announce_resolution,
                notify_resolution,
            ) {
                CatchUpClassificationDecision::Determinate(outcome) => outcome,
                CatchUpClassificationDecision::UtilityIdentityUnavailable => {
                    let mid = msg.id.get();
                    let retry_after = max_settled_id
                        .or(scan_checkpoint)
                        .unwrap_or_else(|| mid.saturating_sub(1));
                    let retry_after = rearm_catch_up_retry_after_defer(
                        shared,
                        channel_id,
                        retry_after,
                        retry_state,
                    );
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        channel_id = channel_id.get(),
                        message_id = mid,
                        retry_after,
                        "  [{ts}] 🔁 catch-up: utility bot identity unavailable; preserving checkpoint before ambiguous message"
                    );
                    break;
                }
            };
            let mid = msg.id.get();
            // Codex P2 round 2 on #1301: check the cap BEFORE recording the
            // recover, otherwise `stats.recovered` would tally a message we
            // refused to enqueue and the log would lie about the queue
            // contents. Stopping iteration keeps the checkpoint pinned at
            // the last actually-queued message — newer entries that we
            // declined are still > `after_msg` for the next pass.
            if outcome == CatchUpClassification::Recover && stats.recovered >= remaining_capacity {
                let retry_after = max_settled_id
                    .or(scan_checkpoint)
                    .unwrap_or_else(|| mid.saturating_sub(1));
                let retry_after = arm_catch_up_retry_pending(shared, channel_id, retry_after);
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔁 catch-up: queue cap reached for channel {}; retry armed after checkpoint {}",
                    channel_id,
                    retry_after
                );
                break;
            }
            if outcome != CatchUpClassification::Recover {
                if outcome == CatchUpClassification::TooOld {
                    // Preserve every TooOld input as detached DLQ evidence;
                    // only semantically human senders enter the resend notice.
                    let _ = api.record_too_old_dead_letter(
                        shared.pg_pool.as_ref(),
                        crate::db::relay_dead_letter::RelayDeadLetterRecord {
                            kind: crate::db::relay_dead_letter::KIND_CATCH_UP_TOO_OLD.to_string(),
                            channel_id: channel_id.to_string(),
                            author_id: Some(msg.author.id.get().to_string()),
                            message_id: Some(mid.to_string()),
                            content: text.clone(),
                            reason: format!(
                                "age_secs={age_secs} > max_age_secs={}",
                                max_age.as_secs()
                            ),
                        },
                    );
                    if let Some(drop) = catch_up_too_old_drop(
                        outcome,
                        msg.author.id.get(),
                        msg.author.bot,
                        &allowed_bot_ids,
                        announce_bot_id,
                        notify_bot_id,
                        &text,
                    ) {
                        max_actionable_too_old_id = Some(
                            max_actionable_too_old_id.map_or(mid, |actionable| actionable.max(mid)),
                        );
                        too_old_drops.push(drop);
                    }
                }
                max_settled_id = advance_catch_up_settled_frontier(max_settled_id, mid);
                stats.record(outcome);
                continue;
            }

            let queued_generation = crate::services::discord::runtime_store::load_generation();
            let source_generation = catch_up_source_generation(
                msg.id,
                queued_generation,
                msg.author.id.get(),
                msg.author.bot,
                &allowed_bot_ids,
                announce_resolution,
            );
            let enqueue = mailbox_enqueue_intervention(
                shared,
                provider,
                channel_id,
                Intervention {
                    author_id: msg.author.id,
                    author_is_bot: msg.author.bot,
                    message_id: msg.id,
                    queued_generation,
                    source_message_ids: vec![msg.id],
                    source_message_queued_generations: vec![source_generation],
                    source_text_segments: Vec::new(),
                    text: text.clone(),
                    mode: InterventionMode::Soft,
                    created_at: catch_up_intervention_created_at(
                        scan_wall_time,
                        scan_instant,
                        msg.id,
                    ),
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
                    stats.record(CatchUpClassification::Recover);
                    api.cleanup_recovered_catch_up_hourglass(shared, channel_id, msg.id)
                        .await;
                    max_settled_id = advance_catch_up_settled_frontier(max_settled_id, mid);
                }
                Phase2EnqueueCommit::Duplicate => {
                    stats.record(CatchUpClassification::Duplicate);
                    max_settled_id = advance_catch_up_settled_frontier(max_settled_id, mid);
                }
                Phase2EnqueueCommit::LastItemDedup => {
                    let snapshot = mailbox_snapshot(shared, channel_id).await;
                    if catch_up_last_item_dedup_is_checkpoint_safe(
                        snapshot.intervention_queue.last(),
                        msg.id,
                    ) {
                        stats.record(CatchUpClassification::Duplicate);
                        max_settled_id = advance_catch_up_settled_frontier(max_settled_id, mid);
                    } else {
                        log_catch_up_enqueue_not_accepted("phase1", channel_id, msg.id, &enqueue);
                        let retry_after = max_settled_id
                            .or(scan_checkpoint)
                            .unwrap_or_else(|| mid.saturating_sub(1));
                        rearm_catch_up_retry_after_defer(
                            shared,
                            channel_id,
                            retry_after,
                            retry_state,
                        );
                        break;
                    }
                }
                Phase2EnqueueCommit::Deferred => {
                    log_catch_up_enqueue_not_accepted("phase1", channel_id, msg.id, &enqueue);
                    let retry_after = max_settled_id
                        .or(scan_checkpoint)
                        .unwrap_or_else(|| mid.saturating_sub(1));
                    rearm_catch_up_retry_after_defer(shared, channel_id, retry_after, retry_state);
                    break;
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
                 (returned={} self={} dup={} too_old={} settled={} empty={} not_allowed={} system={})",
                stats.recovered,
                channel_id,
                stats.returned,
                stats.self_authored,
                stats.duplicate,
                stats.too_old,
                stats.settled,
                stats.empty,
                stats.not_allowed,
                stats.system_kind,
            );
        } else {
            tracing::info!(
                "  [{ts}] 🔍 catch-up scan: channel={} returned={} bot={} dup={} \
                 too_old={} settled={} empty={} not_allowed={} system={} recovered=0",
                channel_id,
                stats.returned,
                stats.self_authored,
                stats.duplicate,
                stats.too_old,
                stats.settled,
                stats.empty,
                stats.not_allowed,
                stats.system_kind,
            );
        }

        // Persist the contiguous settled frontier even when a retry is pending.
        // The current cap/defer item was deliberately not folded in, so this
        // retires permanent skips without crossing work that still needs retry.
        if let Some(newest) = max_settled_id {
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

        // #4260/#4453: one aggregate human-resend notice per channel/run. Bot
        // TooOld inputs stay internal; the newest human id owns batch dedupe.
        if let Some(notice) = catch_up_too_old_notice(&too_old_drops) {
            let batch_id = max_actionable_too_old_id.unwrap_or_default();
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                channel_id = channel_id.get(),
                too_old = too_old_drops.len(),
                "  [{ts}] ⚠ catch-up: dropped too-old message(s); aggregate resend notice enqueued"
            );
            let _ = api.enqueue_too_old_notice(
                shared.pg_pool.clone(),
                CatchUpTooOldOutboxRequest {
                    target: format!("channel:{channel_id}"),
                    content: notice,
                    bot: super::bot_role::UtilityBotRole::Notify.alias(),
                    source: "catch_up_too_old",
                    reason_code: "catch_up.too_old",
                    session_key: format!("catch_up_too_old:{channel_id}:{batch_id}"),
                },
            );
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
    let (announce_resolution_phase2, notify_resolution_phase2) =
        api.utility_bot_user_ids(shared).await;

    // Same per-channel pacing as phase 1 (see `catch_up_scan_pace`).
    let mut paced_scan_phase2 = false;
    for candidate in candidates.values() {
        let channel_id = candidate.channel_id;

        if incomplete_recent_channels.contains(&channel_id) {
            continue;
        }

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
        // #4564: same durable completed-turn ledger consult as phase 1, read once
        // per channel. A Settled outcome in phase 2 simply skips (no enqueue, no
        // notice) — an already-answered message must not be re-surfaced.
        let settled_ids = settled_ledger_consult::settled_ids(provider, channel_id);
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
            if is_restart_gap_notice(msg.author.bot, text) {
                // #4443: phase2's 10-minute window would otherwise hand a
                // young notice straight to the agent after every deploy.
                stats.skipped += 1;
                continue;
            }
            let mid = msg.id.get();
            let msg_age = chrono::Utc::now().signed_duration_since(*msg.id.created_at());
            // These gates are final regardless of announce/notify identity.
            // Apply them before the phase-1 counterfactual, whose actionable
            // TooOld bit is intentionally irrelevant to phase 2.
            if existing_ids.contains(&mid) {
                stats.duplicate += 1;
                phase2_checkpoint = advance_phase2_checkpoint(phase2_checkpoint, mid);
                continue;
            }
            if phase2_checkpoint.is_some_and(|saved| mid <= saved) {
                stats.skipped += 1;
                continue;
            }
            if msg_age.num_seconds() > 600 {
                stats.skipped += 1;
                continue;
            }
            let utility_view = CatchUpMessageView {
                message_id: mid,
                author_id: msg.author.id.get(),
                author_is_bot: msg.author.bot,
                is_processable_kind: true,
                age_secs: msg_age.num_seconds(),
                trimmed_text: text.to_string(),
            };
            let author_is_authorized = {
                let settings = shared.settings.read().await;
                discord_io::user_is_authorized(&settings, msg.author.id.get())
            };
            match classify_phase2_message_with_utility_resolution(
                &utility_view,
                current_bot_user_id,
                &existing_ids,
                &settled_ids,
                600,
                &allowed_bot_ids_phase2,
                announce_resolution_phase2,
                notify_resolution_phase2,
                author_is_authorized,
            ) {
                CatchUpClassificationDecision::UtilityIdentityUnavailable => {
                    let retry_after = phase2_retry_after_checkpoint(
                        max_recovered_id,
                        phase2_checkpoint,
                        last_bot_response_id,
                    );
                    phase2_retry_after = rearm_catch_up_retry_after_defer(
                        shared,
                        channel_id,
                        retry_after,
                        consumed_retry_states_this_cycle.get(&channel_id).copied(),
                    );
                    stats.deferred += 1;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        channel_id = channel_id.get(),
                        message_id = mid,
                        retry_after = phase2_retry_after,
                        "  [{ts}] 🔁 catch-up phase2: utility bot identity unavailable; preserving checkpoint before ambiguous message"
                    );
                    break;
                }
                CatchUpClassificationDecision::Determinate(CatchUpClassification::Recover) => {}
                CatchUpClassificationDecision::Determinate(_) => {
                    stats.skipped += 1;
                    continue;
                }
            }
            stats.eligible += 1;
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

            let queued_generation = crate::services::discord::runtime_store::load_generation();
            let source_generation = catch_up_source_generation(
                msg.id,
                queued_generation,
                msg.author.id.get(),
                msg.author.bot,
                &allowed_bot_ids_phase2,
                announce_resolution_phase2,
            );
            let enqueue = mailbox_enqueue_intervention(
                shared,
                provider,
                channel_id,
                Intervention {
                    author_id: msg.author.id,
                    author_is_bot: msg.author.bot,
                    message_id: msg.id,
                    queued_generation,
                    source_message_ids: vec![msg.id],
                    source_message_queued_generations: vec![source_generation],
                    source_text_segments: Vec::new(),
                    text: text.to_string(),
                    mode: InterventionMode::Soft,
                    created_at: catch_up_intervention_created_at(
                        scan_wall_time,
                        scan_instant,
                        msg.id,
                    ),
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
                Phase2EnqueueCommit::LastItemDedup => {
                    let snapshot = mailbox_snapshot(shared, channel_id).await;
                    if catch_up_last_item_dedup_is_checkpoint_safe(
                        snapshot.intervention_queue.last(),
                        msg.id,
                    ) {
                        existing_ids.insert(mid);
                        phase2_checkpoint = advance_phase2_checkpoint(phase2_checkpoint, mid);
                        stats.duplicate += 1;
                    } else {
                        log_catch_up_enqueue_not_accepted("phase2", channel_id, msg.id, &enqueue);
                        let retry_after = phase2_retry_after_checkpoint(
                            max_recovered_id,
                            phase2_checkpoint,
                            last_bot_response_id,
                        );
                        phase2_retry_after = rearm_catch_up_retry_after_defer(
                            shared,
                            channel_id,
                            retry_after,
                            // #4156: recover the budget phase-1 consumed this cycle
                            // (the pending map entry is gone by now); the helper
                            // merges it with any surviving map entry (most-exhausted).
                            consumed_retry_states_this_cycle.get(&channel_id).copied(),
                        );
                        stats.deferred += 1;
                        break;
                    }
                }
                Phase2EnqueueCommit::Deferred => {
                    log_catch_up_enqueue_not_accepted("phase2", channel_id, msg.id, &enqueue);
                    let retry_after = phase2_retry_after_checkpoint(
                        max_recovered_id,
                        phase2_checkpoint,
                        last_bot_response_id,
                    );
                    phase2_retry_after = rearm_catch_up_retry_after_defer(
                        shared,
                        channel_id,
                        retry_after,
                        consumed_retry_states_this_cycle.get(&channel_id).copied(),
                    );
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
        CATCH_UP_RECENT_MAX_PAGES, CATCH_UP_RETRY_DEFERRED_REARM_LIMIT,
        CATCH_UP_RETRY_FETCH_FAILURE_LIMIT, CATCH_UP_SCAN_PACE_DEFAULT_MS, CatchUpChannelCandidate,
        CatchUpClassification, CatchUpDeps, CatchUpDiscordApi, CatchUpFetchMode,
        CatchUpMessageView, CatchUpRetryScanDecision, CatchUpRetryState, CatchUpTooOldDrop,
        ChannelId, MessageId, Phase2EnqueueCommit, ProviderKind, RecentPageDecision,
        RuntimeChannelBindingStatus, advance_phase2_checkpoint, arm_catch_up_retry_pending,
        catch_up_enqueue_accepted, catch_up_fetch_mode_for_scan, catch_up_intervention_created_at,
        catch_up_last_item_dedup_is_checkpoint_safe, catch_up_message_age_reference_time,
        catch_up_remaining_queue_capacity, catch_up_too_old_notice, catch_up_too_old_snippet,
        classify_catch_up_message, classify_phase2_enqueue_commit,
        collect_catch_up_retry_pending_channels, consume_catch_up_retry_state_for_scan,
        insert_configured_catch_up_candidate, is_restart_gap_notice,
        merge_catch_up_retry_checkpoint, parse_catch_up_scan_pace, phase2_retry_after_checkpoint,
        prune_stale_checkpoint_files, rearm_catch_up_retry_after_defer,
        rearm_catch_up_retry_after_fetch_failure, recent_page_decision, run_catch_up_sweep,
        should_pace_before_scan, take_catch_up_retry_checkpoint_after_queue_drain,
    };
    use crate::services::turn_orchestrator::{
        EnqueueRefusalReason, Intervention, InterventionMode, MAX_INTERVENTIONS_PER_CHANNEL,
    };
    use poise::serenity_prelude as serenity;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn too_old_drop(author_id: u64, text: &str) -> CatchUpTooOldDrop {
        CatchUpTooOldDrop {
            author_id,
            snippet: catch_up_too_old_snippet(text),
        }
    }

    #[test]
    fn catch_up_too_old_snippet_truncates_and_guards_empty() {
        assert_eq!(catch_up_too_old_snippet("  hi  "), "hi");
        assert_eq!(catch_up_too_old_snippet("   "), "(빈 메시지)");
        let long: String = "가".repeat(200);
        let snippet = catch_up_too_old_snippet(&long);
        assert!(
            snippet.chars().count() <= 81,
            "80 chars + one ellipsis, counted by chars not bytes"
        );
        assert!(snippet.ends_with('…'));
    }

    #[test]
    fn catch_up_too_old_notice_is_none_when_empty() {
        assert!(catch_up_too_old_notice(&[]).is_none());
    }

    #[test]
    fn catch_up_too_old_notice_aggregates_and_caps_list() {
        let drops: Vec<CatchUpTooOldDrop> = (0..15)
            .map(|i| too_old_drop(1000 + i, &format!("msg {i}")))
            .collect();
        let notice = catch_up_too_old_notice(&drops).expect("notice for non-empty drops");
        // Aggregated count reflects ALL drops, not just the listed ones.
        assert!(notice.contains("15건"));
        assert!(notice.contains("다시 보내주세요"));
        // Long lists are capped with a summarized remainder (15 - 10 = 5).
        assert!(notice.contains("… 외 5건"));
        // Never lists more than the cap of detail lines.
        assert_eq!(
            notice.matches("• `").count(),
            super::CATCH_UP_TOO_OLD_NOTICE_MAX_ITEMS
        );
    }

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
        std::fs::write(
            dir.join(format!("{}.txt", channel_id.get())),
            checkpoint.to_string(),
        )
        .expect("write last_message checkpoint");
    }

    struct TestCatchUpDiscordApi {
        current_user_id: Option<u64>,
        binding_status: RuntimeChannelBindingStatus,
        fetch_error: Option<&'static str>,
        messages: Vec<serenity::Message>,
        fetch_attempts: Option<Arc<AtomicUsize>>,
        empty_after_first_fetch: bool,
        cleanup_hook:
            Option<Arc<dyn Fn(&Arc<super::super::SharedData>, ChannelId, MessageId) + Send + Sync>>,
    }

    impl TestCatchUpDiscordApi {
        fn transient_fetch_failure() -> Self {
            Self {
                current_user_id: Some(9001),
                binding_status: RuntimeChannelBindingStatus::Owned,
                fetch_error: Some("temporary test fetch failure"),
                messages: Vec::new(),
                fetch_attempts: None,
                empty_after_first_fetch: false,
                cleanup_hook: None,
            }
        }

        fn unknown_binding() -> Self {
            Self {
                current_user_id: Some(9001),
                binding_status: RuntimeChannelBindingStatus::Unknown,
                fetch_error: None,
                messages: Vec::new(),
                fetch_attempts: None,
                empty_after_first_fetch: false,
                cleanup_hook: None,
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
            let fetch_attempt = self
                .fetch_attempts
                .as_ref()
                .map(|attempts| attempts.fetch_add(1, Ordering::SeqCst))
                .unwrap_or_default();
            if self.empty_after_first_fetch && fetch_attempt > 0 {
                return Ok(Vec::new());
            }
            match self.fetch_error {
                Some(error) => Err(error.to_string()),
                None => Ok(self.messages.clone()),
            }
        }

        async fn cleanup_recovered_catch_up_hourglass(
            &self,
            shared: &Arc<super::super::SharedData>,
            channel_id: ChannelId,
            message_id: MessageId,
        ) {
            if let Some(hook) = &self.cleanup_hook {
                hook(shared, channel_id, message_id);
            }
        }
    }

    fn recent_message_id(sequence: u64) -> MessageId {
        recent_message_id_with_age(sequence, Duration::from_secs(30))
    }

    fn recent_message_id_with_age(sequence: u64, age: Duration) -> MessageId {
        const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
        let age_ms = i64::try_from(age.as_millis()).expect("test age fits in i64 millis");
        let timestamp_ms = chrono::Utc::now().timestamp_millis() - age_ms;
        let discord_ms = u64::try_from(timestamp_ms - DISCORD_EPOCH_MS)
            .expect("test timestamp must be after Discord epoch");
        MessageId::new((discord_ms << 22) | sequence)
    }

    fn catch_up_test_message(
        channel_id: ChannelId,
        message_id: MessageId,
        author_id: u64,
        text: &str,
    ) -> serenity::Message {
        let mut author = serenity::User::default();
        author.id = serenity::UserId::new(author_id);
        author.name = format!("user-{author_id}");

        let mut message = serenity::Message::default();
        message.id = message_id;
        message.channel_id = channel_id;
        message.author = author;
        message.content = text.to_string();
        message.timestamp = message_id.created_at();
        message
    }

    fn set_dir_readonly(path: &std::path::Path, readonly: bool) {
        let mut permissions = std::fs::metadata(path)
            .unwrap_or_else(|error| panic!("read permissions for {}: {error}", path.display()))
            .permissions();
        permissions.set_readonly(readonly);
        std::fs::set_permissions(path, permissions)
            .unwrap_or_else(|error| panic!("set permissions for {}: {error}", path.display()));
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
    fn stale_checkpoint_prune_preserves_last_message_lock_sidecars() {
        let root = scoped_runtime_root();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(1479671298497183835);
        write_last_message_checkpoint(root.path(), &provider, channel_id, 1504812094456070174);

        let dir = root
            .path()
            .join("runtime")
            .join("last_message")
            .join(provider.as_str());
        let checkpoint_path = dir.join(format!("{}.txt", channel_id.get()));
        let lock_path = dir.join(format!("{}.txt.lock", channel_id.get()));
        std::fs::write(&lock_path, "lock").expect("write lock sidecar");

        let old_mtime = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() - Duration::from_secs(700),
        );
        filetime::set_file_mtime(&checkpoint_path, old_mtime).expect("age checkpoint");
        filetime::set_file_mtime(&lock_path, old_mtime).expect("age lock sidecar");

        let pruned = prune_stale_checkpoint_files(&dir, Duration::from_secs(600));

        assert_eq!(pruned, 1, "only the stale .txt checkpoint is pruned");
        assert!(
            !checkpoint_path.exists(),
            "stale checkpoint file should be pruned"
        );
        assert!(
            lock_path.exists(),
            ".txt.lock flock sidecar must not be pruned"
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
            classify_catch_up_message(
                &view,
                Some(current_bot_id),
                &existing,
                &HashSet::new(),
                300,
                &[],
                None,
                None,
            ),
            CatchUpClassification::Recover
        );
        assert_eq!(
            classify_catch_up_message(
                &view,
                Some(owner_user_id),
                &existing,
                &HashSet::new(),
                300,
                &[],
                None,
                None,
            ),
            CatchUpClassification::SelfAuthored
        );
    }

    #[test]
    fn restart_gap_notice_from_allowed_bot_is_never_recollected() {
        // #4443: the aggregate restart-gap notice is posted through an
        // allowed sender bot. Re-collecting it as TooOld quoted it inside the
        // next notice — one more nesting level per restart, spammed to every
        // channel. The guard must fire BEFORE the TooOld branch (no DLQ, no
        // drop entry) and regardless of age.
        let notice_bot_id = 1481522187197218816;
        let current_bot_id = 9001;
        let notice_text = catch_up_too_old_notice(&[CatchUpTooOldDrop {
            author_id: notice_bot_id,
            snippet: "⚠️ 재시작 공백으로 1건이 5분 초과로 미처리되었습니다…".to_string(),
        }])
        .expect("non-empty drops produce a notice");
        let mut view = CatchUpMessageView {
            message_id: 1504813049431724099,
            author_id: notice_bot_id,
            author_is_bot: true,
            is_processable_kind: true,
            age_secs: 3600, // far beyond max_age — would be TooOld without the guard
            trimmed_text: notice_text,
        };
        let existing = HashSet::new();

        assert_eq!(
            classify_catch_up_message(
                &view,
                Some(current_bot_id),
                &existing,
                &HashSet::new(),
                300,
                &[notice_bot_id],
                None,
                None,
            ),
            CatchUpClassification::SelfAuthored,
            "aged notice must be classified out before the TooOld drop path"
        );
        view.age_secs = 60;
        assert_eq!(
            classify_catch_up_message(
                &view,
                Some(current_bot_id),
                &existing,
                &HashSet::new(),
                300,
                &[notice_bot_id],
                None,
                None,
            ),
            CatchUpClassification::SelfAuthored,
            "young notice must not be recovered as a turn input either"
        );

        // Shared predicate (also wired into the phase2 enqueue loop):
        assert!(is_restart_gap_notice(true, &view.trimmed_text));
        assert!(
            !is_restart_gap_notice(false, &view.trimmed_text),
            "human-authored text is never a notice"
        );
        assert!(
            !is_restart_gap_notice(true, "PM triage: pick up #42"),
            "ordinary allowed-bot traffic must stay recoverable"
        );

        // A human message that merely mentions the marker mid-text still
        // recovers — the guard is prefix + bot-author scoped.
        let human = CatchUpMessageView {
            message_id: 1504813049431724100,
            author_id: 343742347365974026,
            author_is_bot: false,
            is_processable_kind: true,
            age_secs: 60,
            trimmed_text: "어제 '⚠️ 재시작 공백으로' 카드 왜 떴어?".to_string(),
        };
        assert_eq!(
            classify_catch_up_message(
                &human,
                Some(current_bot_id),
                &existing,
                &HashSet::new(),
                300,
                &[],
                None,
                None,
            ),
            CatchUpClassification::Recover
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
            classify_catch_up_message(
                &view,
                Some(current_bot_id),
                &existing,
                &HashSet::new(),
                300,
                &[],
                None,
                None,
            ),
            CatchUpClassification::NotAllowed
        );
        // With the announce_bot_id hint it recovers.
        assert_eq!(
            classify_catch_up_message(
                &view,
                Some(current_bot_id),
                &existing,
                &HashSet::new(),
                300,
                &[],
                Some(announce_id),
                None,
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
        assert_eq!(
            recent_page_decision(1, Some(120), 300),
            RecentPageDecision::FetchOlder,
            "in-window oldest under budget should page backward",
        );
    }

    #[test]
    fn recent_pagination_stops_when_oldest_exceeds_age_window() {
        // The oldest message in the page is already older than the window —
        // nothing older can qualify, so stop without another fetch.
        assert_eq!(
            recent_page_decision(1, Some(301), 300),
            RecentPageDecision::Complete,
            "out-of-window oldest must prove the gap complete",
        );
    }

    #[test]
    fn recent_pagination_budget_is_incomplete_not_checkpoint_safe() {
        // The REST budget still caps backward reach, but it cannot claim that
        // an unbounded gap is complete while the oldest row remains in-window.
        assert_eq!(
            recent_page_decision(CATCH_UP_RECENT_MAX_PAGES, Some(10), 300),
            RecentPageDecision::IncompleteBudget,
            "page budget exhaustion must preserve the unknown older gap",
        );
        // One below the budget still pages.
        assert_eq!(
            recent_page_decision(CATCH_UP_RECENT_MAX_PAGES - 1, Some(10), 300),
            RecentPageDecision::FetchOlder,
            "below-budget in-window page should continue",
        );
    }

    #[test]
    fn recent_pagination_stops_on_empty_page() {
        // No oldest message (empty page) → no cursor to page before → stop.
        assert_eq!(
            recent_page_decision(1, None, 300),
            RecentPageDecision::Complete,
            "empty page proves there is no older cursor",
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
                deferred_rearms: 0,
                armed_at: older_armed_at,
            },
        );

        let retry_checkpoints = HashMap::from([(
            channel_id,
            CatchUpRetryState {
                checkpoint: 1504812094456070300,
                fetch_failures: 1,
                deferred_rearms: 0,
                armed_at: newer_armed_at,
            },
        )]);

        let pending_retry_channels = collect_catch_up_retry_pending_channels(&shared);
        assert_eq!(pending_retry_channels.len(), 1);
        assert!(shared.catch_up_retry_pending.contains_key(&channel_id));

        let merged = match consume_catch_up_retry_state_for_scan(
            &shared,
            channel_id,
            &retry_checkpoints,
            &pending_retry_channels,
        ) {
            CatchUpRetryScanDecision::Proceed(Some(retry_state)) => retry_state,
            other => panic!("expected pending retry to be consumed for scan, got {other:?}"),
        };
        assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
        assert_eq!(merged.checkpoint, 1504812094456070200);
        assert_eq!(merged.fetch_failures, 2);
        assert_eq!(merged.armed_at, older_armed_at);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn periodic_sweep_skips_retry_scan_when_pending_retry_already_consumed() {
        let root = scoped_runtime_root();
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1479671298497183835);
        let checkpoint = 1504812094456070200;
        write_last_message_checkpoint(root.path(), &provider, channel_id, checkpoint);
        shared.catch_up_retry_pending.insert(
            channel_id,
            CatchUpRetryState {
                checkpoint,
                fetch_failures: 1,
                deferred_rearms: 0,
                armed_at: Instant::now(),
            },
        );

        let pending_retry_channels = collect_catch_up_retry_pending_channels(&shared);
        let consumed = take_catch_up_retry_checkpoint_after_queue_drain(&shared, channel_id, 0)
            .expect("concurrent drain should consume the pending retry first");
        assert_eq!(consumed.checkpoint, checkpoint);
        assert_eq!(
            consume_catch_up_retry_state_for_scan(
                &shared,
                channel_id,
                &HashMap::from([(channel_id, consumed)]),
                &pending_retry_channels,
            ),
            CatchUpRetryScanDecision::Proceed(Some(consumed)),
            "a caller-supplied retry_checkpoints entry is owned by this scan and \
             survives losing the pending-map race"
        );

        let fetch_attempts = Arc::new(AtomicUsize::new(0));
        let api = TestCatchUpDiscordApi {
            current_user_id: Some(9001),
            binding_status: RuntimeChannelBindingStatus::Owned,
            fetch_error: None,
            messages: Vec::new(),
            fetch_attempts: Some(Arc::clone(&fetch_attempts)),
            empty_after_first_fetch: false,
            cleanup_hook: None,
        };
        run_catch_up_sweep(
            CatchUpDeps::new(&api, &shared, &provider)
                .with_pending_retry_channels(&pending_retry_channels),
        )
        .await;

        // Phase 2's fixed 20-message backstop scan legitimately fetches once for
        // every candidate regardless of retry mode; the phase-1 retry-mode pinned
        // fetch would make this 2. Exactly one attempt proves the retry scan was
        // skipped after the point-of-use remove lost the race.
        assert_eq!(
            fetch_attempts.load(Ordering::SeqCst),
            1,
            "lost point-of-use remove must skip the phase-1 retry scan (phase-2 backstop fetch only)"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn phase1_deferred_commit_arms_retry_and_stops_before_later_message() {
        let root = scoped_runtime_root();
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1479671298497183835);
        let author_id = 343742347365974026;
        let first_id = recent_message_id(1);
        let deferred_id = recent_message_id(2);
        let later_id = recent_message_id(3);
        let checkpoint = first_id.get() - 1;
        write_last_message_checkpoint(root.path(), &provider, channel_id, checkpoint);
        let pending_queue_dir = root
            .path()
            .join("runtime")
            .join("discord_pending_queue")
            .join(provider.as_str())
            .join(&shared.token_hash);
        // The queue dir is created lazily on first persist; pre-create it so the
        // readonly flip in the cleanup hook can target it before any file lands.
        std::fs::create_dir_all(&pending_queue_dir).expect("pre-create pending queue dir");
        let readonly_dir = pending_queue_dir.clone();
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let cleanup_calls_for_hook = Arc::clone(&cleanup_calls);

        let api = TestCatchUpDiscordApi {
            current_user_id: Some(9001),
            binding_status: RuntimeChannelBindingStatus::Owned,
            fetch_error: None,
            messages: vec![
                catch_up_test_message(channel_id, first_id, author_id, "repeat turn"),
                catch_up_test_message(channel_id, deferred_id, author_id, "deferred turn"),
                catch_up_test_message(channel_id, later_id, author_id, "later turn"),
            ],
            fetch_attempts: None,
            empty_after_first_fetch: false,
            cleanup_hook: Some(Arc::new(move |_shared, _channel_id, _message_id| {
                if cleanup_calls_for_hook.fetch_add(1, Ordering::SeqCst) == 0 {
                    set_dir_readonly(&readonly_dir, true);
                }
            })),
        };
        run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;
        set_dir_readonly(&pending_queue_dir, false);
        assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);

        let saved_checkpoint = *shared
            .last_message_ids
            .get(&channel_id)
            .expect("phase1 should checkpoint the first committed message");
        assert_eq!(saved_checkpoint, first_id.get());
        assert!(
            saved_checkpoint < deferred_id.get(),
            "phase1 checkpoint must stay below the deferred message"
        );

        let pending = shared
            .catch_up_retry_pending
            .get(&channel_id)
            .expect("phase1 deferred commit should arm catch-up retry");
        assert_eq!(pending.checkpoint, first_id.get());
        assert!(pending.checkpoint < deferred_id.get());
        assert_eq!(pending.fetch_failures, 0);

        let snapshot = super::super::mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(
            snapshot.intervention_queue.len(),
            1,
            "phase1 must stop before committing the later message"
        );
        assert_eq!(snapshot.intervention_queue[0].message_id, first_id);
        assert_eq!(
            snapshot.intervention_queue[0].source_message_ids,
            vec![first_id]
        );
        assert!(
            !snapshot.intervention_queue[0]
                .source_message_ids
                .contains(&later_id),
            "later message must not be merged or committed in the deferred pass"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn phase1_recent_queue_cap_arms_retry_without_checkpoint() {
        let root = scoped_runtime_root();
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1479671298497183835);
        let author_id = 343742347365974026;
        let capped_id = recent_message_id(1);
        let config_dir = root.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::write(
            config_dir.join("role_map.json"),
            format!(
                r#"{{
  "byChannelId": {{
    "{}": {{
      "roleId": "adk-cc",
      "promptFile": "prompt.md",
      "provider": "claude"
    }}
  }}
}}"#,
                channel_id.get()
            ),
        )
        .expect("write role map");

        for index in 0..MAX_INTERVENTIONS_PER_CHANNEL {
            let message_id = recent_message_id(100 + index as u64);
            let enqueue = super::super::mailbox_enqueue_intervention(
                &shared,
                &provider,
                channel_id,
                Intervention {
                    author_id: serenity::UserId::new(author_id),
                    author_is_bot: false,
                    message_id,
                    queued_generation: crate::services::discord::runtime_store::load_generation(),
                    source_message_ids: vec![message_id],
                    source_message_queued_generations: Vec::new(),
                    source_text_segments: Vec::new(),
                    text: format!("queued turn {index}"),
                    mode: InterventionMode::Soft,
                    created_at: Instant::now(),
                    reply_context: None,
                    has_reply_boundary: false,
                    merge_consecutive: false,
                    pending_uploads: Vec::new(),
                    voice_announcement: None,
                },
            )
            .await;
            assert!(catch_up_enqueue_accepted(&enqueue));
        }

        let snapshot = super::super::mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(
            snapshot.intervention_queue.len(),
            MAX_INTERVENTIONS_PER_CHANNEL
        );
        assert!(shared.last_message_ids.get(&channel_id).is_none());

        let fetch_attempts = Arc::new(AtomicUsize::new(0));
        let api = TestCatchUpDiscordApi {
            current_user_id: Some(9001),
            binding_status: RuntimeChannelBindingStatus::Owned,
            fetch_error: None,
            messages: vec![catch_up_test_message(
                channel_id,
                capped_id,
                author_id,
                "queued after clear",
            )],
            fetch_attempts: Some(Arc::clone(&fetch_attempts)),
            empty_after_first_fetch: true,
            cleanup_hook: None,
        };
        run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

        assert!(shared.last_message_ids.get(&channel_id).is_none());
        let pending = shared
            .catch_up_retry_pending
            .get(&channel_id)
            .expect("Recent queue cap should arm catch-up retry");
        assert_eq!(pending.checkpoint, capped_id.get().saturating_sub(1));
        assert_eq!(pending.fetch_failures, 0);
    }

    #[test]
    fn catch_up_intervention_created_at_preserves_message_gap() {
        let scan_wall_time = chrono::Utc::now();
        let scan_instant = Instant::now();
        let first_id = recent_message_id_with_age(1, Duration::from_secs(240));
        let resent_id = recent_message_id_with_age(2, Duration::from_secs(60));

        let first_created_at =
            catch_up_intervention_created_at(scan_wall_time, scan_instant, first_id);
        let resent_created_at =
            catch_up_intervention_created_at(scan_wall_time, scan_instant, resent_id);
        let gap = resent_created_at.duration_since(first_created_at);

        assert!(
            (Duration::from_secs(179)..=Duration::from_secs(181)).contains(&gap),
            "catch-up should preserve the real inter-message gap, got {gap:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn phase1_resend_after_real_dedup_window_survives_catch_up() {
        let root = scoped_runtime_root();
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1479671298497183835);
        let author_id = 343742347365974026;
        let first_id = recent_message_id_with_age(1, Duration::from_secs(240));
        let resent_id = recent_message_id_with_age(2, Duration::from_secs(60));
        let checkpoint = first_id.get() - 1;
        write_last_message_checkpoint(root.path(), &provider, channel_id, checkpoint);

        let api = TestCatchUpDiscordApi {
            current_user_id: Some(9001),
            binding_status: RuntimeChannelBindingStatus::Owned,
            fetch_error: None,
            messages: vec![
                catch_up_test_message(channel_id, first_id, author_id, "진행해줘"),
                catch_up_test_message(channel_id, resent_id, author_id, "진행해줘"),
            ],
            fetch_attempts: None,
            empty_after_first_fetch: false,
            cleanup_hook: None,
        };
        run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

        let saved_checkpoint = *shared
            .last_message_ids
            .get(&channel_id)
            .expect("phase1 should checkpoint through the committed resend");
        assert_eq!(saved_checkpoint, resent_id.get());
        assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));

        let snapshot = super::super::mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(snapshot.intervention_queue.len(), 1);
        let queued = &snapshot.intervention_queue[0];
        assert_eq!(queued.message_id, resent_id);
        assert_eq!(queued.source_message_ids, vec![first_id, resent_id]);
        assert_eq!(queued.text, "진행해줘\n진행해줘");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn phase1_true_rapid_resend_dedups_and_advances_checkpoint() {
        let root = scoped_runtime_root();
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1479671298497183835);
        let author_id = 343742347365974026;
        let first_id = recent_message_id_with_age(1, Duration::from_secs(35));
        let duplicate_id = recent_message_id_with_age(2, Duration::from_secs(30));
        let checkpoint = first_id.get() - 1;
        write_last_message_checkpoint(root.path(), &provider, channel_id, checkpoint);

        let api = TestCatchUpDiscordApi {
            current_user_id: Some(9001),
            binding_status: RuntimeChannelBindingStatus::Owned,
            fetch_error: None,
            messages: vec![
                catch_up_test_message(channel_id, first_id, author_id, "repeat turn"),
                catch_up_test_message(channel_id, duplicate_id, author_id, "repeat turn"),
            ],
            fetch_attempts: None,
            empty_after_first_fetch: false,
            cleanup_hook: None,
        };
        run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

        let saved_checkpoint = *shared
            .last_message_ids
            .get(&channel_id)
            .expect("phase1 should checkpoint through a genuine rapid duplicate");
        assert_eq!(saved_checkpoint, duplicate_id.get());
        assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));

        let snapshot = super::super::mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(snapshot.intervention_queue.len(), 1);
        assert_eq!(snapshot.intervention_queue[0].message_id, first_id);
        assert!(catch_up_last_item_dedup_is_checkpoint_safe(
            snapshot.intervention_queue.last(),
            duplicate_id,
        ));
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
            deferred_rearms: 0,
            armed_at,
        };

        let normal_reference =
            catch_up_message_age_reference_time(scan_wall_time, scan_instant, None);
        assert_eq!(
            normal_reference
                .signed_duration_since(message_time)
                .num_seconds(),
            400
        );

        let retry_reference =
            catch_up_message_age_reference_time(scan_wall_time, scan_instant, Some(retry_state));
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
            deferred_rearms: 0,
            armed_at,
        };
        let retry_checkpoints = HashMap::from([(channel_id, retry_state)]);

        run_catch_up_sweep(
            CatchUpDeps::new(
                &TestCatchUpDiscordApi::transient_fetch_failure(),
                &shared,
                &provider,
            )
            .with_retry_checkpoints(&retry_checkpoints),
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
            deferred_rearms: 0,
            armed_at,
        };
        let retry_checkpoints = HashMap::from([(channel_id, retry_state)]);

        run_catch_up_sweep(
            CatchUpDeps::new(
                &TestCatchUpDiscordApi::unknown_binding(),
                &shared,
                &provider,
            )
            .with_retry_checkpoints(&retry_checkpoints),
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
        let first_retry = take_catch_up_retry_checkpoint_after_queue_drain(&shared, channel_id, 0)
            .expect("pending retry should be consumed by the first drain");
        assert_eq!(first_retry.checkpoint, checkpoint);
        assert_eq!(first_retry.fetch_failures, 0);
        assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));

        let rearmed = rearm_catch_up_retry_after_fetch_failure(&shared, channel_id, first_retry)
            .expect("transient retry fetch failure should re-arm the same backlog cursor");
        assert_eq!(rearmed.checkpoint, checkpoint);
        assert_eq!(rearmed.fetch_failures, 1);
        assert_eq!(rearmed.armed_at, first_retry.armed_at);

        let second_retry = take_catch_up_retry_checkpoint_after_queue_drain(&shared, channel_id, 0)
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
            deferred_rearms: 0,
            armed_at: Instant::now(),
        };

        assert!(
            rearm_catch_up_retry_after_fetch_failure(&shared, channel_id, exhausted_retry)
                .is_none(),
            "retry fetch failures must not re-arm forever"
        );
        assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
    }

    // #4156: a channel whose fetch keeps succeeding but whose messages keep
    // being Deferred must not re-arm the catch-up retry forever.
    #[test]
    fn deferred_rearm_gives_up_after_bounded_attempts_and_preserves_arm_time() {
        let shared = super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(1479671298497183835);
        let checkpoint = 1504812094456070130;
        let armed_at = Instant::now();

        // Already at the Deferred cap ⇒ no further re-arm, nothing left pending.
        let exhausted = CatchUpRetryState {
            checkpoint,
            fetch_failures: 0,
            deferred_rearms: CATCH_UP_RETRY_DEFERRED_REARM_LIMIT,
            armed_at,
        };
        assert!(
            rearm_catch_up_retry_after_defer(&shared, channel_id, checkpoint, Some(exhausted))
                .is_none(),
            "deferred re-arms must not spin forever"
        );
        assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));

        // One below the cap ⇒ re-arms, carrying the incremented Deferred budget
        // and the ORIGINAL arm time forward (the age window must not reset).
        let almost = CatchUpRetryState {
            checkpoint,
            fetch_failures: 0,
            deferred_rearms: CATCH_UP_RETRY_DEFERRED_REARM_LIMIT - 1,
            armed_at,
        };
        assert_eq!(
            rearm_catch_up_retry_after_defer(&shared, channel_id, checkpoint, Some(almost)),
            Some(checkpoint),
        );
        let pending = *shared
            .catch_up_retry_pending
            .get(&channel_id)
            .expect("re-arm below cap should stay pending");
        assert_eq!(pending.deferred_rearms, CATCH_UP_RETRY_DEFERRED_REARM_LIMIT);
        assert_eq!(pending.armed_at, armed_at);
    }

    // #4156 (opus review): phase-1 CONSUMES the pending entry before phase-2
    // runs, so phase-2 must recover the Deferred budget from the state phase-1
    // consumed this cycle (`threaded_prior`) even when the pending map is empty
    // — otherwise a phase-2-origin defer would reset the budget to 1 forever and
    // the cap would never bind. This asserts the budget still climbs to the cap
    // through the threaded path with NO surviving map entry.
    #[test]
    fn deferred_rearm_accumulates_via_threaded_prior_when_map_entry_absent() {
        let shared = super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_156_222);
        let checkpoint = 1504812094456070130;
        let armed_at = Instant::now();

        // Simulate phase-2 re-arming with the budget phase-1 consumed this cycle
        // while the pending map is empty (phase-1 removed it and did not re-arm).
        let mut budget = 2u8;
        loop {
            let consumed = CatchUpRetryState {
                checkpoint,
                fetch_failures: 0,
                deferred_rearms: budget,
                armed_at,
            };
            // The pending map is empty each iteration (phase-1 consumed it).
            shared.catch_up_retry_pending.remove(&channel_id);
            let result =
                rearm_catch_up_retry_after_defer(&shared, channel_id, checkpoint, Some(consumed));
            if budget >= CATCH_UP_RETRY_DEFERRED_REARM_LIMIT {
                assert!(
                    result.is_none(),
                    "at/over the cap the threaded path must give up, not reset to 1"
                );
                assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
                break;
            }
            assert_eq!(result, Some(checkpoint));
            let pending = *shared
                .catch_up_retry_pending
                .get(&channel_id)
                .expect("below cap should re-arm");
            // Budget climbs by exactly one — never resets to 1.
            assert_eq!(pending.deferred_rearms, budget + 1);
            budget += 1;
        }
        assert_eq!(budget, CATCH_UP_RETRY_DEFERRED_REARM_LIMIT);
    }

    #[test]
    fn phase2_checkpoint_advances_only_to_known_recovered_or_duplicate_ids() {
        assert_eq!(advance_phase2_checkpoint(None, 100), Some(100));
        assert_eq!(advance_phase2_checkpoint(Some(100), 99), Some(100));
        assert_eq!(advance_phase2_checkpoint(Some(100), 101), Some(101));
    }

    #[test]
    fn phase2_enqueue_commit_classifies_duplicate_refusals_without_recovery() {
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

        let last_item_dedup = super::super::MailboxEnqueueOutcome {
            enqueued: false,
            merged: false,
            refusal_reason: Some(EnqueueRefusalReason::LastItemDedup),
            persistence_error: None,
        };
        assert_eq!(
            classify_phase2_enqueue_commit(&last_item_dedup),
            Phase2EnqueueCommit::LastItemDedup
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
