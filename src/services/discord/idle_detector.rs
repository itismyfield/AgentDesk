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

use chrono::{DateTime, Utc};
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

fn classify_idle_with_snapshot(
    classification: IdleClassification,
    inflight_state: Option<super::inflight::InflightTurnState>,
) -> (
    IdleClassification,
    Option<super::inflight::InflightTurnState>,
) {
    let signals = inflight_state.as_ref().map(|state| InflightWaitSignals {
        terminal_delivery_committed: state.terminal_delivery_committed,
        task_notification_kind_present: state.task_notification_kind.is_some(),
        long_running_placeholder_active: state.long_running_placeholder_active,
    });
    let effective = if classification == IdleClassification::Idle
        && !should_register_system_detected_idle(signals.as_ref())
    {
        IdleClassification::ActiveAndFresh
    } else {
        classification
    };
    (effective, inflight_state)
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
                .restart
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
        let inflight_state = if classification == IdleClassification::Idle {
            super::inflight::load_inflight_state(provider, channel_id.get())
        } else {
            None
        };
        let (effective, inflight_state) =
            classify_idle_with_snapshot(classification, inflight_state);
        apply_classification(channel_id, effective, health_registry.as_ref()).await;
        if effective == IdleClassification::Idle {
            spawn_idle_expiry_reflect_if_needed(shared, provider, channel_id, inflight_state).await;
        }
    }
}

async fn spawn_idle_expiry_reflect_if_needed(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight_state: Option<super::inflight::InflightTurnState>,
) {
    let session_channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    };
    let Some((memory_scope_channel_id, memory_scope_channel_name)) = resolve_idle_memory_scope(
        shared,
        inflight_state.as_ref(),
        channel_id,
        session_channel_name,
    )
    .await
    else {
        tracing::debug!(
            "idle-detector: unresolved memory scope for channel {}; skipping idle reflect",
            channel_id.get()
        );
        return;
    };
    let role_binding = settings::resolve_role_binding(
        memory_scope_channel_id,
        memory_scope_channel_name.as_deref(),
    );
    let reflect_job = {
        let mut data = shared.core.lock().await;
        let Some(session) = data.sessions.get_mut(&channel_id) else {
            return;
        };
        take_idle_expiry_reflect_request(
            session,
            provider,
            role_binding.as_ref(),
            memory_scope_channel_id,
            memory_scope_channel_name,
        )
    };
    let Some((memory_settings, reflect_request)) = reflect_job else {
        return;
    };
    super::turn_bridge::spawn_memory_reflect_task(channel_id, memory_settings, reflect_request);
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IdleDiscordChannelRelation {
    NonThread,
    Thread {
        parent_id: ChannelId,
        parent_name: Option<String>,
    },
    Unresolved,
}

async fn resolve_idle_memory_scope(
    shared: &SharedData,
    inflight_state: Option<&super::inflight::InflightTurnState>,
    channel_id: ChannelId,
    channel_name: Option<String>,
) -> Option<(ChannelId, Option<String>)> {
    if let Some(scope) = persisted_idle_memory_scope(inflight_state) {
        return Some(scope);
    }
    if settings::resolve_role_binding(channel_id, channel_name.as_deref()).is_some() {
        return Some((channel_id, channel_name));
    }
    let http = shared.serenity_http_or_token_fallback()?;
    let relation = resolve_idle_discord_channel_relation(&http, channel_id).await;
    resolve_no_inflight_idle_scope(channel_id, channel_name, relation)
}

fn persisted_idle_memory_scope(
    inflight_state: Option<&super::inflight::InflightTurnState>,
) -> Option<(ChannelId, Option<String>)> {
    inflight_state.and_then(|state| {
        state.memory_scope_channel_id.map(|scope_id| {
            (
                ChannelId::new(scope_id),
                state.memory_scope_channel_name.clone(),
            )
        })
    })
}

async fn resolve_idle_discord_channel_relation(
    http: &Arc<poise::serenity_prelude::Http>,
    channel_id: ChannelId,
) -> IdleDiscordChannelRelation {
    let Ok(channel) = channel_id.to_channel(http).await else {
        return IdleDiscordChannelRelation::Unresolved;
    };
    let poise::serenity_prelude::Channel::Guild(channel) = channel else {
        return IdleDiscordChannelRelation::NonThread;
    };
    if !crate::utils::discord::is_thread_channel_type(channel.kind) {
        return IdleDiscordChannelRelation::NonThread;
    }
    let Some(parent_id) = channel.parent_id else {
        return IdleDiscordChannelRelation::Unresolved;
    };
    let Ok(poise::serenity_prelude::Channel::Guild(parent)) = parent_id.to_channel(http).await
    else {
        return IdleDiscordChannelRelation::Unresolved;
    };
    IdleDiscordChannelRelation::Thread {
        parent_id,
        parent_name: Some(parent.name),
    }
}

