//! #1031 server-level idle detection (Option A — turn idle heuristic).
//!
//! Background task that automatically registers a `system-detected:idle`
//! monitoring entry on channels whose mailbox is in an active turn but whose
//! freshness anchor (the more recent of `sessions.last_heartbeat` and the
//! mailbox-tracked `turn_started_at`) has not advanced within the configured
//! threshold.
//!
//! Why this exists:
//!   - The `/api/channels/:id/monitoring` surface introduced in #997 only
//!     populates the "👀 모니터링 중: ..." banner when an agent explicitly
//!     calls the API. If the agent forgets, a user observing the channel has
//!     no way to tell whether the agent is still working or stuck.
//!   - The watcher heartbeat throttle from #982 already records 30s-bucketed
//!     `sessions.last_heartbeat` updates whenever a tmux watcher sees fresh
//!     output. We piggy-back on that signal: an active turn whose freshness
//!     anchor is older than 15 minutes is treated as "에이전트 15분 이상
//!     응답 없음".
//!
//! Why turn-start-aware (#1031 follow-up):
//!   - The original implementation only compared `last_heartbeat` to `now`.
//!     If a channel was idle prior to the new turn, `last_heartbeat` was
//!     already older than the threshold the moment the user kicked off a
//!     fresh turn — producing a 3-second false-positive banner. The fix
//!     plumbs `turn_started_at` from the mailbox actor so a brand-new turn
//!     always counts as fresh until the threshold elapses, regardless of
//!     stale prior-turn heartbeat data.
//!
//! Scope (per the issue):
//!   - Option A only — turn-idle heuristic. Options B/C are deferred.
//!   - Mailbox `cancel_token.is_some()` is treated as the active-turn signal,
//!     matching `health.rs` and `commands/diagnostics.rs`.
//!   - When the heartbeat refreshes (or the mailbox transitions to no-active-turn),
//!     the auto-registered entry is removed in the next polling pass.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};
use poise::serenity_prelude::ChannelId;

use super::SharedData;
use super::monitoring_status;
use crate::server::routes::state::global_monitoring_store;
use crate::services::provider::ProviderKind;

/// Freshness threshold. Active turns whose freshness anchor (the later of
/// `last_heartbeat` and `turn_started_at`) is older than this are treated as
/// stuck. 15 minutes is high-confidence: real long-running agent steps (large
/// builds, model-heavy reasoning) almost always emit watcher output more
/// frequently than that, so the banner avoids false positives during normal
/// operation while still surfacing genuinely hung sessions.
pub(crate) const IDLE_THRESHOLD: Duration = Duration::from_secs(15 * 60);

/// Initial delay before the first poll runs. Defers detection until startup
/// reconcile / recovery has had a chance to refresh heartbeats so we don't
/// flag freshly-restored turns.
pub(crate) const INITIAL_DELAY: Duration = Duration::from_secs(60);

/// How often the detector re-evaluates every active mailbox. Picked to keep
/// the lag between heartbeat-stops and banner appearance bounded by a small
/// multiple of the polling interval without spamming the DB.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Stable monitoring key the detector owns. Documented in the issue and
/// referenced by post-mortem tooling — do not rename without a migration.
pub(crate) const MONITORING_KEY: &str = "system-detected:idle";

/// Default banner text shown when the detector flags a channel. At a 15min
/// threshold this is no longer a soft "추정" — it's a high-confidence stuck
/// signal, so the wording is direct.
const MONITORING_DESCRIPTION: &str = "에이전트 15분 이상 응답 없음";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdleClassification {
    /// Mailbox has no active turn. If a system-detected entry is registered,
    /// it should be cleared.
    NoActiveTurn,
    /// Active turn, heartbeat is recent enough — clear any prior auto-entry.
    ActiveAndFresh,
    /// Active turn, no heartbeat advanced within the threshold — register the
    /// auto-entry.
    Idle,
}

