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
//!   - tui_o turns run without that watcher, so their newest transcript record also
//!     counts, and a quiet stretch inside an unreturned tool call is named as such.
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

/// Banner text when the quiet stretch sits inside a tool call that has not returned yet.
const TOOL_RUNNING_DESCRIPTION: &str = "도구 실행 중 · 15분 이상 새 기록 없음";

/// Banner text when the quiet stretch's tool state cannot be read from the transcript tail.
const TOOL_UNKNOWN_DESCRIPTION: &str = "15분 이상 새 기록 없음 · 도구 상태 확인 불가";

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
    /// Like `Idle`, but the transcript shows a tool call still waiting for its
    /// result; the entry says so instead of "응답 없음".
    ToolRunning,
    /// Like `Idle`, but the transcript tail cannot show whether a tool call is
    /// still running; the entry says the tool state is unknown.
    ToolStateUnknown,
}

impl IdleClassification {
    /// Whether the verdict registers the monitoring entry.
    fn registers(self) -> bool {
        matches!(
            self,
            Self::Idle | Self::ToolRunning | Self::ToolStateUnknown
        )
    }
}

/// What a tui_o channel's provider transcript shows about the open turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TurnProgress {
    /// Timestamp of the newest provider conversation record.
    pub output_at: Option<DateTime<Utc>>,
    /// Read only when `output_at` alone cannot keep the turn fresh.
    pub tool_calls: Option<crate::services::tui_turn_state::ToolCalls>,
}

/// Records stamped further ahead than this are not progress evidence.
const FUTURE_RECORD_SKEW: chrono::Duration = chrono::Duration::seconds(60);

/// Whether a record stamped `at` may count as progress at `now`.
fn usable_record_time(at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    at <= now + FUTURE_RECORD_SKEW
}

/// `classify` with the transcript's newest record as one more freshness anchor. Without
/// progress evidence the verdict is exactly `classify`'s.
pub(crate) fn classify_with_progress(
    has_active_turn: bool,
    last_heartbeat: Option<DateTime<Utc>>,
    turn_started_at: Option<DateTime<Utc>>,
    progress: Option<TurnProgress>,
    now: DateTime<Utc>,
    threshold: Duration,
) -> IdleClassification {
    let output_at = progress
        .and_then(|progress| progress.output_at)
        .filter(|at| usable_record_time(*at, now));
    let freshness = last_heartbeat.max(output_at);
    use crate::services::tui_turn_state::ToolCalls;
    match classify(has_active_turn, freshness, turn_started_at, now, threshold) {
        IdleClassification::Idle => match progress.and_then(|p| p.tool_calls) {
            Some(ToolCalls::Open) => IdleClassification::ToolRunning,
            Some(ToolCalls::Inconclusive) => IdleClassification::ToolStateUnknown,
            Some(ToolCalls::Closed) | None => IdleClassification::Idle,
        },
        other => other,
    }
}

