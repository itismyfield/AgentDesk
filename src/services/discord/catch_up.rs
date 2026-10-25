//! Startup and restart-gap recovery from Discord history.
//! Sweep phases preserve retry origins and unresolved message frontiers.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::SourceMessageQueuedGeneration;

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

fn catch_up_intervention_text(text: &str, author_is_bot: bool) -> String {
    if author_is_bot {
        return text.to_string();
    }
    let (without_operational_alert_origin, _) =
        super::dispatch_policy::strip_operational_alert_origin(text);
    without_operational_alert_origin.trim().to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services) struct CatchUpRetryState {
    checkpoint: u64,
    fetch_failures: u8,
    // #4156: consecutive Deferred re-arms (fetch succeeded, enqueue deferred).
    // Tracked separately from `fetch_failures` because a Deferred re-arm is not
    // a fetch error; both budgets share the merge's most-exhausted semantics.
    // A value beyond the retry limit retains the exhausted arm for periodic sweeps.
    deferred_rearms: u8,
    armed_at: Instant,
}

mod api;
mod classification;
mod clock;
pub(in crate::services::discord) mod consumed_commands;
mod deps;
mod frontier_evidence;
pub(in crate::services::discord) mod handled_command;
mod held_range;
mod phase1_sweep;
mod phase2;
mod phase2_sweep;
pub(in crate::services::discord) mod retry_state;
mod settled_frontier;
mod settled_ledger_consult;

#[cfg(test)]
#[path = "catch_up/classification_order_tests.rs"]
mod classification_order_tests;

use api::{CatchUpDiscordApi, CatchUpFetchRequest, SerenityCatchUpDiscordApi};
use classification::{
    CatchUpClassification, CatchUpClassificationDecision, CatchUpMessageView, CatchUpScanStats,
    classify_catch_up_message_with_utility_resolution, too_old_is_actionable,
};
#[cfg(test)]
use classification::{classify_catch_up_message, is_restart_gap_notice};
#[cfg(test)]
use clock::SystemClock;
use deps::CatchUpDeps;
use handled_command::{TextCommandEvidence, defer_unrecognized_command};
use phase2::{
    Phase2EnqueueCommit, Phase2Frontier, Phase2PreGate, Phase2RecoveryStats,
    advance_phase2_checkpoint, catch_up_last_item_dedup_is_checkpoint_safe,
    catch_up_remaining_queue_capacity, classify_phase2_enqueue_commit,
    log_catch_up_enqueue_not_accepted, log_catch_up_phase2_age_out, phase2_pre_gate,
};
#[cfg(test)]
use phase2::{catch_up_enqueue_accepted, phase2_retry_after_checkpoint};
#[cfg(test)]
use retry_state::arm_catch_up_retry_state;
use retry_state::{
    CatchUpRetryScanDecision, arm_catch_up_retry_preserving_prior,
    catch_up_message_age_reference_time, catch_up_retry_prior,
    collect_catch_up_retry_pending_channels, consume_catch_up_retry_state_for_scan,
    finish_hold_scan, hold_utility_resolution, rearm_catch_up_retry_after_defer,
    rearm_catch_up_retry_after_fetch_failure, record_exhausted_hold_expiry, try_channel_scan,
};
use settled_frontier::{RetainedBarrier, SettledFrontier};

pub(in crate::services::discord) fn should_trigger_catch_up_retry(queue_len: usize) -> bool {
    queue_len <= CATCH_UP_RETRY_QUEUE_THRESHOLD
}

pub(in crate::services::discord) use retry_state::take_catch_up_retry_checkpoint_after_queue_drain;