/// Pure classifier suitable for unit testing without DB or tokio runtime.
///
/// Freshness anchor = `max(last_heartbeat, turn_started_at)`:
///   - Either signal alone is enough to keep the channel fresh.
///   - `turn_started_at` defends against the "stale heartbeat from a prior
///     turn" race that produced 3-second false positives.
///   - If both anchors are absent (genuinely no evidence), we still classify
///     as `ActiveAndFresh` rather than `Idle` so a brand-new turn that
///     hasn't yet populated either signal does not flicker the banner —
///     the next poll will pick up `turn_started_at` once the mailbox actor
///     records it.
pub(crate) fn classify(
    has_active_turn: bool,
    last_heartbeat: Option<DateTime<Utc>>,
    turn_started_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    threshold: Duration,
) -> IdleClassification {
    if !has_active_turn {
        return IdleClassification::NoActiveTurn;
    }
    let threshold_chrono = match chrono::Duration::from_std(threshold) {
        Ok(value) => value,
        Err(_) => chrono::Duration::seconds(i64::from(threshold.as_secs() as i32)),
    };
    let anchor = match (last_heartbeat, turn_started_at) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    match anchor {
        Some(value) if now.signed_duration_since(value) <= threshold_chrono => {
            IdleClassification::ActiveAndFresh
        }
        Some(_) => IdleClassification::Idle,
        // No anchor at all: treat as fresh. We only escalate to `Idle` once
        // we have positive evidence that the turn has been active for at
        // least the threshold. This avoids penalizing the first poll after
        // a turn just started.
        None => IdleClassification::ActiveAndFresh,
    }
}

/// Parse `last_heartbeat` strings as written by either Postgres
/// (`TIMESTAMPTZ` rendered to RFC3339) or SQLite (`datetime('now')` ⇒
/// `YYYY-MM-DD HH:MM:SS` UTC). Returns `None` for empty / unrecognized values.
pub(crate) fn parse_last_heartbeat(raw: &str) -> Option<DateTime<Utc>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(value.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|value| DateTime::<Utc>::from_naive_utc_and_offset(value, Utc))
}

/// Spawn the per-provider background task. Cheap to call multiple times
/// because each provider has its own `SharedData`. The task lives for the
/// remainder of the dcserver process.
pub(super) fn spawn_idle_detector(shared: Arc<SharedData>, provider: ProviderKind) {
    tokio::spawn(async move {
        tokio::time::sleep(INITIAL_DELAY).await;
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if shared
                .shutting_down
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return;
            }
            run_pass(shared.as_ref(), &provider).await;
        }
    });
}

/// Evaluate every channel currently held by the provider's mailbox registry
/// and reconcile the `system-detected:idle` monitoring entry against the
/// heartbeat staleness signal.
async fn run_pass(shared: &SharedData, provider: &ProviderKind) {
    let snapshots = shared.mailbox_snapshots_for_idle_detector().await;
    if snapshots.is_empty() {
        return;
    }

    let now = Utc::now();
    let health_registry = shared.health_registry_for_idle_detector();
    for (channel_id, has_active_turn, in_recovery, turn_started_at) in snapshots {
        let last_heartbeat = if has_active_turn {
            fetch_last_heartbeat(shared, provider, channel_id).await
        } else {
            None
        };
        let classification = if in_recovery {
            IdleClassification::ActiveAndFresh
        } else {
            classify(
                has_active_turn,
                last_heartbeat,
                turn_started_at,
                now,
                IDLE_THRESHOLD,
            )
        };
        apply_classification(channel_id, classification, health_registry.as_ref()).await;
    }
}

async fn apply_classification(
    channel_id: ChannelId,
    classification: IdleClassification,
    health_registry: Option<&Arc<super::health::HealthRegistry>>,
) {
    match classification {
        IdleClassification::Idle => {
            register_idle_entry(channel_id, health_registry).await;
        }
        IdleClassification::ActiveAndFresh | IdleClassification::NoActiveTurn => {
            clear_idle_entry(channel_id, health_registry).await;
        }
    }
}