/// A tui_o turn's progress from its tmux binding's transcript, `None` when O does not own the
/// channel; tool calls are skipped only when a usable record already keeps the turn fresh.
pub(crate) fn observe_turn_progress(
    channel: u64,
    tmux_session: &str,
    now: DateTime<Utc>,
) -> Option<TurnProgress> {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    if !super::health::o_owns(channel) {
        return None;
    }
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session);
    let Some((provider, path)) = binding.and_then(|binding| {
        let provider = match binding.runtime_kind {
            RuntimeHandoffKind::ClaudeTui => ProviderKind::Claude,
            RuntimeHandoffKind::CodexTui => ProviderKind::Codex,
            _ => return None,
        };
        Some((provider, std::path::PathBuf::from(binding.output_path)))
    }) else {
        return Some(TurnProgress::default());
    };
    let latest = crate::services::tui_turn_state::latest_provider_record_unix_nanos_widening;
    let output_at = latest(&provider, &path).map(DateTime::from_timestamp_nanos);
    let quiet_before = now - chrono::Duration::from_std(IDLE_THRESHOLD).ok()?;
    let fresh = output_at.is_some_and(|at| at >= quiet_before && usable_record_time(at, now));
    let tool_calls =
        (!fresh).then(|| crate::services::tui_turn_state::provider_tool_calls(&provider, &path));
    Some(TurnProgress {
        output_at,
        tool_calls,
    })
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
        let progress = if has_active_turn && !in_recovery {
            fetch_turn_progress(shared, provider, channel_id, now).await
        } else {
            None
        };
        let classification = if in_recovery {
            IdleClassification::ActiveAndFresh
        } else {
            classify_with_progress(
                has_active_turn,
                last_heartbeat,
                turn_started_at,
                progress,
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
        let effective = if classification.registers() {
            let signals =
                super::inflight::load_inflight_state(provider, channel_id.get()).map(|state| {
                    InflightWaitSignals {
                        terminal_delivery_committed: state.terminal_delivery_committed,
                        task_notification_kind_present: state.task_notification_kind.is_some(),
                        long_running_placeholder_active: state.long_running_placeholder_active,
                    }
                });
            if should_register_system_detected_idle(signals.as_ref()) {
                classification
            } else {
                IdleClassification::ActiveAndFresh
            }
        } else {
            classification
        };
        apply_classification(channel_id, effective, health_registry.as_ref()).await;
        if effective.registers() {
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
            register_idle_entry(channel_id, MONITORING_DESCRIPTION, health_registry).await;
        }
        IdleClassification::ToolRunning => {
            register_idle_entry(channel_id, TOOL_RUNNING_DESCRIPTION, health_registry).await;
        }
        IdleClassification::ToolStateUnknown => {
            register_idle_entry(channel_id, TOOL_UNKNOWN_DESCRIPTION, health_registry).await;
        }
        IdleClassification::ActiveAndFresh | IdleClassification::NoActiveTurn => {
            clear_idle_entry(channel_id, health_registry).await;
        }
    }
}

