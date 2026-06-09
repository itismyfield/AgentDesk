use super::*;

const RECENT_TURN_STOP_CAPACITY: usize = 128;
const RECENT_TURN_STOP_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);
pub(in crate::services::discord) const RECENT_TURN_STOP_METADATA_FALLBACK_TTL: std::time::Duration =
    std::time::Duration::from_secs(60);
/// Slack between the cancel boundary recorded at stop time and the wrapper's
/// post-cancel teardown bytes that flush into the same jsonl before the
/// session actually dies. Anything beyond this boundary is treated as
/// follow-up turn output and disqualifies the death from
/// `cancel_induced_watcher_death`. Empirically the wrapper writes <2 KB of
/// teardown lines (final stream item, "[stderr] killed", etc.). Keep the
/// bound below the smallest observed Codex TUI follow-up rollout frame so a
/// reset/cancel tombstone cannot suppress the next pane-death lifecycle.
pub(in crate::services::discord) const CANCEL_TEARDOWN_GRACE_BYTES: u64 = 4 * 1024;
pub(in crate::services::discord) const MONITOR_AUTO_TURN_REASON_CODE: &str =
    "lifecycle.monitor_auto_turn";
pub(in crate::services::discord) const MONITOR_AUTO_TURN_DEFERRED_REASON_CODE: &str =
    "lifecycle.monitor_auto_turn.deferred";
/// #2441 (H4) — current cadence at which `tmux_output_watcher_with_restore`
/// re-probes `probe_tmux_session_liveness` while waiting for new bytes.
/// A session-local tmux `pane-exited` / `session-closed` hook writes the
/// canonical `pane_dead` marker, and `JsonlWatcher` wakes the watcher on
/// that marker. This 2s probe is retained as the hook-miss safety net for
/// environments where tmux hooks or filesystem notifications are dropped.
pub(in crate::services::discord) const TMUX_LIVENESS_PROBE_INTERVAL: tokio::time::Duration =
    tokio::time::Duration::from_secs(2);

#[derive(Debug, Clone)]
pub(in crate::services::discord) struct RecentTurnStop {
    /// #1309 codex round-3/4 fix: the same UUID is also stamped on the
    /// PG `cancel_tombstones.client_id` row that mirrors this entry. Async
    /// consumers wait for the durable write to finish, then delete exactly
    /// this UUID from PG before reporting the tombstone consumed.
    pub(in crate::services::discord) id: uuid::Uuid,
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) tmux_session_name: Option<String>,
    pub(in crate::services::discord) stop_output_offset: Option<u64>,
    pub(in crate::services::discord) stop_generation_mtime_ns: Option<i64>,
    pub(in crate::services::discord) reason: String,
    pub(in crate::services::discord) recorded_at: std::time::Instant,
    pg_persistence: Option<std::sync::Arc<CancelTombstonePersistence>>,
}

static RECENT_TURN_STOPS: LazyLock<Mutex<VecDeque<RecentTurnStop>>> =
    LazyLock::new(|| Mutex::new(VecDeque::with_capacity(RECENT_TURN_STOP_CAPACITY)));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelTombstonePersistOutcome {
    Persisted,
    Failed,
}

struct CancelTombstonePersistence {
    outcome: Mutex<Option<CancelTombstonePersistOutcome>>,
    notify: tokio::sync::Notify,
}

struct CancelTombstonePersistenceGuard(std::sync::Arc<CancelTombstonePersistence>);

impl Drop for CancelTombstonePersistenceGuard {
    fn drop(&mut self) {
        if self.0.outcome().is_none() {
            self.0.mark(CancelTombstonePersistOutcome::Failed);
        }
    }
}

impl std::fmt::Debug for CancelTombstonePersistence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelTombstonePersistence")
            .field("outcome", &self.outcome())
            .finish_non_exhaustive()
    }
}

impl CancelTombstonePersistence {
    fn new() -> Self {
        Self {
            outcome: Mutex::new(None),
            notify: tokio::sync::Notify::new(),
        }
    }