async fn register_idle_entry(
    channel_id: ChannelId,
    health_registry: Option<&Arc<super::health::HealthRegistry>>,
) {
    let store = global_monitoring_store();
    let already_registered = {
        let guard = store.lock().await;
        guard
            .list(channel_id.get())
            .into_iter()
            .any(|entry| entry.key == MONITORING_KEY)
    };
    {
        let mut guard = store.lock().await;
        guard.upsert(
            channel_id.get(),
            MONITORING_KEY.to_string(),
            MONITORING_DESCRIPTION.to_string(),
        );
    }
    // Re-render only when the entry is newly registered. Subsequent polls
    // refresh the entry's `last_refresh` timestamp without churning Discord.
    if !already_registered {
        monitoring_status::schedule_render_channel(
            global_monitoring_store(),
            health_registry.cloned(),
            channel_id,
        );
    }
}

async fn clear_idle_entry(
    channel_id: ChannelId,
    health_registry: Option<&Arc<super::health::HealthRegistry>>,
) {
    let store = global_monitoring_store();
    let removed = {
        let mut guard = store.lock().await;
        let was_present = guard
            .list(channel_id.get())
            .into_iter()
            .any(|entry| entry.key == MONITORING_KEY);
        guard.remove(channel_id.get(), MONITORING_KEY);
        was_present
    };
    if removed {
        monitoring_status::schedule_render_channel(
            global_monitoring_store(),
            health_registry.cloned(),
            channel_id,
        );
    }
}

