//! Server-level idle detection for active turns.
//!
//! Tracks channels that have an active turn (sessions.status = 'working'),
//! measures time since the last `sessions.last_heartbeat` refresh, and
//! registers a `system-detected:idle` entry in [`MonitoringStore`] when no
//! tmux output has been observed for `threshold` seconds. The entry is
//! removed automatically when activity resumes or when the turn ends.
//!
//! See GitHub issue #1031.
//!
//! The entry coexists with `agent-registered:*` entries, but is suppressed
//! when the same channel already has a `user-approval:*` or `wave*` entry
//! (those signals are higher priority and convey the same "waiting" idea).
//!
//! Interval is controlled by env var `ADK_IDLE_DETECT_INTERVAL_SECS` (default 30).

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDateTime, TimeZone, Utc};
use poise::serenity_prelude::ChannelId;
use tokio::sync::Mutex;

use super::health;
use super::monitoring_status;
use crate::server::routes::state::{MonitoringEntry, MonitoringStore};

/// Monitoring entry key reserved for this detector.
pub(crate) const SYSTEM_DETECTED_IDLE_KEY: &str = "system-detected:idle";

/// Default description attached to a newly inserted idle entry.
const SYSTEM_DETECTED_IDLE_DESCRIPTION: &str = "에이전트 대기 중(추정) — 30초 이상 출력 없음";

/// Default cadence if `ADK_IDLE_DETECT_INTERVAL_SECS` is unset or invalid.
const DEFAULT_INTERVAL_SECS: u64 = 30;

/// Default idle threshold matches the 30s heartbeat throttle.
const DEFAULT_IDLE_THRESHOLD_SECS: i64 = 30;

/// Active-turn snapshot used by the detector.
#[derive(Clone, Debug)]
pub(crate) struct ActiveChannelSnapshot {
    pub channel_id: u64,
    pub last_heartbeat: Option<DateTime<Utc>>,
}

/// Action to apply to the monitoring store for a given channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IdleAction {
    /// Ensure a `system-detected:idle` entry exists (insert or refresh).
    Insert,
    /// Remove any existing `system-detected:idle` entry.
    Remove,
    /// Leave the channel untouched.
    NoOp,
}