    fn outcome(&self) -> Option<CancelTombstonePersistOutcome> {
        *self
            .outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn mark(&self, outcome: CancelTombstonePersistOutcome) {
        *self
            .outcome
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(outcome);
        self.notify.notify_waiters();
    }

    async fn wait(&self) -> CancelTombstonePersistOutcome {
        loop {
            let notified = self.notify.notified();
            if let Some(outcome) = self.outcome() {
                return outcome;
            }
            notified.await;
        }
    }
}

fn recent_turn_stops() -> std::sync::MutexGuard<'static, VecDeque<RecentTurnStop>> {
    match RECENT_TURN_STOPS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn prune_recent_turn_stops(stops: &mut VecDeque<RecentTurnStop>, now: std::time::Instant) {
    stops.retain(|entry| now.saturating_duration_since(entry.recorded_at) <= RECENT_TURN_STOP_TTL);
}

pub(in crate::services::discord) fn tmux_output_offset(tmux_session_name: &str) -> Option<u64> {
    let (output_path, _) =
        crate::services::discord::turn_bridge::tmux_runtime_paths(tmux_session_name);
    std::fs::metadata(output_path).ok().map(|meta| meta.len())
}

fn provider_native_output_offset_for_stop(
    channel_id: ChannelId,
    tmux_session_name: &str,
) -> Option<u64> {
    let (provider, _) = parse_provider_and_channel_from_tmux_name(tmux_session_name)?;
    if provider != ProviderKind::Codex {
        return None;
    }

    let state = crate::services::discord::inflight::load_inflight_state(
        &ProviderKind::Codex,
        channel_id.get(),
    )?;
    if !matches!(
        state.runtime_kind,
        Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui)
    ) {
        return None;
    }

    let session_id = state.session_id.as_deref()?;
    let rollout = crate::services::codex_tui::rollout_tail::find_rollout_by_session_id(session_id)?;
    std::fs::metadata(rollout).ok().map(|meta| meta.len())
}

fn tmux_output_offset_for_stop(channel_id: ChannelId, tmux_session_name: &str) -> Option<u64> {
    provider_native_output_offset_for_stop(channel_id, tmux_session_name)
        .or_else(|| tmux_output_offset(tmux_session_name))
}

fn tmux_generation_mtime_for_stop(tmux_session_name: Option<&str>) -> Option<i64> {
    tmux_session_name
        .map(read_generation_file_mtime_ns)
        .filter(|mtime| *mtime > 0)
}

pub(in crate::services::discord) async fn record_recent_turn_stop(
    channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    reason: &str,
) {
    let stop_output_offset =
        tmux_session_name.and_then(|name| tmux_output_offset_for_stop(channel_id, name));
    let stop_generation_mtime_ns = tmux_generation_mtime_for_stop(tmux_session_name);
    // #2549: the in-memory entry is still published immediately so a live
    // watcher can classify the cancel race, but async consumers wait for the
    // PG insert tied to the same UUID before deleting/consuming the row. That
    // makes PG the durable source of truth without the old drained-ID
    // registry used to cover late inserts.
    record_recent_turn_stop_with_offset(
        channel_id,
        tmux_session_name,
        stop_output_offset,
        stop_generation_mtime_ns,
        reason,
        crate::db::cancel_tombstones::global_pool(),
    )
    .await;
}

async fn record_recent_turn_stop_with_offset(
    channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    stop_output_offset: Option<u64>,
    stop_generation_mtime_ns: Option<i64>,
    reason: &str,
    pg_pool: Option<&sqlx::PgPool>,
) {
    let client_id = uuid::Uuid::new_v4();
    let pg_persistence = pg_pool
        .is_some()
        .then(|| std::sync::Arc::new(CancelTombstonePersistence::new()));

    // Phase 1 — publish the in-memory entry synchronously. An in-process
    // watcher firing right after `cancel_active_turn` returns will see the
    // tombstone. If PG is available, the watcher waits on `pg_persistence`
    // before it deletes the durable row, closing the old memory-consume /
    // late-insert race without a third drained-ID layer.
    let now = std::time::Instant::now();
    {
        let mut stops = recent_turn_stops();
        prune_recent_turn_stops(&mut stops, now);
        while stops.len() >= RECENT_TURN_STOP_CAPACITY {
            stops.pop_front();
        }
        stops.push_back(RecentTurnStop {
            id: client_id,
            channel_id,
            tmux_session_name: tmux_session_name.map(str::to_string),
            stop_output_offset,
            stop_generation_mtime_ns,
            reason: reason.to_string(),
            recorded_at: now,
            pg_persistence: pg_persistence.clone(),
        });
    }

    // Phase 2 — durable PG mirror. The await is intentional: PG is the
    // source of truth for cross-restart suppression, and in-process
    // consumers use the shared UUID to delete the committed row exactly once.
    if let (Some(pool), Some(persistence)) = (pg_pool, pg_persistence.as_ref()) {
        let _mark_failed_on_drop = CancelTombstonePersistenceGuard(persistence.clone());
        let channel_id_i64 = channel_id.get() as i64;
        let stop_output_offset_i64 = stop_output_offset.map(|v| v as i64);
        match crate::db::cancel_tombstones::insert_cancel_tombstone(
            pool,
            client_id,
            channel_id_i64,
            tmux_session_name,
            stop_output_offset_i64,
            reason,
        )
        .await
        {
            Ok(()) => {
                persistence.mark(CancelTombstonePersistOutcome::Persisted);
            }
            Err(error) => {
                tracing::warn!(
                    "[cancel-tombstone] PG persist failed for channel {}: {}",
                    channel_id_i64,
                    error
                );
                persistence.mark(CancelTombstonePersistOutcome::Failed);
            }
        }
    }
}