async fn fetch_last_heartbeat(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<DateTime<Utc>> {
    let provider_name = provider.as_str().to_string();
    let thread_channel_id = channel_id.get().to_string();

    if let Some(pool) = shared.pg_pool.as_ref() {
        let pool = pool.clone();
        let result: Result<Option<DateTime<Utc>>, sqlx::Error> =
            sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
                "SELECT last_heartbeat
             FROM sessions
             WHERE provider = $1 AND thread_channel_id = $2
             ORDER BY COALESCE(last_heartbeat, created_at) DESC
             LIMIT 1",
            )
            .bind(&provider_name)
            .bind(&thread_channel_id)
            .fetch_optional(&pool)
            .await
            .map(|row| row.flatten());
        match result {
            Ok(value) => return value,
            Err(error) => {
                tracing::debug!(
                    "idle-detector: pg heartbeat lookup failed for channel {}: {error}",
                    channel_id.get()
                );
            }
        }
    }

    #[cfg(test)]
    if let Some(db) = shared.legacy_sqlite() {
        let db = db.clone();
        let provider_name_clone = provider_name.clone();
        let thread_channel_id_clone = thread_channel_id.clone();
        let result = tokio::task::spawn_blocking(move || -> Option<String> {
            let conn = db.read_conn().ok()?;
            conn.query_row(
                "SELECT last_heartbeat
                 FROM sessions
                 WHERE provider = ?1 AND thread_channel_id = ?2
                 ORDER BY COALESCE(last_heartbeat, created_at) DESC
                 LIMIT 1",
                [
                    provider_name_clone.as_str(),
                    thread_channel_id_clone.as_str(),
                ],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        })
        .await;
        if let Ok(Some(raw)) = result {
            return parse_last_heartbeat(&raw);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    /// Helper: a heartbeat older than the 15-minute threshold.
    fn stale_heartbeat(now: DateTime<Utc>) -> DateTime<Utc> {
        now - ChronoDuration::from_std(IDLE_THRESHOLD).expect("threshold convert")
            - ChronoDuration::seconds(1)
    }

    #[test]
    fn classify_no_active_turn_returns_no_active_turn() {
        let now = Utc::now();
        assert_eq!(
            classify(false, None, None, now, IDLE_THRESHOLD),
            IdleClassification::NoActiveTurn
        );
        assert_eq!(
            classify(false, Some(now), Some(now), now, IDLE_THRESHOLD),
            IdleClassification::NoActiveTurn
        );
    }

    #[test]
    fn classify_active_with_recent_heartbeat_is_fresh() {
        let now = Utc::now();
        let recent = now - ChronoDuration::seconds(10);
        assert_eq!(
            classify(true, Some(recent), None, now, IDLE_THRESHOLD),
            IdleClassification::ActiveAndFresh
        );
    }

    #[test]
    fn classify_active_with_stale_heartbeat_is_idle() {
        let now = Utc::now();
        let stale = stale_heartbeat(now);
        let stale_turn = stale - ChronoDuration::seconds(60);
        // Both anchors stale → Idle.
        assert_eq!(
            classify(true, Some(stale), Some(stale_turn), now, IDLE_THRESHOLD),
            IdleClassification::Idle
        );
    }

    #[test]
    fn classify_active_with_missing_heartbeat_falls_back_to_fresh() {
        // No heartbeat AND no turn_started_at → fall back to ActiveAndFresh.
        // Previously this produced a false-positive Idle on the first poll
        // after a brand-new turn (the #1031 UX bug).
        let now = Utc::now();
        assert_eq!(
            classify(true, None, None, now, IDLE_THRESHOLD),
            IdleClassification::ActiveAndFresh
        );
    }

    #[test]
    fn classify_active_at_exact_threshold_is_fresh() {
        let now = Utc::now();
        let edge = now - ChronoDuration::from_std(IDLE_THRESHOLD).expect("threshold convert");
        assert_eq!(
            classify(true, Some(edge), None, now, IDLE_THRESHOLD),
            IdleClassification::ActiveAndFresh
        );
    }

    #[test]
    fn classify_active_just_past_threshold_is_idle() {
        let now = Utc::now();
        let edge = stale_heartbeat(now);
        let stale_turn = edge - ChronoDuration::seconds(60);
        assert_eq!(
            classify(true, Some(edge), Some(stale_turn), now, IDLE_THRESHOLD),
            IdleClassification::Idle
        );
    }

    #[test]
    fn classify_idle_to_active_transition() {
        // Simulates the "watcher just emitted output" path: prior pass classified
        // the channel as Idle (heartbeat too old); after the heartbeat ticks,
        // the next pass should classify as ActiveAndFresh so the auto entry
        // is cleared.
        let now = Utc::now();
        let stale = stale_heartbeat(now);
        let stale_turn = stale - ChronoDuration::seconds(60);
        assert_eq!(
            classify(true, Some(stale), Some(stale_turn), now, IDLE_THRESHOLD),
            IdleClassification::Idle
        );
        let refreshed = now - ChronoDuration::seconds(2);
        assert_eq!(
            classify(true, Some(refreshed), Some(stale_turn), now, IDLE_THRESHOLD),
            IdleClassification::ActiveAndFresh
        );
    }

    /// #1031 UX fix: a freshly-started turn whose `last_heartbeat` is from
    /// the *prior* idle period must not flip to Idle on the very next poll.
    /// `turn_started_at` (within threshold) wins.
    #[test]
    fn classify_fresh_turn_start_overrides_stale_heartbeat() {
        let now = Utc::now();
        let stale_hb = stale_heartbeat(now);
        let fresh_turn = now - ChronoDuration::seconds(3);
        assert_eq!(
            classify(true, Some(stale_hb), Some(fresh_turn), now, IDLE_THRESHOLD),
            IdleClassification::ActiveAndFresh
        );
    }

    /// Both anchors exist and both are older than threshold → Idle.
    #[test]
    fn classify_old_turn_and_old_heartbeat_is_idle() {
        let now = Utc::now();
        let stale_hb = stale_heartbeat(now);
        let stale_turn = stale_hb - ChronoDuration::seconds(60);
        assert_eq!(
            classify(true, Some(stale_hb), Some(stale_turn), now, IDLE_THRESHOLD),
            IdleClassification::Idle
        );
    }

    /// `last_heartbeat` is the more-recent anchor — wins over a very old
    /// `turn_started_at` (long-running turn that's been steadily emitting
    /// output).
    #[test]
    fn classify_fresh_heartbeat_overrides_old_turn_start() {
        let now = Utc::now();
        let very_old_turn = now - ChronoDuration::hours(2);
        let fresh_hb = now - ChronoDuration::seconds(5);
        assert_eq!(
            classify(
                true,
                Some(fresh_hb),
                Some(very_old_turn),
                now,
                IDLE_THRESHOLD
            ),
            IdleClassification::ActiveAndFresh
        );
    }

    /// `turn_started_at` alone (no heartbeat yet) within threshold → fresh.
    #[test]
    fn classify_only_turn_started_at_within_threshold_is_fresh() {
        let now = Utc::now();
        let fresh_turn = now - ChronoDuration::seconds(30);
        assert_eq!(
            classify(true, None, Some(fresh_turn), now, IDLE_THRESHOLD),
            IdleClassification::ActiveAndFresh
        );
    }

    /// `turn_started_at` alone past threshold → Idle (positive evidence the
    /// turn has been active long enough without any heartbeat refresh).
    #[test]
    fn classify_only_turn_started_at_past_threshold_is_idle() {
        let now = Utc::now();
        let stale_turn = stale_heartbeat(now);
        assert_eq!(
            classify(true, None, Some(stale_turn), now, IDLE_THRESHOLD),
            IdleClassification::Idle
        );
    }

    #[test]
    fn parse_last_heartbeat_handles_rfc3339() -> Result<(), String> {
        let parsed =
            parse_last_heartbeat("2026-04-25T01:02:03+00:00").ok_or("rfc3339 parse failed")?;
        assert_eq!(parsed.timestamp(), 1777078923);
        Ok(())
    }

    #[test]
    fn parse_last_heartbeat_handles_sqlite_datetime_now_format() -> Result<(), String> {
        let parsed = parse_last_heartbeat("2026-04-25 01:02:03").ok_or("sqlite parse failed")?;
        assert_eq!(parsed.timestamp(), 1777078923);
        Ok(())
    }

    #[test]
    fn parse_last_heartbeat_returns_none_for_blank() {
        assert!(parse_last_heartbeat("").is_none());
        assert!(parse_last_heartbeat("   ").is_none());
        assert!(parse_last_heartbeat("not-a-timestamp").is_none());
    }

    /// Drive the apply step end-to-end through the shared monitoring store.
    /// The `health_registry` is intentionally `None` so `schedule_render_channel`
    /// short-circuits (no live Discord HTTP) — we only want to verify the store
    /// transitions: nothing → idle entry → cleared.
    #[tokio::test]
    async fn apply_idle_then_active_round_trips_store() {
        let channel_id = ChannelId::new(8800_0000_0000_0001);

        // Sanity: ensure no leftover entry from a prior test run.
        {
            let store = global_monitoring_store();
            let mut guard = store.lock().await;
            guard.remove(channel_id.get(), MONITORING_KEY);
        }

        apply_classification(channel_id, IdleClassification::Idle, None).await;
        {
            let store = global_monitoring_store();
            let guard = store.lock().await;
            let has_idle = guard
                .list(channel_id.get())
                .into_iter()
                .any(|entry| entry.key == MONITORING_KEY);
            assert!(has_idle, "Idle classification should register entry");
        }

        apply_classification(channel_id, IdleClassification::ActiveAndFresh, None).await;
        {
            let store = global_monitoring_store();
            let guard = store.lock().await;
            let has_idle = guard
                .list(channel_id.get())
                .into_iter()
                .any(|entry| entry.key == MONITORING_KEY);
            assert!(
                !has_idle,
                "ActiveAndFresh classification should remove entry"
            );
        }
    }

    /// `NoActiveTurn` after `Idle` must also clear the entry — covers the
    /// "agent finished its turn while we were monitoring" path.
    #[tokio::test]
    async fn apply_idle_then_no_active_turn_clears_store() {
        let channel_id = ChannelId::new(8800_0000_0000_0002);

        {
            let store = global_monitoring_store();
            let mut guard = store.lock().await;
            guard.remove(channel_id.get(), MONITORING_KEY);
        }

        apply_classification(channel_id, IdleClassification::Idle, None).await;
        apply_classification(channel_id, IdleClassification::NoActiveTurn, None).await;

        let store = global_monitoring_store();
        let guard = store.lock().await;
        let has_idle = guard
            .list(channel_id.get())
            .into_iter()
            .any(|entry| entry.key == MONITORING_KEY);
        assert!(!has_idle, "NoActiveTurn classification should remove entry");
    }
}
