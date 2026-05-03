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
/// teardown lines (final stream item, "[stderr] killed", etc.) so 16 KB is
/// generous yet far below the multi-KB output of even a tiny new turn.
pub(in crate::services::discord) const CANCEL_TEARDOWN_GRACE_BYTES: u64 = 16 * 1024;
pub(in crate::services::discord) const MONITOR_AUTO_TURN_REASON_CODE: &str =
    "lifecycle.monitor_auto_turn";
pub(in crate::services::discord) const MONITOR_AUTO_TURN_DEFERRED_REASON_CODE: &str =
    "lifecycle.monitor_auto_turn.deferred";
pub(in crate::services::discord) const TMUX_LIVENESS_PROBE_INTERVAL: tokio::time::Duration =
    tokio::time::Duration::from_secs(2);

#[derive(Debug, Clone)]
pub(in crate::services::discord) struct RecentTurnStop {
    /// #1309 codex round-3/4 fix: the same UUID is also stamped on the
    /// PG `cancel_tombstones.client_id` row that mirrors this entry.
    /// `cancel_induced_watcher_death` registers drained UUIDs with
    /// `crate::db::cancel_tombstones::register_drained_ids` so a
    /// late-landing PG row carrying the same UUID can be DELETEd without
    /// false-suppressing an unrelated future watcher death.
    pub(in crate::services::discord) id: uuid::Uuid,
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) tmux_session_name: Option<String>,
    pub(in crate::services::discord) stop_output_offset: Option<u64>,
    pub(in crate::services::discord) reason: String,
    pub(in crate::services::discord) recorded_at: std::time::Instant,
}

static RECENT_TURN_STOPS: LazyLock<Mutex<VecDeque<RecentTurnStop>>> =
    LazyLock::new(|| Mutex::new(VecDeque::with_capacity(RECENT_TURN_STOP_CAPACITY)));

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

pub(in crate::services::discord) async fn record_recent_turn_stop(
    channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    reason: &str,
) {
    let stop_output_offset = tmux_session_name.and_then(tmux_output_offset);
    // #1309: in-memory publish is synchronous + immediate so an in-process
    // watcher can suppress the very next death without waiting on PG.
    // The PG insert is awaited (with a 500 ms cap) so a quick dcserver
    // restart immediately after the cancel cannot lose the durable copy.
    // Cross-restart correctness AND in-process race safety are layered:
    //   - in-memory: instant suppression for live watchers
    //   - PG: durable across restart
    //   - shared `client_id` + drained-id registry: skip + delete late
    //     PG rows whose UUID was already drained in-memory
    record_recent_turn_stop_with_offset(
        channel_id,
        tmux_session_name,
        stop_output_offset,
        reason,
        crate::db::cancel_tombstones::global_pool(),
    )
    .await;
}

/// Bounded foreground budget for the durable PG mirror. Normal inserts
/// finish in well under 10 ms; if a saturated pool exceeds this we fall
/// back to in-memory only and warn — the cancel signal must not stall
/// behind PG since `turn_bridge` polls `cancel_token` and could kill the
/// wrapper before the C-c path runs (codex round-3 P2 on PR #1310).
const CANCEL_TOMBSTONE_PERSIST_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