// #3034: per-channel recent-turn-stop query — sibling of the live
// `recent_turn_stop_for_watcher_range`; no live caller yet. Kept as part of the
// cancel-suppression query surface.
#[allow(dead_code)]
pub(in crate::services::discord) fn recent_turn_stop_for_channel(
    channel_id: ChannelId,
) -> Option<RecentTurnStop> {
    let now = std::time::Instant::now();
    let mut stops = recent_turn_stops();
    prune_recent_turn_stops(&mut stops, now);
    stops
        .iter()
        .rev()
        .find(|entry| entry.channel_id == channel_id)
        .cloned()
}

/// Returns true if a watcher death for `(channel_id, tmux_session_name)` was
/// preceded by an explicit user-initiated turn-stop (cancel) within
/// `RECENT_TURN_STOP_METADATA_FALLBACK_TTL`. The watcher cleanup path that
/// follows a cancel writes
/// `record_tmux_exit_reason("watcher cleanup: dead session after turn")`
/// and tears the session down — surfacing that as a 🔴 lifecycle notification
/// or as the "대화를 이어붙이지 못했습니다" handoff is misleading because the
/// death IS the cancel, not a crash.
///
/// IMPORTANT: this consumes ALL matching in-window tombstones on a true
/// return so the suppression is one-shot per cancel (codex P1/P2 on #1277).
/// A single user cancel commonly records two tombstones —
/// `mailbox_cancel_active_turn` records one, and
/// `turn_lifecycle::stop_provider_turn_with_outcome` records another via
/// `record_turn_stop_tombstone` — so draining only the newest leaves the
/// duplicate alive to suppress a follow-up turn's real failure that
/// reuses the same `(channel_id, tmux_session_name)` pair within the 60s
/// metadata-fallback TTL.
///
/// `current_output_offset` is the jsonl size at the moment the watcher
/// observed the death. When the tombstone was recorded with a known
/// `stop_output_offset`, this lets us bound the suppression to the
/// canceled turn's data range (codex P2 round 3 on #1277): for
/// preserve-session stops the tmux session is reused, the wrapper keeps
/// writing past the cancel EOF, and a real crash on the follow-up turn
/// would otherwise be silently swallowed. We allow a small
/// `CANCEL_TEARDOWN_GRACE_BYTES` to accommodate the wrapper's normal
/// post-cancel teardown bytes that flush before the session actually dies.
// #3034: sync cancel-suppression contract (#1277), superseded at live callsites
// by the PG-aware `cancel_induced_watcher_death_async`. Kept as the documented
// reference (db::cancel_tombstones points here) and IO-free counterpart.
#[allow(dead_code)]
pub(in crate::services::discord) fn cancel_induced_watcher_death(
    channel_id: ChannelId,
    tmux_session_name: &str,
    current_output_offset: Option<u64>,
) -> bool {
    !drain_cancel_induced_recent_turn_stops(channel_id, tmux_session_name, current_output_offset)
        .is_empty()
}

fn drain_cancel_induced_recent_turn_stops(
    channel_id: ChannelId,
    tmux_session_name: &str,
    current_output_offset: Option<u64>,
) -> Vec<RecentTurnStop> {
    let now = std::time::Instant::now();
    let mut drained: Vec<RecentTurnStop> = Vec::new();
    let mut stops = recent_turn_stops();
    prune_recent_turn_stops(&mut stops, now);
    stops.retain(|entry| {
        if !recent_turn_stop_matches_watcher_death(
            entry,
            channel_id,
            tmux_session_name,
            current_output_offset,
            now,
        ) {
            return true;
        }
        drained.push(entry.clone());
        false
    });
    drained
}

fn recent_turn_stop_matches_watcher_death(
    entry: &RecentTurnStop,
    channel_id: ChannelId,
    tmux_session_name: &str,
    current_output_offset: Option<u64>,
    now: std::time::Instant,
) -> bool {
    if entry.channel_id != channel_id {
        return false;
    }
    if now.saturating_duration_since(entry.recorded_at) > RECENT_TURN_STOP_METADATA_FALLBACK_TTL {
        return false;
    }
    let session_matches = match entry.tmux_session_name.as_deref() {
        Some(entry_tmux) => entry_tmux == tmux_session_name,
        None => true,
    };
    if !session_matches {
        return false;
    }
    if let (Some(_entry_tmux), Some(stop_generation_mtime_ns)) = (
        entry.tmux_session_name.as_deref(),
        entry.stop_generation_mtime_ns,
    ) {
        let current_generation_mtime_ns = read_generation_file_mtime_ns(tmux_session_name);
        if current_generation_mtime_ns != stop_generation_mtime_ns {
            return false;
        }
    }
    // codex P2 round 3: when both offsets are known, only consume the
    // tombstone if the watcher has not moved past the cancel boundary
    // (with a small grace for the wrapper's teardown bytes between cancel
    // record and session kill). Past that boundary means a follow-up turn
    // produced new output, so the death is unrelated to the cancel and must
    // surface its own lifecycle/handoff signal.
    if let (Some(stop_offset), Some(current_offset)) =
        (entry.stop_output_offset, current_output_offset)
    {
        if current_offset > stop_offset.saturating_add(CANCEL_TEARDOWN_GRACE_BYTES) {
            return false;
        }
    }
    true
}