/// Resolve the detector tick interval, honoring the env override.
pub(crate) fn resolve_interval() -> StdDuration {
    let secs = std::env::var("ADK_IDLE_DETECT_INTERVAL_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    StdDuration::from_secs(secs)
}

/// Resolve the idle threshold. Defaults to the same value as the poll interval
/// so a single missed heartbeat triggers the signal. Kept separate so tests
/// can exercise the policy without racing the clock.
pub(crate) fn resolve_idle_threshold() -> ChronoDuration {
    ChronoDuration::seconds(DEFAULT_IDLE_THRESHOLD_SECS)
}

/// Return `true` when the current `entries` for a channel indicate a
/// higher-priority suppression signal is already active. The detector must
/// not overwrite user-approval or wave signals.
pub(crate) fn is_suppressed_by_existing(entries: &[MonitoringEntry]) -> bool {
    entries
        .iter()
        .any(|entry| entry.key.starts_with("user-approval:") || entry.key.starts_with("wave"))
}

/// Pure decision logic: given the current entries for a channel, the active
/// session snapshot, and the wall clock, produce the action the detector
/// should apply. Factored out for straightforward unit testing.
pub(crate) fn decide_idle_action(
    entries: &[MonitoringEntry],
    snapshot: Option<&ActiveChannelSnapshot>,
    now: DateTime<Utc>,
    idle_threshold: ChronoDuration,
) -> IdleAction {
    let has_existing_idle = entries
        .iter()
        .any(|entry| entry.key == SYSTEM_DETECTED_IDLE_KEY);

    let Some(snapshot) = snapshot else {
        // No active turn → any stale entry must be cleared.
        return if has_existing_idle {
            IdleAction::Remove
        } else {
            IdleAction::NoOp
        };
    };

    if is_suppressed_by_existing(entries) {
        return if has_existing_idle {
            IdleAction::Remove
        } else {
            IdleAction::NoOp
        };
    }

    let is_idle = match snapshot.last_heartbeat {
        Some(ts) => now.signed_duration_since(ts) > idle_threshold,
        // Active session without any heartbeat yet: treat as idle.
        None => true,
    };

    match (is_idle, has_existing_idle) {
        (true, false) => IdleAction::Insert,
        (true, true) => IdleAction::NoOp,
        (false, true) => IdleAction::Remove,
        (false, false) => IdleAction::NoOp,
    }
}

/// Apply the idle action to the store for a single channel. Returns `true`
/// when the store changed so the caller can schedule a render.
pub(crate) async fn apply_idle_action(
    store: &Arc<Mutex<MonitoringStore>>,
    channel_id: u64,
    action: IdleAction,
) -> bool {
    match action {
        IdleAction::Insert => {
            let mut guard = store.lock().await;
            guard.upsert(
                channel_id,
                SYSTEM_DETECTED_IDLE_KEY.to_string(),
                SYSTEM_DETECTED_IDLE_DESCRIPTION.to_string(),
            );
            true
        }
        IdleAction::Remove => {
            let mut guard = store.lock().await;
            let before = guard
                .list(channel_id)
                .iter()
                .any(|entry| entry.key == SYSTEM_DETECTED_IDLE_KEY);
            guard.remove(channel_id, SYSTEM_DETECTED_IDLE_KEY);
            before
        }
        IdleAction::NoOp => false,
    }
}

/// Collect active-turn snapshots from SQLite (default) or Postgres.
///
/// Only sessions with `status = 'working'` and a non-null `thread_channel_id`
/// are considered. The `last_heartbeat` column is optional — a NULL value
/// means "never heard from" and the detector treats that as idle immediately.
async fn query_active_channels(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
) -> Vec<ActiveChannelSnapshot> {
    if let Some(pool) = pg_pool {
        match sqlx::query_as::<_, (String, Option<DateTime<Utc>>)>(
            "SELECT thread_channel_id, last_heartbeat
             FROM sessions
             WHERE status = 'working'
               AND thread_channel_id IS NOT NULL",
        )
        .fetch_all(pool)
        .await
        {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|(channel_raw, heartbeat)| {
                    channel_raw
                        .parse::<u64>()
                        .ok()
                        .map(|channel_id| ActiveChannelSnapshot {
                            channel_id,
                            last_heartbeat: heartbeat,
                        })
                })
                .collect(),
            Err(error) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ monitoring_detector: failed to query active sessions (pg): {}",
                    error
                );
                Vec::new()
            }
        }
    } else if let Some(db) = db {
        match db.lock() {
            Ok(conn) => {
                let result = conn
                    .prepare(
                        "SELECT thread_channel_id, last_heartbeat
                         FROM sessions
                         WHERE status = 'working'
                           AND thread_channel_id IS NOT NULL",
                    )
                    .and_then(|mut stmt| {
                        stmt.query_map([], |row| {
                            let channel_raw: String = row.get(0)?;
                            let heartbeat_raw: Option<String> = row.get(1)?;
                            Ok((channel_raw, heartbeat_raw))
                        })?
                        .collect::<libsql_rusqlite::Result<Vec<_>>>()
                    });
                match result {
                    Ok(rows) => rows
                        .into_iter()
                        .filter_map(|(channel_raw, heartbeat_raw)| {
                            let channel_id = channel_raw.parse::<u64>().ok()?;
                            let last_heartbeat =
                                heartbeat_raw.as_deref().and_then(parse_sqlite_datetime);
                            Some(ActiveChannelSnapshot {
                                channel_id,
                                last_heartbeat,
                            })
                        })
                        .collect(),
                    Err(error) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ monitoring_detector: failed to query active sessions (sqlite): {}",
                            error
                        );
                        Vec::new()
                    }
                }
            }
            Err(error) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ monitoring_detector: cannot lock sqlite: {}",
                    error
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    }
}

/// Parse SQLite `datetime('now')` output (formats like
/// `2026-04-24 10:11:12` or `2026-04-24T10:11:12Z`) into `DateTime<Utc>`.
fn parse_sqlite_datetime(raw: &str) -> Option<DateTime<Utc>> {
    let trimmed = raw.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(parsed.with_timezone(&Utc));
    }
    let normalized = trimmed.replace('T', " ");
    let candidate = normalized.strip_suffix('Z').unwrap_or(&normalized);
    NaiveDateTime::parse_from_str(candidate, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(candidate, "%Y-%m-%d %H:%M:%S%.f"))
        .ok()
        .map(|naive| Utc.from_utc_datetime(&naive))
}

