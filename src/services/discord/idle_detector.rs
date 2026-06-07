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
use super::settings::{self, ResolvedMemorySettings, RoleBinding};
use crate::services::memory::{ReflectRequest, SessionEndReason};
use crate::services::monitoring_store::global_monitoring_store;
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

/// DB/serde-free copy of the three inflight signals the idle gate consults.
/// Keeping this a plain `Copy` struct lets `should_register_system_detected_idle`
/// stay a pure, unit-testable function (no `InflightTurnState` / serde / disk).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct InflightWaitSignals {
    /// Terminal response already committed to delivery. The turn is
    /// intentionally quiet now (ScheduleWakeup sleep / agent-loop wind-down).
    pub terminal_delivery_committed: bool,
    /// `task_notification_kind.is_some()` — an explicit background-work
    /// notification (Monitor / background Bash / Task / Agent) is in flight.
    pub task_notification_kind_present: bool,
    /// A long-running background placeholder is active for this turn.
    pub long_running_placeholder_active: bool,
}

/// Pure gate: given that `classify()` decided `Idle` on the heartbeat
/// anchor, should we actually register `system-detected:idle`? Returns
/// `false` (suppress) when inflight signals prove the stalled-heartbeat
/// turn is an INTENTIONAL wait, not a genuine hang. Mirrors the
/// stall-watchdog #3126 suppression (recovery.rs `stall_watchdog_should_force_clean`).
///
/// The gate is the logical inverse of "if active then suppress": it
/// suppresses ONLY on POSITIVE wait-evidence, so a genuine hang (no
/// committed delivery, no background work) and a missing inflight row both
/// still register — preserving pre-#3146 detection with no regression.
pub(crate) fn should_register_system_detected_idle(inflight: Option<&InflightWaitSignals>) -> bool {
    match inflight {
        // A completed-then-idle turn (ScheduleWakeup / loop wind-down).
        Some(s) if s.terminal_delivery_committed => false,
        // Explicit background work in flight (Monitor / Bash / Task / Agent).
        Some(s) if s.task_notification_kind_present || s.long_running_placeholder_active => false,
        // Either: inflight present but none of the wait-signals set (genuine
        // hang), OR no inflight row at all (genuine hang — preserves current
        // behavior, no regression).
        _ => true,
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
        // #3146 Part 2: when the heartbeat anchor says `Idle`, consult the
        // inflight signals before registering the banner. A TUI turn that is
        // intentionally waiting (ScheduleWakeup sleep / committed-then-idle /
        // explicit background work) produces no fresh tmux output, so its
        // heartbeat stalls — but that is NOT a hang. We load inflight only on
        // the `Idle` branch to avoid a disk read every poll for fresh channels.
        // Downgrading to `ActiveAndFresh` (rather than just skipping) also
        // CLEARS a banner that was registered while the turn was hung but has
        // since transitioned into a committed/background wait.
        let effective = if classification == IdleClassification::Idle {
            let signals =
                super::inflight::load_inflight_state(provider, channel_id.get()).map(|state| {
                    InflightWaitSignals {
                        terminal_delivery_committed: state.terminal_delivery_committed,
                        task_notification_kind_present: state.task_notification_kind.is_some(),
                        long_running_placeholder_active: state.long_running_placeholder_active,
                    }
                });
            if should_register_system_detected_idle(signals.as_ref()) {
                IdleClassification::Idle
            } else {
                IdleClassification::ActiveAndFresh
            }
        } else {
            classification
        };
        apply_classification(channel_id, effective, health_registry.as_ref()).await;
        if effective == IdleClassification::Idle {
            spawn_idle_expiry_reflect_if_needed(shared, provider, channel_id).await;
        }
    }
}

async fn spawn_idle_expiry_reflect_if_needed(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
) {
    let channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    };
    let role_binding = settings::resolve_role_binding(channel_id, channel_name.as_deref());
    let reflect_job = {
        let mut data = shared.core.lock().await;
        let Some(session) = data.sessions.get_mut(&channel_id) else {
            return;
        };
        take_idle_expiry_reflect_request(session, provider, role_binding.as_ref(), channel_id)
    };
    let Some((memory_settings, reflect_request)) = reflect_job else {
        return;
    };
    super::turn_bridge::spawn_memory_reflect_task(channel_id, memory_settings, reflect_request);
}

fn take_idle_expiry_reflect_request(
    session: &mut super::DiscordSession,
    provider: &ProviderKind,
    role_binding: Option<&RoleBinding>,
    channel_id: ChannelId,
) -> Option<(ResolvedMemorySettings, ReflectRequest)> {
    let memory_settings = settings::memory_settings_for_binding(role_binding);
    let reflect_request = super::turn_bridge::take_memento_reflect_request(
        session,
        &memory_settings,
        provider,
        role_binding,
        channel_id.get(),
        SessionEndReason::IdleExpiry,
    )?;
    Some((memory_settings, reflect_request))
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

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- #3146 Part 2: idle-registration gate ----
    //
    // The gate (`should_register_system_detected_idle`) is layered on top of
    // the UNCHANGED `classify()`. `classify()` still produces the `Idle`
    // decision from the heartbeat anchor; the gate then decides whether that
    // `Idle` should actually surface the banner. These tests cover BOTH the
    // suppress case (intentional wait) and the still-detect case (genuine
    // hang) so we prove the gate does not regress detection.

    fn signals(
        terminal_delivery_committed: bool,
        task_notification_kind_present: bool,
        long_running_placeholder_active: bool,
    ) -> InflightWaitSignals {
        InflightWaitSignals {
            terminal_delivery_committed,
            task_notification_kind_present,
            long_running_placeholder_active,
        }
    }

    #[test]
    fn suppress_when_terminal_delivery_committed() {
        // ScheduleWakeup sleep / agent-loop wind-down: terminal response is
        // already committed, so the stalled heartbeat is an intentional quiet,
        // not a hang. Exactly the #3126 stall-watchdog gate.
        let s = signals(true, false, false);
        assert!(!should_register_system_detected_idle(Some(&s)));
    }

    #[test]
    fn suppress_when_background_task_notification_present() {
        // Explicit background work in flight (Monitor / Bash / Task / Agent).
        let s = signals(false, true, false);
        assert!(!should_register_system_detected_idle(Some(&s)));
    }

    #[test]
    fn suppress_when_long_running_placeholder_active() {
        // Long-running background placeholder — no tmux output expected.
        let s = signals(false, false, true);
        assert!(!should_register_system_detected_idle(Some(&s)));
    }

    #[test]
    fn detect_genuine_hang_inflight_present_no_wait_signals() {
        // Turn started, never committed output, no background work, heartbeat
        // stale >= threshold => genuine hang. MUST still register.
        let s = signals(false, false, false);
        assert!(should_register_system_detected_idle(Some(&s)));
    }

    #[test]
    fn detect_genuine_hang_when_no_inflight_row() {
        // Critical regression guard: absence of an inflight row is NOT a wait.
        // Preserves pre-#3146 behavior exactly (register on stale anchor).
        assert!(should_register_system_detected_idle(None));
    }

    #[test]
    fn classify_still_returns_idle_for_stale_anchor() {
        // The gate is layered on top of an UNCHANGED classifier. A stale
        // anchor on an active, non-recovery turn still yields `Idle`; the gate
        // alone decides whether the banner surfaces.
        let now = Utc::now();
        let stale = now - chrono::Duration::minutes(30);
        let result = classify(true, Some(stale), Some(stale), now, IDLE_THRESHOLD);
        assert_eq!(result, IdleClassification::Idle);
    }
}