async fn register_idle_entry(
    channel_id: ChannelId,
    description: &str,
    health_registry: Option<&Arc<super::health::HealthRegistry>>,
) {
    let store = global_monitoring_store();
    let already_registered = {
        let guard = store.lock().await;
        guard
            .list(channel_id.get())
            .into_iter()
            .any(|entry| entry.key == MONITORING_KEY && entry.description == description)
    };
    {
        let mut guard = store.lock().await;
        guard.upsert(
            channel_id.get(),
            MONITORING_KEY.to_string(),
            description.to_string(),
        );
    }
    // Re-render only when the entry is new or its text changed. Subsequent polls
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

/// The channel's transcript progress, read off the async runtime; `None` when the channel is not
/// O-owned or has no named session.
async fn fetch_turn_progress(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    now: DateTime<Utc>,
) -> Option<TurnProgress> {
    let channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    }?;
    let tmux_session = provider.build_tmux_session_name(&channel_name);
    let channel = channel_id.get();
    tokio::task::spawn_blocking(move || observe_turn_progress(channel, &tmux_session, now))
        .await
        .ok()
        .flatten()
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

    const CHANNEL: u64 = 8_196_000_001;
    const CODEX_CHANNEL: u64 = 8_196_000_003;
    use crate::services::agent_protocol::RuntimeHandoffKind::{self, ClaudeTui, CodexTui};
    use IdleClassification::{ActiveAndFresh, Idle, ToolRunning, ToolStateUnknown};

    /// Writes `(minutes ago, row)` records to `path` with their timestamps.
    fn write_rows(path: &std::path::Path, now: DateTime<Utc>, rows: &[(i64, &str)]) {
        let lines: Vec<String> = rows
            .iter()
            .map(|(ago, row)| {
                let at = (now - chrono::Duration::minutes(*ago)).to_rfc3339();
                format!("{{{row},\"timestamp\":\"{at}\"}}\n")
            })
            .collect();
        std::fs::write(path, lines.concat()).unwrap();
    }

    /// A TUI transcript of `kind` bound to a fresh tmux session.
    fn bound_as(
        kind: RuntimeHandoffKind,
        dir: &std::path::Path,
        now: DateTime<Utc>,
        rows: &[(i64, &str)],
    ) -> (String, impl Drop + use<>) {
        let provider = match kind {
            CodexTui => ProviderKind::Codex,
            _ => ProviderKind::Claude,
        };
        let tmux = provider.build_tmux_session_name(&format!("i8196-{}", uuid::Uuid::new_v4()));
        let path = dir.join(format!("{tmux}.jsonl"));
        write_rows(&path, now, rows);
        let bind = crate::services::tui_o::cutover::test_override::bind_tui_session;
        let guard = bind(&tmux, kind, &path.display().to_string());
        (tmux, guard)
    }

    const PROMPT: &str = r#""type":"user","message":{"role":"user","content":"go"}"#;
    const TOOL_A: &str = r#""type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"Bash","input":{}}]}"#;
    const RESULT_A: &str = r#""type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"ok"}]}"#;
    const TOOL_B: &str = r#""type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"b","name":"Bash","input":{}}]}"#;
    const TEXT: &str = r#""type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"thinking aloud"}]}"#;

    const SEARCH_CALL: &str =
        r#""type":"response_item","payload":{"type":"tool_search_call","call_id":"s1"}"#;
    const SEARCH_OUTPUT: &str =
        r#""type":"response_item","payload":{"type":"tool_search_output","call_id":"s1"}"#;

    /// Appends raw `text` to the transcript `bound_as` wrote for `tmux`.
    fn append(dir: &std::path::Path, tmux: &str, text: &str) {
        use std::io::Write;
        let path = dir.join(format!("{tmux}.jsonl"));
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    /// `row` as one whole transcript line stamped `secs` from `now`.
    fn stamped(row: &str, now: DateTime<Utc>, secs: i64) -> String {
        let at = (now + chrono::Duration::seconds(secs)).to_rfc3339();
        format!("{{{row},\"timestamp\":\"{at}\"}}\n")
    }

    /// A Claude tool result row whose output is `chars` long.
    fn big_result(id: &str, chars: usize) -> String {
        let output = "x".repeat(chars);
        format!(
            r#""type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{id}","content":"{output}"}}]}}"#
        )
    }

    /// Runs the detector's verdict for an O-owned channel's 20-minute-old turn with no heartbeat.
    fn verdict(channel: u64, tmux: &str, now: DateTime<Utc>) -> IdleClassification {
        let started = Some(now - chrono::Duration::minutes(20));
        let progress = observe_turn_progress(channel, tmux, now);
        classify_with_progress(true, None, started, progress, now, IDLE_THRESHOLD)
    }

    /// Serializes env and binding state, isolates the runtime root and makes the channels O-owned.
    fn o_channel_fixture() -> (
        impl Drop,
        impl Drop,
        impl Drop,
        impl Drop,
        tempfile::TempDir,
    ) {
        let env = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let boot = crate::services::tui_o::cutover::test_override::force_channels(&[
            (CHANNEL, ClaudeTui),
            (CODEX_CHANNEL, CodexTui),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            dir.path(),
        );
        (env, dedupe, boot, root, dir)
    }

    #[test]
    fn tui_o_turn_running_tools_past_threshold_is_not_reported_unresponsive() {
        let (_env, _dedupe, _boot, _root, dir) = o_channel_fixture();
        let now = Utc::now();
        let busy = [(20, PROMPT), (19, TOOL_A), (10, RESULT_A), (2, TOOL_B)];
        let (tmux, _a) = bound_as(ClaudeTui, dir.path(), now, &busy);
        assert_eq!(verdict(CHANNEL, &tmux, now), ActiveAndFresh);
        // One tool call quiet past the threshold is named as a running tool, not "응답 없음".
        let (long_tool, _b) = bound_as(ClaudeTui, dir.path(), now, &[(20, PROMPT), (16, TOOL_A)]);
        assert_eq!(verdict(CHANNEL, &long_tool, now), ToolRunning);
    }

    #[test]
    fn tui_o_turn_without_progress_past_threshold_is_still_reported() {
        let (_env, _dedupe, _boot, _root, dir) = o_channel_fixture();
        let now = Utc::now();
        let quiet = [(20, PROMPT), (17, TOOL_A), (16, RESULT_A), (16, TEXT)];
        let (tmux, _a) = bound_as(ClaudeTui, dir.path(), now, &quiet);
        assert_eq!(verdict(CHANNEL, &tmux, now), Idle);
        // An unbound session gives no progress evidence and keeps the alarm.
        assert_eq!(
            verdict(CHANNEL, "AgentDesk-claude-i8196-unbound", now),
            Idle
        );
        // A channel O does not own keeps the heartbeat-only verdict even with fresh records.
        let (fresh, _b) = bound_as(ClaudeTui, dir.path(), now, &[(20, PROMPT), (1, TEXT)]);
        assert_eq!(verdict(CHANNEL + 1, &fresh, now), Idle);
    }

    #[test]
    fn tui_o_tool_turn_behind_an_oversized_result_is_read_past_the_tail_window() {
        let (_env, _dedupe, _boot, _root, dir) = o_channel_fixture();
        let now = Utc::now();
        let result_b = big_result("b", 100_000);
        let mut rows = vec![
            (20, PROMPT),
            (19, TOOL_A),
            (19, TOOL_B),
            (1, result_b.as_str()),
        ];
        let (fresh, _a) = bound_as(ClaudeTui, dir.path(), now, &rows);
        assert_eq!(verdict(CHANNEL, &fresh, now), ActiveAndFresh);
        rows[3].0 = 16;
        let (running, _b) = bound_as(ClaudeTui, dir.path(), now, &rows);
        assert_eq!(verdict(CHANNEL, &running, now), ToolRunning);
        rows.push((16, RESULT_A));
        let (done, _c) = bound_as(ClaudeTui, dir.path(), now, &rows);
        assert_eq!(verdict(CHANNEL, &done, now), Idle);
        // No turn boundary within the widest tail: the alarm stays, naming the tool state unknown.
        let flood: Vec<String> = (0..17)
            .map(|n| big_result(&format!("x{n}"), 1 << 20))
            .collect();
        let mut rows = vec![(20, PROMPT), (19, TOOL_A)];
        rows.extend(flood.iter().map(|row| (16, row.as_str())));
        let (unknown, _d) = bound_as(ClaudeTui, dir.path(), now, &rows);
        assert_eq!(verdict(CHANNEL, &unknown, now), ToolStateUnknown);
    }

    #[test]
    fn tui_o_codex_tool_search_counts_as_progress_and_as_an_open_call() {
        let (_env, _dedupe, _boot, _root, dir) = o_channel_fixture();
        let now = Utc::now();
        let (call, output) = (SEARCH_CALL, SEARCH_OUTPUT);
        let codex = |rows: &[(i64, &str)]| bound_as(CodexTui, dir.path(), now, rows);
        let (fresh, _a) = codex(&[(1, call)]);
        assert_eq!(verdict(CODEX_CHANNEL, &fresh, now), ActiveAndFresh);
        let (running, _b) = codex(&[(16, call)]);
        assert_eq!(verdict(CODEX_CHANNEL, &running, now), ToolRunning);
        let (done, _c) = codex(&[(17, call), (16, output)]);
        assert_eq!(verdict(CODEX_CHANNEL, &done, now), Idle);
    }

    #[test]
    fn tui_o_torn_last_record_reads_as_unknown_until_it_completes() {
        let (_env, _dedupe, _boot, _root, dir) = o_channel_fixture();
        let now = Utc::now();
        let result_b = big_result("b", 2);
        let call = r#""type":"response_item","payload":{"type":"function_call","call_id":"c2"}"#;
        let out =
            r#""type":"response_item","payload":{"type":"function_call_output","call_id":"c2"}"#;
        let claude = [(20, PROMPT), (17, TOOL_A), (16, RESULT_A)];
        let codex = [(17, SEARCH_CALL), (16, SEARCH_OUTPUT)];
        let cases = [
            (ClaudeTui, CHANNEL, &claude[..], TOOL_B, result_b.as_str()),
            (CodexTui, CODEX_CHANNEL, &codex[..], call, out),
        ];
        for (kind, channel, rows, call, result) in cases {
            let (tmux, _g) = bound_as(kind, dir.path(), now, &rows);
            let line = stamped(call, now, -16 * 60);
            let (head, tail) = line.split_at(line.len() / 2);
            append(dir.path(), &tmux, head);
            assert_eq!(verdict(channel, &tmux, now), ToolStateUnknown);
            // Once whole it reads again; blank, whitespace-only and CRLF endings are no torn tail.
            append(dir.path(), &tmux, &format!("{tail}\n  \n"));
            assert_eq!(verdict(channel, &tmux, now), ToolRunning);
            let crlf = stamped(result, now, -16 * 60).replace('\n', "\r\n");
            append(dir.path(), &tmux, &crlf);
            assert_eq!(verdict(channel, &tmux, now), Idle);
        }
    }

    #[test]
    fn tui_o_record_past_the_clock_skew_still_lets_an_open_tool_show() {
        let (_env, _dedupe, _boot, _root, dir) = o_channel_fixture();
        let now = Utc::now();
        for (secs, expected) in [(60, ActiveAndFresh), (61, ToolRunning)] {
            let (tmux, _g) = bound_as(ClaudeTui, dir.path(), now, &[(20, PROMPT), (16, TOOL_A)]);
            append(dir.path(), &tmux, &stamped(TEXT, now, secs));
            assert_eq!(verdict(CHANNEL, &tmux, now), expected);
        }
    }

    /// Runs `name` again in a child process whose every thread sees `CHANNEL` as O-owned, since the
    /// detector reads ownership on a blocking-pool thread; true inside that child.
    fn in_o_channel_process(name: &str) -> bool {
        let env = crate::services::tui_o::cutover::test_override::CHANNELS_ENV;
        if std::env::var_os(env).is_some() {
            return true;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(env, format!("[[{CHANNEL},\"claude_tui\"]]"))
            .output()
            .unwrap();
        let (stdout, stderr) = (String::from_utf8_lossy(&output.stdout), &output.stderr);
        let passed = output.status.success() && stdout.contains("1 passed; 0 failed");
        assert!(passed, "{stdout}\n{}", String::from_utf8_lossy(stderr));
        false
    }

    #[test]
    fn run_pass_reads_tui_o_progress_of_the_open_turn() {
        const NAME: &str = "services::discord::idle_detector::tests::run_pass_reads_tui_o_progress_of_the_open_turn";
        if !in_o_channel_process(NAME) {
            return;
        }
        let (_env, _dedupe, _boot, _root, dir) = o_channel_fixture();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let channel = ChannelId::new(CHANNEL);
            let name = format!("i8196-pass-{}", uuid::Uuid::new_v4());
            let session = super::super::DiscordSession {
                session_id: None,
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                channel_id: Some(CHANNEL),
                channel_name: Some(name.clone()),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: shared.restart.current_generation,
            };
            shared.core.lock().await.sessions.insert(channel, session);
            let token = Arc::new(crate::services::provider::CancelToken::new());
            use poise::serenity_prelude::{MessageId, UserId};
            let (user, message) = (UserId::new(1), MessageId::new(11));
            let start =
                super::super::mailbox_try_start_turn(&shared, channel, token, user, message);
            assert!(start.await);
            let mailbox = shared.mailbox_peek(channel).unwrap();
            mailbox
                .age_active_turn_for_test(Duration::from_secs(20 * 60))
                .await;
            let tmux = ProviderKind::Claude.build_tmux_session_name(&name);
            let path = dir.path().join("pass.jsonl");
            let now = Utc::now();
            write_rows(&path, now, &[(20, PROMPT), (2, TOOL_A)]);
            let bind = crate::services::tui_o::cutover::test_override::bind_claude_tui_session;
            let _bound = bind(&tmux, &path.display().to_string());
            let entry = || async {
                let entries = global_monitoring_store().lock().await.list(CHANNEL);
                entries
                    .into_iter()
                    .find(|e| e.key == MONITORING_KEY)
                    .map(|e| e.description)
            };
            run_pass(&shared, &ProviderKind::Claude).await;
            assert_eq!(entry().await, None);
            write_rows(&path, now, &[(20, PROMPT), (16, TOOL_A)]);
            run_pass(&shared, &ProviderKind::Claude).await;
            assert_eq!(entry().await.as_deref(), Some(TOOL_RUNNING_DESCRIPTION));
        });
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