/// PG-aware async wrapper around `cancel_induced_watcher_death` (#1309).
///
/// In-memory hit handles the same-process attach race, but it waits for the
/// associated PG insert and deletes that exact durable row by UUID before
/// returning. On memory miss, fall back to the `cancel_tombstones` table so a
/// dcserver restart between cancel and watcher-death observation can still
/// suppress the misleading lifecycle notice. The PG row is consumed (DELETEd)
/// in the same tx so suppression remains one-shot per cancel.
pub(in crate::services::discord) async fn cancel_induced_watcher_death_async(
    channel_id: ChannelId,
    tmux_session_name: &str,
    current_output_offset: Option<u64>,
    pg_pool: Option<&sqlx::PgPool>,
) -> bool {
    let in_memory_hits = drain_cancel_induced_recent_turn_stops(
        channel_id,
        tmux_session_name,
        current_output_offset,
    );

    let Some(pool) = pg_pool else {
        return !in_memory_hits.is_empty();
    };

    let channel_id_i64 = channel_id.get() as i64;
    let current_offset_i64 = current_output_offset.and_then(|v| i64::try_from(v).ok());

    if !in_memory_hits.is_empty() {
        let mut persisted_ids = Vec::new();
        for entry in &in_memory_hits {
            let Some(persistence) = entry.pg_persistence.as_ref() else {
                continue;
            };
            match persistence.wait().await {
                CancelTombstonePersistOutcome::Persisted => persisted_ids.push(entry.id),
                CancelTombstonePersistOutcome::Failed => {}
            }
        }

        if !persisted_ids.is_empty()
            && let Err(error) =
                crate::db::cancel_tombstones::delete_cancel_tombstones_by_client_ids(
                    pool,
                    &persisted_ids,
                )
                .await
        {
            tracing::warn!(
                "[cancel-tombstone] PG delete failed for channel {} session {} after memory hit: {}",
                channel_id_i64,
                tmux_session_name,
                error
            );
        }
        return true;
    }

    let pg_hit = match crate::db::cancel_tombstones::consume_cancel_tombstone(
        pool,
        channel_id_i64,
        tmux_session_name,
        current_offset_i64,
    )
    .await
    {
        Ok(consumed) => consumed,
        Err(error) => {
            tracing::warn!(
                "[cancel-tombstone] PG consume failed for channel {} session {}: {}",
                channel_id_i64,
                tmux_session_name,
                error
            );
            false
        }
    };

    pg_hit
}

pub(in crate::services::discord) fn recent_turn_stop_for_watcher_range(
    channel_id: ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
) -> Option<RecentTurnStop> {
    let now = std::time::Instant::now();
    let mut stops = recent_turn_stops();
    prune_recent_turn_stops(&mut stops, now);
    stops
        .iter()
        .rev()
        .find(|entry| {
            recent_turn_stop_matches_watcher_range(
                entry,
                channel_id,
                tmux_session_name,
                data_start_offset,
                now,
            )
        })
        .cloned()
}

fn recent_turn_stop_matches_watcher_range(
    entry: &RecentTurnStop,
    channel_id: ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
    now: std::time::Instant,
) -> bool {
    if entry.channel_id != channel_id {
        return false;
    }

    if let (Some(entry_tmux), Some(stop_offset)) =
        (entry.tmux_session_name.as_deref(), entry.stop_output_offset)
    {
        if let Some(stop_generation_mtime_ns) = entry.stop_generation_mtime_ns {
            let current_generation_mtime_ns = read_generation_file_mtime_ns(tmux_session_name);
            if current_generation_mtime_ns != stop_generation_mtime_ns {
                return false;
            }
        }
        // Exact EOF equality means the next watcher range starts after a clean
        // cancel boundary. Only ranges that began before the stop EOF belong to
        // the canceled turn.
        return entry_tmux == tmux_session_name && data_start_offset < stop_offset;
    }

    let session_matches = entry
        .tmux_session_name
        .as_deref()
        .map_or(true, |entry_tmux| entry_tmux == tmux_session_name);
    session_matches
        && now.saturating_duration_since(entry.recorded_at)
            <= RECENT_TURN_STOP_METADATA_FALLBACK_TTL
}