/// Spawn the background idle-detector task. Safe to call once at server boot.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn spawn_idle_detector(
    store: Arc<Mutex<MonitoringStore>>,
    health_registry: Option<Arc<health::HealthRegistry>>,
    db: Option<crate::db::Db>,
    pg_pool: Option<sqlx::PgPool>,
) {
    tokio::spawn(async move {
        let interval_duration = resolve_interval();
        let idle_threshold = resolve_idle_threshold();
        let mut ticker = tokio::time::interval(interval_duration);
        // Skip an immediate tick so boot time does not race the first heartbeat.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;

        loop {
            ticker.tick().await;

            let snapshots = query_active_channels(db.as_ref(), pg_pool.as_ref()).await;
            let active_ids: std::collections::HashSet<u64> =
                snapshots.iter().map(|s| s.channel_id).collect();

            // Channels with an active turn → evaluate their idle state.
            let mut affected: Vec<u64> = Vec::new();
            let now = Utc::now();
            for snapshot in &snapshots {
                let entries = {
                    let guard = store.lock().await;
                    guard.list(snapshot.channel_id)
                };
                let action = decide_idle_action(&entries, Some(snapshot), now, idle_threshold);
                if apply_idle_action(&store, snapshot.channel_id, action).await {
                    affected.push(snapshot.channel_id);
                }
            }

            // Channels that previously had our entry but are no longer active →
            // clean up (turn ended without the watcher telling us).
            let lingering: Vec<u64> = {
                let guard = store.lock().await;
                guard
                    .channels_with_entry(SYSTEM_DETECTED_IDLE_KEY)
                    .into_iter()
                    .filter(|channel_id| !active_ids.contains(channel_id))
                    .collect()
            };
            for channel_id in lingering {
                if apply_idle_action(&store, channel_id, IdleAction::Remove).await {
                    affected.push(channel_id);
                }
            }

            for channel_id in affected {
                monitoring_status::schedule_render_channel(
                    store.clone(),
                    health_registry.clone(),
                    ChannelId::new(channel_id),
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, description: &str, started_at: DateTime<Utc>) -> MonitoringEntry {
        MonitoringEntry {
            key: key.to_string(),
            description: description.to_string(),
            started_at,
            last_refresh: started_at,
        }
    }

    fn t(minutes: i64, seconds: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 24, 1, 0, 0).unwrap()
            + ChronoDuration::minutes(minutes)
            + ChronoDuration::seconds(seconds)
    }

    #[test]
    fn idle_threshold_triggers_insert_when_no_existing_entry() {
        let snapshot = ActiveChannelSnapshot {
            channel_id: 42,
            last_heartbeat: Some(t(0, 0)),
        };
        let action =
            decide_idle_action(&[], Some(&snapshot), t(0, 45), ChronoDuration::seconds(30));
        assert_eq!(action, IdleAction::Insert);
    }

    #[test]
    fn recent_heartbeat_requests_removal_when_entry_lingers() {
        let snapshot = ActiveChannelSnapshot {
            channel_id: 42,
            last_heartbeat: Some(t(0, 40)),
        };
        let entries = vec![entry(
            SYSTEM_DETECTED_IDLE_KEY,
            SYSTEM_DETECTED_IDLE_DESCRIPTION,
            t(0, 10),
        )];
        let action = decide_idle_action(
            &entries,
            Some(&snapshot),
            t(0, 45),
            ChronoDuration::seconds(30),
        );
        assert_eq!(action, IdleAction::Remove);
    }

    #[test]
    fn idle_coexists_with_agent_registered_entry() {
        let snapshot = ActiveChannelSnapshot {
            channel_id: 42,
            last_heartbeat: Some(t(0, 0)),
        };
        let entries = vec![entry("agent-registered:td", "TD 등록 대기", t(0, 0))];
        let action = decide_idle_action(
            &entries,
            Some(&snapshot),
            t(0, 45),
            ChronoDuration::seconds(30),
        );
        assert_eq!(action, IdleAction::Insert);
    }

    #[test]
    fn user_approval_suppresses_detector_insert() {
        let snapshot = ActiveChannelSnapshot {
            channel_id: 42,
            last_heartbeat: Some(t(0, 0)),
        };
        let entries = vec![entry("user-approval:skill", "스킬 승인 대기", t(0, 5))];
        let action = decide_idle_action(
            &entries,
            Some(&snapshot),
            t(0, 45),
            ChronoDuration::seconds(30),
        );
        assert_eq!(action, IdleAction::NoOp);
    }

    #[test]
    fn user_approval_causes_removal_when_detector_entry_stale() {
        let snapshot = ActiveChannelSnapshot {
            channel_id: 42,
            last_heartbeat: Some(t(0, 0)),
        };
        let entries = vec![
            entry("user-approval:skill", "스킬 승인 대기", t(0, 5)),
            entry(
                SYSTEM_DETECTED_IDLE_KEY,
                SYSTEM_DETECTED_IDLE_DESCRIPTION,
                t(0, 10),
            ),
        ];
        let action = decide_idle_action(
            &entries,
            Some(&snapshot),
            t(0, 45),
            ChronoDuration::seconds(30),
        );
        assert_eq!(action, IdleAction::Remove);
    }

    #[test]
    fn wave_entry_suppresses_detector() {
        let snapshot = ActiveChannelSnapshot {
            channel_id: 42,
            last_heartbeat: Some(t(0, 0)),
        };
        let entries = vec![entry("wave-2", "wave 2 진행 중", t(0, 5))];
        let action = decide_idle_action(
            &entries,
            Some(&snapshot),
            t(0, 45),
            ChronoDuration::seconds(30),
        );
        assert_eq!(action, IdleAction::NoOp);
    }

    #[test]
    fn turn_ended_causes_removal_of_system_detected_idle() {
        let entries = vec![entry(
            SYSTEM_DETECTED_IDLE_KEY,
            SYSTEM_DETECTED_IDLE_DESCRIPTION,
            t(0, 10),
        )];
        let action = decide_idle_action(&entries, None, t(1, 0), ChronoDuration::seconds(30));
        assert_eq!(action, IdleAction::Remove);
    }

    #[test]
    fn turn_ended_without_entry_is_noop() {
        let action = decide_idle_action(&[], None, t(1, 0), ChronoDuration::seconds(30));
        assert_eq!(action, IdleAction::NoOp);
    }

    #[test]
    fn null_heartbeat_is_treated_as_idle() {
        let snapshot = ActiveChannelSnapshot {
            channel_id: 42,
            last_heartbeat: None,
        };
        let action = decide_idle_action(&[], Some(&snapshot), t(0, 5), ChronoDuration::seconds(30));
        assert_eq!(action, IdleAction::Insert);
    }

    #[tokio::test]
    async fn apply_idle_action_inserts_and_removes_entry() {
        let store = Arc::new(Mutex::new(MonitoringStore::default()));

        let inserted = apply_idle_action(&store, 99, IdleAction::Insert).await;
        assert!(inserted);
        {
            let guard = store.lock().await;
            let entries = guard.list(99);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].key, SYSTEM_DETECTED_IDLE_KEY);
        }

        let removed = apply_idle_action(&store, 99, IdleAction::Remove).await;
        assert!(removed);
        {
            let guard = store.lock().await;
            assert!(guard.list(99).is_empty());
        }
    }

    #[tokio::test]
    async fn apply_noop_leaves_store_unchanged() {
        let store = Arc::new(Mutex::new(MonitoringStore::default()));
        let changed = apply_idle_action(&store, 42, IdleAction::NoOp).await;
        assert!(!changed);
        let guard = store.lock().await;
        assert!(guard.list(42).is_empty());
    }

    #[test]
    fn resolve_interval_honors_env_override() {
        // Use a guard so concurrent tests don't leak env state.
        let _guard = EnvGuard::set("ADK_IDLE_DETECT_INTERVAL_SECS", "7");
        assert_eq!(resolve_interval(), StdDuration::from_secs(7));
    }

    #[test]
    fn resolve_interval_falls_back_to_default_on_bad_value() {
        let _guard = EnvGuard::set("ADK_IDLE_DETECT_INTERVAL_SECS", "not-a-number");
        assert_eq!(
            resolve_interval(),
            StdDuration::from_secs(DEFAULT_INTERVAL_SECS)
        );
    }

    #[test]
    fn resolve_interval_rejects_zero() {
        let _guard = EnvGuard::set("ADK_IDLE_DETECT_INTERVAL_SECS", "0");
        assert_eq!(
            resolve_interval(),
            StdDuration::from_secs(DEFAULT_INTERVAL_SECS)
        );
    }

    #[test]
    fn parse_sqlite_datetime_handles_common_formats() {
        assert!(parse_sqlite_datetime("2026-04-24 10:11:12").is_some());
        assert!(parse_sqlite_datetime("2026-04-24T10:11:12Z").is_some());
        assert!(parse_sqlite_datetime("not-a-date").is_none());
    }

    /// Minimal RAII guard for environment variables used inside tests. A
    /// process-wide mutex serializes mutations so parallel tests don't race.
    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
            let previous = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self {
                key,
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(prev) => std::env::set_var(self.key, prev),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }
}