#[cfg(test)]
fn arm_catch_up_retry_pending(shared: &SharedData, channel_id: ChannelId, retry_after: u64) -> u64 {
    arm_catch_up_retry_state(shared, channel_id, CatchUpRetryState::new(retry_after)).checkpoint
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

// Historical restart-gap notices must never be recollected as user input.
const CATCH_UP_TOO_OLD_NOTICE_PREFIX: &str = "⚠️ 재시작 공백으로";

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

/// Startup catch-up polling: fetch messages that arrived during the catch-up
/// gap. Uses saved last_message_ids to query Discord REST API, classifies
/// sender eligibility/age/duplicates, and enqueues only safe recoveries.
pub(in crate::services::discord) async fn catch_up_missed_messages(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let api = SerenityCatchUpDiscordApi { http };
    catch_up_missed_messages_with_api(&api, shared, provider).await;
}

async fn catch_up_missed_messages_with_api<A: CatchUpDiscordApi + ?Sized>(
    api: &A,
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
    run_catch_up_sweep(
        CatchUpDeps::new(api, shared, provider)
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

struct SweepContext<'a, A: CatchUpDiscordApi + ?Sized> {
    deps: CatchUpDeps<'a, A>,
    scan_wall_time: chrono::DateTime<chrono::Utc>,
    scan_instant: Instant,
    max_age: std::time::Duration,
    current_bot_user_id: Option<u64>,
}

struct SweepState {
    consumed_retry_states_this_cycle: HashMap<ChannelId, CatchUpRetryState>,
    incomplete_recent_channels: HashSet<ChannelId>,
    ttl_retired_channels: HashSet<ChannelId>,
    open_barriers: HashMap<ChannelId, RetainedBarrier>,
}

async fn run_catch_up_sweep<A: CatchUpDiscordApi + ?Sized>(deps: CatchUpDeps<'_, A>) {
    let CatchUpDeps {
        api,
        shared,
        provider,
        retry_checkpoints,
        pending_retry_channels,
        clock,
    } = deps;
    let Some(root) = runtime_store::last_message_root() else {
        return;
    };
    let dir = root.join(provider.as_str());

    let (scan_wall_time, scan_instant) = clock.sample();
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
        tracing::info!("  [{ts}] 🧹 catch-up: pruned {pruned} stale checkpoint(s) (>10min old)");
    }

    let mut candidates = collect_catch_up_channel_candidates(&dir, provider);
    for channel_id in retry_checkpoints
        .keys()
        .copied()
        .chain(pending_retry_channels.iter().copied())
        .chain(
            shared
                .catch_up_retry_pending
                .iter()
                .filter(|entry| entry.is_exhausted_hold())
                .map(|entry| *entry.key()),
        )
        .chain(
            shared
                .catch_up_scan_in_progress
                .iter()
                .filter(|slot| slot.1.is_some())
                .map(|slot| *slot.key()),
        )
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
    let consumed_retry_states_this_cycle: HashMap<ChannelId, CatchUpRetryState> = HashMap::new();
    // Phase 2 must not bypass an incomplete unbounded Recent scan and persist a
    // newer recovery across the same unknown lower gap.
    let incomplete_recent_channels: HashSet<ChannelId> = HashSet::new();
    let ttl_retired_channels = HashSet::new();
    // Open barriers bound phase 2's durable write and every retry cursor.
    let open_barriers: HashMap<ChannelId, RetainedBarrier> = HashMap::new();

    let context = SweepContext {
        deps: CatchUpDeps {
            api,
            shared,
            provider,
            retry_checkpoints,
            pending_retry_channels,
            clock,
        },
        scan_wall_time,
        scan_instant,
        max_age,
        current_bot_user_id,
    };
    let mut state = SweepState {
        consumed_retry_states_this_cycle,
        incomplete_recent_channels,
        ttl_retired_channels,
        open_barriers,
    };
    phase1_sweep::run(&context, &candidates, &mut state).await;
    phase2_sweep::run(&context, &candidates, &mut state).await;
    let SweepState {
        open_barriers,
        consumed_retry_states_this_cycle,
        ..
    } = state;

    // An open barrier must be reread next sweep; waiting on membership spends
    // no budget, and an exhausted budget is not reset by a fresh arm.
    for (channel_id, retained) in open_barriers {
        if retained.exhausted {
            continue;
        }
        arm_catch_up_retry_preserving_prior(
            shared,
            channel_id,
            retained.retry_after,
            consumed_retry_states_this_cycle.get(&channel_id).copied(),
            clock,
        );
    }
}

#[cfg(test)]
mod catch_up_recovery_tests {
    use super::retry_state::merge_catch_up_retry_checkpoint;
    use super::{
        CATCH_UP_RECENT_MAX_PAGES, CATCH_UP_RETRY_DEFERRED_REARM_LIMIT,
        CATCH_UP_RETRY_FETCH_FAILURE_LIMIT, CATCH_UP_SCAN_PACE_DEFAULT_MS, CatchUpChannelCandidate,
        CatchUpClassification, CatchUpDeps, CatchUpDiscordApi, CatchUpFetchMode,
        CatchUpMessageView, CatchUpRetryScanDecision, CatchUpRetryState, ChannelId, MessageId,
        Phase2EnqueueCommit, ProviderKind, RecentPageDecision, RuntimeChannelBindingStatus,
        SystemClock, advance_phase2_checkpoint, arm_catch_up_retry_pending,
        catch_up_enqueue_accepted, catch_up_fetch_mode_for_scan, catch_up_intervention_created_at,
        catch_up_last_item_dedup_is_checkpoint_safe, catch_up_message_age_reference_time,
        catch_up_remaining_queue_capacity, classify_catch_up_message,
        classify_phase2_enqueue_commit, collect_catch_up_retry_pending_channels,
        consume_catch_up_retry_state_for_scan, insert_configured_catch_up_candidate,
        is_restart_gap_notice, parse_catch_up_scan_pace, phase2_retry_after_checkpoint,
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

    /// Satisfies the owner requirement without being any author, so fixtures
    /// stay authorized through `allow_all_users`.
    const OTHER_OWNER_ID: u64 = 343_742_347_365_974_030;

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
            _request: super::CatchUpFetchRequest,
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
        // Historical notices from allowed bots remain terminal at every age.
        let notice_bot_id = 1481522187197218816;
        let current_bot_id = 9001;
        let notice_text = "⚠️ 재시작 공백으로 1건이 5분 초과로 미처리되었습니다. 필요하면 다시 보내주세요:\n• `42`: 예전 요청".to_string();
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
    async fn periodic_sweep_inherits_retry_reserved_by_queue_drain() {
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
        assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));

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

        // The periodic owner consumes the reservation and reads the pinned phase-1 cursor.
        assert_eq!(fetch_attempts.load(Ordering::SeqCst), 2);
        assert!(!shared.catch_up_scan_in_progress.contains_key(&channel_id));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn phase1_deferred_commit_arms_retry_and_stops_before_later_message() {
        let root = scoped_runtime_root();
        let shared = super::super::make_shared_data_for_tests();
        // #6042: phase 1 now gates on `author_is_authorized`, and
        // `user_is_authorized` is false for every id under default test
        // settings. Without this the fresh human message classifies
        // `NotAllowed` and the sweep never reaches the behaviour this test
        // exists to pin.
        {
            let mut settings = shared.settings.write().await;
            settings.owner_user_id = Some(OTHER_OWNER_ID);
            settings.allow_all_users = true;
        }
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
        // Replace the queue directory with a file after the first commit so later
        // persistence fails on every platform, preserving the committed contents.
        std::fs::create_dir_all(&pending_queue_dir).expect("pre-create pending queue dir");
        let blocked_dir = pending_queue_dir.clone();
        let saved_dir = pending_queue_dir.with_extension("saved");
        let saved_dir_for_hook = saved_dir.clone();
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
                    std::fs::rename(&blocked_dir, &saved_dir_for_hook)
                        .expect("preserve committed queue directory");
                    std::fs::write(&blocked_dir, b"not a directory")
                        .expect("block subsequent queue persistence");
                }
            })),
        };
        run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;
        std::fs::remove_file(&pending_queue_dir).expect("remove queue persistence blocker");
        std::fs::rename(&saved_dir, &pending_queue_dir).expect("restore committed queue directory");
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
        // #6042: phase 1 now gates on `author_is_authorized`, and
        // `user_is_authorized` is false for every id under default test
        // settings. Without this the fresh human message classifies
        // `NotAllowed` and the sweep never reaches the behaviour this test
        // exists to pin.
        {
            let mut settings = shared.settings.write().await;
            settings.owner_user_id = Some(OTHER_OWNER_ID);
            settings.allow_all_users = true;
        }
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
                    queued_generation: crate::services::discord::runtime_store::process_generation(
                    ),
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
        // #6042: phase 1 now gates on `author_is_authorized`, and
        // `user_is_authorized` is false for every id under default test
        // settings. Without this the fresh human message classifies
        // `NotAllowed` and the sweep never reaches the behaviour this test
        // exists to pin.
        {
            let mut settings = shared.settings.write().await;
            settings.owner_user_id = Some(OTHER_OWNER_ID);
            settings.allow_all_users = true;
        }
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
        // #6042: phase 1 now gates on `author_is_authorized`, and
        // `user_is_authorized` is false for every id under default test
        // settings. Without this the fresh human message classifies
        // `NotAllowed` and the sweep never reaches the behaviour this test
        // exists to pin.
        {
            let mut settings = shared.settings.write().await;
            settings.owner_user_id = Some(OTHER_OWNER_ID);
            settings.allow_all_users = true;
        }
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
        drop(super::retry_state::try_channel_scan(&shared, channel_id, Some(first_retry)).unwrap());

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

        // At the cap, the original arm remains held but cannot trigger a retry.
        let exhausted = CatchUpRetryState {
            checkpoint,
            fetch_failures: 0,
            deferred_rearms: CATCH_UP_RETRY_DEFERRED_REARM_LIMIT,
            armed_at,
        };
        assert!(
            rearm_catch_up_retry_after_defer(
                &shared,
                channel_id,
                checkpoint,
                Some(exhausted),
                &SystemClock
            )
            .is_none(),
            "deferred re-arms must not spin forever"
        );
        let hold = *shared
            .catch_up_retry_pending
            .get(&channel_id)
            .expect("exhausted hold");
        assert!(hold.is_exhausted_hold());
        assert_eq!((hold.checkpoint, hold.armed_at), (checkpoint, armed_at));
        assert!(collect_catch_up_retry_pending_channels(&shared).is_empty());
        assert!(take_catch_up_retry_checkpoint_after_queue_drain(&shared, channel_id, 0).is_none());
        shared.catch_up_retry_pending.remove(&channel_id);

        // One below the cap ⇒ re-arms, carrying the incremented Deferred budget
        // and the ORIGINAL arm time forward (the age window must not reset).
        let almost = CatchUpRetryState {
            checkpoint,
            fetch_failures: 0,
            deferred_rearms: CATCH_UP_RETRY_DEFERRED_REARM_LIMIT - 1,
            armed_at,
        };
        assert_eq!(
            rearm_catch_up_retry_after_defer(
                &shared,
                channel_id,
                checkpoint,
                Some(almost),
                &SystemClock
            ),
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
            let result = rearm_catch_up_retry_after_defer(
                &shared,
                channel_id,
                checkpoint,
                Some(consumed),
                &SystemClock,
            );
            if budget >= CATCH_UP_RETRY_DEFERRED_REARM_LIMIT {
                assert!(
                    result.is_none(),
                    "at/over the cap the threaded path must give up, not reset to 1"
                );
                let hold = *shared
                    .catch_up_retry_pending
                    .get(&channel_id)
                    .expect("exhausted hold");
                assert!(hold.is_exhausted_hold());
                assert_eq!((hold.checkpoint, hold.armed_at), (checkpoint, armed_at));
                assert!(collect_catch_up_retry_pending_channels(&shared).is_empty());
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
    fn exhausted_recovery_consumes_reserved_cursor_and_origin_into_actual_hold() {
        let shared = super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(6_813_293);
        let hold = CatchUpRetryState {
            checkpoint: 200,
            fetch_failures: 1,
            deferred_rearms: CATCH_UP_RETRY_DEFERRED_REARM_LIMIT + 1,
            armed_at: Instant::now(),
        };
        let reserved = CatchUpRetryState {
            checkpoint: 100,
            fetch_failures: CATCH_UP_RETRY_FETCH_FAILURE_LIMIT,
            deferred_rearms: CATCH_UP_RETRY_DEFERRED_REARM_LIMIT,
            armed_at: hold.armed_at - Duration::from_secs(60),
        };
        shared.catch_up_retry_pending.insert(channel_id, hold);
        shared
            .catch_up_scan_in_progress
            .insert(channel_id, (true, Some(reserved)));
        let expected = CatchUpRetryState {
            checkpoint: reserved.checkpoint,
            armed_at: reserved.armed_at,
            ..hold
        };
        assert_eq!(
            consume_catch_up_retry_state_for_scan(
                &shared,
                channel_id,
                &HashMap::new(),
                &HashSet::new()
            ),
            CatchUpRetryScanDecision::Proceed(Some(expected))
        );
        assert_eq!(
            shared
                .catch_up_retry_pending
                .get(&channel_id)
                .map(|entry| *entry),
            Some(expected)
        );
        assert_eq!(
            shared
                .catch_up_scan_in_progress
                .get(&channel_id)
                .map(|entry| *entry),
            Some((true, None))
        );
    }

    #[test]
    fn exhausted_recovery_release_requires_durable_progress_past_remaining_cursor() {
        let shared = super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(6_813_294);
        let hold = CatchUpRetryState {
            checkpoint: 100,
            fetch_failures: 0,
            deferred_rearms: CATCH_UP_RETRY_DEFERRED_REARM_LIMIT + 1,
            armed_at: Instant::now(),
        };
        shared.catch_up_retry_pending.insert(channel_id, hold);
        let finish = super::retry_state::finish_hold_scan;
        finish(&shared, channel_id, Some(hold), None, None);
        assert_eq!(
            shared
                .catch_up_retry_pending
                .get(&channel_id)
                .map(|entry| *entry),
            Some(hold)
        );
        let mut frontier = super::phase2::Phase2Frontier::new(None, 100);
        frontier.recovered(150);
        frontier.seal(200);
        assert_eq!(frontier.durable(), Some(150));
        let mut remaining = HashMap::new();
        frontier.retain_into(&mut remaining, channel_id, Some(150));
        let barrier = remaining[&channel_id];
        assert_eq!((barrier.barrier, barrier.retry_after), (200, 150));
        finish(
            &shared,
            channel_id,
            Some(hold),
            frontier.durable(),
            Some(barrier),
        );
        let advanced = CatchUpRetryState {
            checkpoint: 150,
            ..hold
        };
        assert_eq!(
            shared
                .catch_up_retry_pending
                .get(&channel_id)
                .map(|entry| *entry),
            Some(advanced)
        );
        assert!(advanced.hold_expired_at(hold.armed_at + Duration::from_secs(86_400)));
        finish(&shared, channel_id, Some(advanced), None, None);
        assert_eq!(
            shared
                .catch_up_retry_pending
                .get(&channel_id)
                .map(|entry| *entry),
            Some(advanced)
        );
        finish(&shared, channel_id, Some(advanced), Some(200), None);
        assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
    }

    #[test]
    fn exhausted_recovery_stale_busy_retry_cannot_resurrect_ttl_retired_message() {
        let shared = super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(6_813_299);
        let retry = CatchUpRetryState {
            checkpoint: 100,
            fetch_failures: 0,
            deferred_rearms: CATCH_UP_RETRY_DEFERRED_REARM_LIMIT,
            armed_at: Instant::now(),
        };
        let hold = CatchUpRetryState {
            deferred_rearms: retry.deferred_rearms + 1,
            ..retry
        };
        let mut owner = super::retry_state::try_channel_scan(&shared, channel_id, None).unwrap();
        shared.catch_up_retry_pending.insert(channel_id, hold);
        owner.settled(Some(200));
        super::retry_state::finish_hold_scan(&shared, channel_id, Some(hold), Some(200), None);
        assert!(super::retry_state::try_channel_scan(&shared, channel_id, Some(retry)).is_none());
        assert_eq!(
            shared
                .catch_up_scan_in_progress
                .get(&channel_id)
                .map(|e| *e),
            Some((true, Some(retry)))
        );
        drop(owner);
        assert!(!shared.catch_up_scan_in_progress.contains_key(&channel_id));
        assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
        shared.last_message_ids.insert(channel_id, 200);
        assert!(super::retry_state::try_channel_scan(&shared, channel_id, Some(retry)).is_none());
        assert_eq!(
            super::retry_state::catch_up_retry_prior(&shared, channel_id, Some(hold)),
            None
        );
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
            Phase2EnqueueCommit::DuplicateQueued
        );

        let already_active = super::super::MailboxEnqueueOutcome {
            enqueued: false,
            merged: false,
            refusal_reason: Some(EnqueueRefusalReason::AlreadyActiveTurn),
            persistence_error: None,
        };
        assert_eq!(
            classify_phase2_enqueue_commit(&already_active),
            Phase2EnqueueCommit::DuplicateActiveTurn
        );

        let absorbed = super::super::MailboxEnqueueOutcome {
            enqueued: false,
            merged: false,
            refusal_reason: Some(EnqueueRefusalReason::AbsorbedByActiveTurn),
            persistence_error: None,
        };
        assert_eq!(
            classify_phase2_enqueue_commit(&absorbed),
            Phase2EnqueueCommit::NotYetEvidenced
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