async fn record_recent_turn_stop_with_offset(
    channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    stop_output_offset: Option<u64>,
    reason: &str,
    pg_pool: Option<&sqlx::PgPool>,
) {
    let client_id = uuid::Uuid::new_v4();

    // Phase 1 — publish the in-memory entry synchronously. An in-process
    // watcher firing right after `cancel_active_turn` returns will see
    // the tombstone with zero PG dependency.
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
            reason: reason.to_string(),
            recorded_at: now,
        });
    }

    // Phase 2 — durable PG mirror with a bounded foreground budget. The
    // await guarantees the row is committed before the cancel path
    // returns, so a dcserver restart immediately after the cancel can
    // still see the tombstone (codex round-2/5 P1/P2 on PR #1310). The
    // 500 ms timeout caps worst-case foreground latency under PG
    // saturation.
    if let Some(pool) = pg_pool {
        let channel_id_i64 = channel_id.get() as i64;
        let stop_output_offset_i64 = stop_output_offset.map(|v| v as i64);
        let persist = crate::db::cancel_tombstones::insert_cancel_tombstone(
            pool,
            client_id,
            channel_id_i64,
            tmux_session_name,
            stop_output_offset_i64,
            reason,
        );
        match tokio::time::timeout(CANCEL_TOMBSTONE_PERSIST_TIMEOUT, persist).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(
                    "[cancel-tombstone] PG persist failed for channel {}: {}",
                    channel_id_i64,
                    error
                );
            }
            Err(_) => {
                tracing::warn!(
                    "[cancel-tombstone] PG persist for channel {} exceeded {:?}; \
                     falling back to in-memory only",
                    channel_id_i64,
                    CANCEL_TOMBSTONE_PERSIST_TIMEOUT
                );
            }
        }
    }
}

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
pub(in crate::services::discord) fn cancel_induced_watcher_death(
    channel_id: ChannelId,
    tmux_session_name: &str,
    current_output_offset: Option<u64>,
) -> bool {
    let now = std::time::Instant::now();
    let mut drained_ids: Vec<uuid::Uuid> = Vec::new();
    {
        let mut stops = recent_turn_stops();
        prune_recent_turn_stops(&mut stops, now);
        stops.retain(|entry| {
            if entry.channel_id != channel_id {
                return true;
            }
            if now.saturating_duration_since(entry.recorded_at)
                > RECENT_TURN_STOP_METADATA_FALLBACK_TTL
            {
                return true;
            }
            let session_matches = match entry.tmux_session_name.as_deref() {
                Some(entry_tmux) => entry_tmux == tmux_session_name,
                None => true,
            };
            if !session_matches {
                return true;
            }
            // codex P2 round 3: when both offsets are known, only consume
            // the tombstone if the watcher has not moved past the cancel
            // boundary (with a small grace for the wrapper's teardown
            // bytes between cancel record and session kill). Past that
            // boundary means a follow-up turn produced new output, so the
            // death is unrelated to the cancel and must surface its own
            // lifecycle/handoff signal.
            if let (Some(stop_offset), Some(current_offset)) =
                (entry.stop_output_offset, current_output_offset)
            {
                if current_offset > stop_offset.saturating_add(CANCEL_TEARDOWN_GRACE_BYTES) {
                    return true;
                }
            }
            drained_ids.push(entry.id);
            false
        });
    }
    if !drained_ids.is_empty() {
        // codex round-3/4 fix on PR #1310: register the drained UUIDs so a
        // late-landing PG row carrying any of them is skipped + deleted by
        // `consume_cancel_tombstone` instead of false-suppressing an
        // unrelated future watcher death within the 60 s fallback window.
        crate::db::cancel_tombstones::register_drained_ids(&drained_ids);
        true
    } else {
        false
    }
}

/// PG-aware async wrapper around `cancel_induced_watcher_death` (#1309).
///
/// In-memory hit is the fast path. On miss, fall back to the durable
/// `cancel_tombstones` table so a dcserver restart between cancel and
/// watcher-death observation can still suppress the misleading 🔴 lifecycle
/// notice. The PG row is consumed (DELETEd) in the same tx so suppression
/// remains one-shot per cancel.
pub(in crate::services::discord) async fn cancel_induced_watcher_death_async(
    channel_id: ChannelId,
    tmux_session_name: &str,
    current_output_offset: Option<u64>,
    pg_pool: Option<&sqlx::PgPool>,
) -> bool {
    let in_memory_hit =
        cancel_induced_watcher_death(channel_id, tmux_session_name, current_output_offset);

    let Some(pool) = pg_pool else {
        return in_memory_hit;
    };

    let channel_id_i64 = channel_id.get() as i64;
    let current_offset_i64 = current_output_offset.and_then(|v| i64::try_from(v).ok());

    // codex round-1 P2 on PR #1310: even when the in-memory store hits, the
    // PG mirror needs to be consumed so a follow-up watcher death within the
    // 60s fallback window cannot inherit the stale row and silently swallow
    // a real lifecycle/restart signal. The fire-and-forget insert from the
    // record path may even land after the in-memory consume, so we always
    // try to consume both layers and treat either hit as cancel-induced.
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

    in_memory_hit || pg_hit
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(in crate::services::discord) fn clear_recent_turn_stops_for_tests() {
    recent_turn_stops().clear();
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(in crate::services::discord) fn record_recent_turn_stop_with_offset_for_tests(
    channel_id: ChannelId,
    tmux_session_name: &str,
    stop_output_offset: u64,
    reason: &str,
) {
    // Tests target the in-memory fast path; bypass the async PG mirror so
    // the helper stays sync and existing `#[test]` cases don't need to be
    // rewritten as `#[tokio::test]`.
    let now = std::time::Instant::now();
    let mut stops = recent_turn_stops();
    prune_recent_turn_stops(&mut stops, now);
    while stops.len() >= RECENT_TURN_STOP_CAPACITY {
        stops.pop_front();
    }
    stops.push_back(RecentTurnStop {
        id: uuid::Uuid::new_v4(),
        channel_id,
        tmux_session_name: Some(tmux_session_name.to_string()),
        stop_output_offset: Some(stop_output_offset),
        reason: reason.to_string(),
        recorded_at: now,
    });
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(in crate::services::discord) fn record_recent_turn_stop_for_tests(
    channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    stop_output_offset: Option<u64>,
    reason: &str,
    recorded_at: std::time::Instant,
) {
    let mut stops = recent_turn_stops();
    stops.push_back(RecentTurnStop {
        id: uuid::Uuid::new_v4(),
        channel_id,
        tmux_session_name: tmux_session_name.map(str::to_string),
        stop_output_offset,
        reason: reason.to_string(),
        recorded_at,
    });
}