fn resolve_no_inflight_idle_scope(
    channel_id: ChannelId,
    channel_name: Option<String>,
    relation: IdleDiscordChannelRelation,
) -> Option<(ChannelId, Option<String>)> {
    if settings::resolve_role_binding(channel_id, channel_name.as_deref()).is_some() {
        return Some((channel_id, channel_name));
    }
    match relation {
        IdleDiscordChannelRelation::NonThread => Some((channel_id, channel_name)),
        IdleDiscordChannelRelation::Thread {
            parent_id,
            parent_name,
        } => {
            if settings::resolve_inherited_role_binding(parent_id, parent_name.as_deref()).is_some()
            {
                Some((parent_id, parent_name))
            } else {
                Some((channel_id, channel_name))
            }
        }
        IdleDiscordChannelRelation::Unresolved => None,
    }
}

fn take_idle_expiry_reflect_request(
    session: &mut super::DiscordSession,
    provider: &ProviderKind,
    role_binding: Option<&RoleBinding>,
    memory_scope_channel_id: ChannelId,
    memory_scope_channel_name: Option<String>,
) -> Option<(ResolvedMemorySettings, ReflectRequest)> {
    let memory_settings = settings::memory_settings_for_binding(role_binding);
    let reflect_request = super::turn_bridge::take_memento_reflect_request(
        session,
        &memory_settings,
        provider,
        role_binding,
        memory_scope_channel_id.get(),
        memory_scope_channel_name,
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

    fn inflight_without_persisted_scope() -> super::super::inflight::InflightTurnState {
        super::super::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            222,
            Some("thread".to_string()),
            1,
            2,
            3,
            "hello".to_string(),
            Some("provider-session".to_string()),
            None,
            None,
            None,
            0,
        )
    }

    fn inflight_with_parent_scope() -> super::super::inflight::InflightTurnState {
        let mut state = inflight_without_persisted_scope();
        state.set_memory_scope(111, Some("parent".to_string()));
        state
    }

    #[test]
    fn idle_scope_carries_first_parent_snapshot_when_later_lookup_would_be_absent() {
        let state = inflight_with_parent_scope();
        let (effective, first_snapshot) =
            classify_idle_with_snapshot(IdleClassification::Idle, Some(state));
        assert_eq!(effective, IdleClassification::Idle);

        assert_eq!(
            persisted_idle_memory_scope(first_snapshot.as_ref()),
            Some((ChannelId::new(111), Some("parent".to_string()))),
            "the first inflight snapshot must survive classification and avoid any HTTP/reload fallback"
        );
    }

    #[test]
    fn idle_pass_has_exactly_one_inflight_load_site() {
        let source = include_str!("idle_detector.rs");
        let load_call = ["load_inflight_", "state(provider, channel_id.get())"].concat();
        assert_eq!(
            source.matches(&load_call).count(),
            1,
            "idle classification and reflect must share one exact inflight snapshot"
        );
    }

    #[test]
    fn no_inflight_scope_uses_exact_shared_runtime_http() {
        let source = include_str!("idle_detector.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        let exact_runtime_http = ["shared.serenity_http_", "or_token_fallback()"].concat();
        let ambiguous_provider_lookup = ["health::resolve_", "bot_http"].concat();
        assert!(
            production.contains(&exact_runtime_http),
            "no-inflight scope lookup must use the exact SharedData bot runtime"
        );
        assert!(
            !production.contains(&ambiguous_provider_lookup),
            "provider-name registry lookup can select a different same-provider bot"
        );
    }

    #[test]
    fn current_intake_rows_persist_resolved_memory_scope_before_save() {
        for source in [
            include_str!("router/message_handler/intake_turn.rs"),
            include_str!("router/message_handler/headless_turn.rs"),
        ] {
            let construct = source
                .find("let mut inflight_state = InflightTurnState::new(")
                .expect("current intake constructs inflight state");
            let stamp = source[construct..]
                .find("resolved_role.stamp_inflight(&mut inflight_state")
                .map(|offset| construct + offset)
                .expect("current intake stamps resolved memory scope");
            let save = source[stamp..]
                .find("save_inflight_state(&inflight_state)")
                .map(|offset| stamp + offset)
                .expect("current intake persists inflight state");
            assert!(construct < stamp && stamp < save);
        }
    }

    fn with_idle_scope_config(yaml: &str, test: impl FnOnce()) {
        let root = tempfile::tempdir().expect("temp AgentDesk root");
        let config_dir = root.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::write(config_dir.join("agentdesk.yaml"), yaml).expect("write AgentDesk config");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        test();
    }

    const CHILD_ID: u64 = 1479671301387059299;
    const PARENT_ID: u64 = 1479671301387059200;

    fn thread_relation() -> IdleDiscordChannelRelation {
        IdleDiscordChannelRelation::Thread {
            parent_id: ChannelId::new(PARENT_ID),
            parent_name: Some("adk-cdx".to_string()),
        }
    }

    #[test]
    fn no_inflight_thread_uses_authoritative_inherited_parent_scope() {
        with_idle_scope_config(
            &format!(
                r#"server:
  port: 8791
agents:
  - id: project-agentdesk
    name: AgentDesk
    provider: codex
    channels:
      codex:
        id: "{PARENT_ID}"
        name: adk-cdx
"#
            ),
            || {
                assert_eq!(
                    resolve_no_inflight_idle_scope(
                        ChannelId::new(CHILD_ID),
                        Some("thread".to_string()),
                        thread_relation(),
                    ),
                    Some((ChannelId::new(PARENT_ID), Some("adk-cdx".to_string())))
                );
            },
        );
    }

    #[test]
    fn legacy_inflight_without_persisted_scope_re_resolves_typed_parent() {
        with_idle_scope_config(
            &format!(
                r#"server:
  port: 8791
agents:
  - id: project-agentdesk
    name: AgentDesk
    provider: codex
    channels:
      codex:
        id: "{PARENT_ID}"
        name: adk-cdx
"#
            ),
            || {
                let legacy = inflight_without_persisted_scope();
                assert_eq!(
                    persisted_idle_memory_scope(Some(&legacy)),
                    None,
                    "serde-default legacy rows must not make child scope authoritative"
                );
                assert_eq!(
                    resolve_no_inflight_idle_scope(
                        ChannelId::new(CHILD_ID),
                        Some("thread".to_string()),
                        thread_relation(),
                    ),
                    Some((ChannelId::new(PARENT_ID), Some("adk-cdx".to_string())))
                );
            },
        );
    }

    #[test]
    fn no_inflight_direct_child_binding_takes_precedence() {
        with_idle_scope_config(
            &format!(
                r#"server:
  port: 8791
agents:
  - id: direct-child
    name: Direct Child
    provider: codex
    channels:
      codex:
        id: "{CHILD_ID}"
        name: dispatch-child
"#
            ),
            || {
                assert_eq!(
                    resolve_no_inflight_idle_scope(
                        ChannelId::new(CHILD_ID),
                        Some("dispatch-child".to_string()),
                        thread_relation(),
                    ),
                    Some((ChannelId::new(CHILD_ID), Some("dispatch-child".to_string())))
                );
            },
        );
    }

    #[test]
    fn no_inflight_parent_opt_out_keeps_child_scope() {
        with_idle_scope_config(
            &format!(
                r#"server:
  port: 8791
agents:
  - id: project-agentdesk
    name: AgentDesk
    provider: codex
    channels:
      codex:
        id: "{PARENT_ID}"
        name: adk-cdx
        threadInherit: false
"#
            ),
            || {
                assert_eq!(
                    resolve_no_inflight_idle_scope(
                        ChannelId::new(CHILD_ID),
                        Some("thread".to_string()),
                        thread_relation(),
                    ),
                    Some((ChannelId::new(CHILD_ID), Some("thread".to_string())))
                );
            },
        );
    }

    #[test]
    fn no_inflight_resolution_failure_is_fail_closed() {
        with_idle_scope_config("server:\n  port: 8791\nagents: []\n", || {
            assert_eq!(
                resolve_no_inflight_idle_scope(
                    ChannelId::new(CHILD_ID),
                    Some("possible-thread".to_string()),
                    IdleDiscordChannelRelation::Unresolved,
                ),
                None,
                "unknown Discord channel type must never default to child memory scope"
            );
        });
    }

    #[test]
    fn no_inflight_confirmed_non_thread_preserves_child_scope() {
        with_idle_scope_config("server:\n  port: 8791\nagents: []\n", || {
            assert_eq!(
                resolve_no_inflight_idle_scope(
                    ChannelId::new(CHILD_ID),
                    Some("regular-channel".to_string()),
                    IdleDiscordChannelRelation::NonThread,
                ),
                Some((
                    ChannelId::new(CHILD_ID),
                    Some("regular-channel".to_string())
                ))
            );
        });
    }

    #[test]
    fn idle_reflect_uses_persisted_parent_scope() {
        use crate::ui::ai_screen::{HistoryItem, HistoryType};

        let mut session = super::super::DiscordSession {
            session_id: Some("provider-session".to_string()),
            memento_context_loaded: true,
            memento_reflected: false,
            current_path: None,
            history: vec![HistoryItem {
                item_type: HistoryType::User,
                content: "question".to_string(),
            }],
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(222),
            channel_name: Some("thread".to_string()),
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
        };
        let binding = RoleBinding {
            role_id: "project-agentdesk".to_string(),
            prompt_file: String::new(),
            provider: None,
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: true,
            quality_feedback_injection_enabled: true,
            memory: ResolvedMemorySettings {
                backend: settings::MemoryBackendKind::Memento,
                ..ResolvedMemorySettings::default()
            },
        };

        let (_, request) = take_idle_expiry_reflect_request(
            &mut session,
            &ProviderKind::Codex,
            Some(&binding),
            ChannelId::new(111),
            Some("parent".to_string()),
        )
        .expect("idle reflect request");
        assert_eq!(request.channel_id, 111);
        assert_eq!(request.channel_name.as_deref(), Some("parent"));
    }
}
