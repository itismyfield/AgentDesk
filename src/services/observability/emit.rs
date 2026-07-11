//! #2049: `emit_*` family + invariant violation recorder split out of
//! `mod.rs`. Each function still talks to the same global runtime via
//! `super::runtime()`. Public re-exports preserve `crate::services::
//! observability::emit_turn_started` import paths.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::events;
use super::helpers::normalize_string;
use super::metrics;
use super::turn_lifecycle;
use super::{
    AgentQualityEvent, CounterBucket, CounterDelta, CounterKey, InvariantViolation, QueuedEvent,
    QueuedQualityEvent, WorkerMessage, runtime,
};

pub fn emit_turn_started(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
) {
    // #1070: lightweight atomic counter for turn_bridge attempt entry.
    metrics::record_attempt(channel_id, provider);
    emit_event(
        "turn_started",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        turn_id,
        Some("started"),
        CounterDelta {
            turn_attempts: 1,
            ..CounterDelta::default()
        },
        json!({}),
    );
}

pub fn emit_turn_finished_with_dispatch_kind(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    outcome: &str,
    duration_ms: i64,
    tmux_handoff: bool,
    dispatch_kind: Option<&str>,
) {
    let normalized_outcome = normalize_string(outcome);
    let dispatch_kind = dispatch_kind.and_then(normalize_string);
    let is_success = matches!(
        normalized_outcome.as_deref(),
        Some("completed") | Some("tmux_handoff")
    );
    // #1070: atomic success/fail counters for dispatch outcome.
    if is_success {
        metrics::record_success(channel_id, provider);
    } else {
        metrics::record_fail(channel_id, provider);
    }
    emit_event(
        "turn_finished",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        turn_id,
        normalized_outcome.as_deref(),
        CounterDelta {
            turn_successes: u64::from(is_success),
            turn_failures: u64::from(!is_success),
            ..CounterDelta::default()
        },
        json!({
            "duration_ms": duration_ms.max(0),
            "tmux_handoff": tmux_handoff,
            "dispatch_kind": dispatch_kind.as_deref(),
        }),
    );
}

pub fn emit_guard_fired(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    guard_type: &str,
) {
    // #1070: atomic guard-fire counter.
    metrics::record_guard_fire(channel_id, provider);
    emit_event(
        "guard_fired",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        turn_id,
        normalize_string(guard_type).as_deref(),
        CounterDelta {
            guard_fires: 1,
            ..CounterDelta::default()
        },
        json!({
            "guard_type": normalize_string(guard_type),
        }),
    );
}

pub fn emit_watcher_replaced(provider: &str, channel_id: u64, source: &str) {
    // #1070: atomic watcher-replacement counter for claim_or_replace stale cancel.
    metrics::record_watcher_replacement(channel_id, provider);
    emit_event(
        "watcher_replaced",
        Some(provider),
        Some(channel_id),
        None,
        None,
        None,
        Some("replaced"),
        CounterDelta {
            watcher_replacements: 1,
            ..CounterDelta::default()
        },
        json!({
            "source": normalize_string(source),
        }),
    );
}

pub fn emit_recovery_fired(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    agent_id: Option<&str>,
    reason: &str,
) {
    // #3562: thread the turn identity and agent role through so consumers can
    // back-trace *which* agent/turn triggered the recovery. `turn_id` rides the
    // dedicated correlation column (enrich_payload_with_correlation also mirrors
    // it into payload_json for jsonl/dashboard parsers). `observability_events`
    // has no `agent_id` column, so `agent_id` lives only in payload_json — a
    // backward-compatible add (older rows simply omit the key).
    emit_event(
        "recovery_fired",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        turn_id,
        normalize_string(reason).as_deref(),
        CounterDelta {
            recovery_fires: 1,
            ..CounterDelta::default()
        },
        json!({
            "reason": normalize_string(reason),
            "agent_id": agent_id.and_then(normalize_string),
        }),
    );
}

/// Records a cancellation attempt/result from shared lifecycle paths. Callers
/// should pass correlation IDs when they own them; lower-level stop helpers may
/// only know provider/channel and still emit a channel-scoped event.
pub fn emit_turn_cancelled(
    provider: Option<&str>,
    channel_id: Option<u64>,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    details: turn_lifecycle::TurnCancellationDetails,
) {
    let mut payload = serde_json::to_value(&details).unwrap_or_else(|_| json!({}));
    if let Value::Object(fields) = &mut payload {
        fields.insert("dispatch_id".to_string(), json!(dispatch_id));
        fields.insert("session_key".to_string(), json!(session_key));
        fields.insert("turn_id".to_string(), json!(turn_id));
    }
    emit_event(
        "turn_cancelled",
        provider,
        channel_id,
        dispatch_id,
        session_key,
        turn_id,
        Some("cancelled"),
        CounterDelta::default(),
        payload,
    );
}

/// Inflight lifecycle observability: pair-tracking events so external monitors
/// can detect cleanup leaks. `kind` is the lifecycle phase identifier:
/// `"delegated_to_watcher"`, `"delegated_to_standby_relay"`,
/// `"cleared_by_bridge"`, `"cleared_by_watcher"`, `"cleared_by_standby_relay"`,
/// `"leak_detected_completed_stale"`. Delegated/cleared events should pair
/// 1:1 per owner; a sustained drift between counters indicates the bridge
/// handed off cleanup but the delegated relay never executed it.
/// `leak_detected_completed_stale` fires from the stall-watchdog when an
/// inflight is healthy/synced but the mailbox is idle past the staleness
/// threshold — the smoking-gun signal for the deadlock-manager alarm pattern.
pub fn emit_inflight_lifecycle_event(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    kind: &str,
    extra: Value,
) {
    emit_event(
        "inflight_lifecycle",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        turn_id,
        normalize_string(kind).as_deref(),
        CounterDelta::default(),
        json!({
            "kind": normalize_string(kind),
            "extra": extra,
        }),
    );
}

/// Persist one relay root-cause counter increment. The hot-path atomic counter
/// remains in `metrics`; this event gives operators a restart-safe stream they
/// can aggregate across deploys (#2878).
pub(super) fn emit_relay_root_cause_counter(provider: &str, channel_id: u64, counter: &str) {
    let Some(counter) = normalize_string(counter) else {
        return;
    };
    emit_event(
        "relay_root_cause_counter",
        Some(provider),
        Some(channel_id),
        None,
        None,
        None,
        Some(counter.as_str()),
        CounterDelta::default(),
        json!({
            "counter": counter,
            "delta": 1,
        }),
    );
}

pub fn record_invariant_check(condition: bool, violation: InvariantViolation<'_>) -> bool {
    record_invariant_check_with_severity(condition, violation, InvariantSeverity::Error)
}

/// #3552: severity for a recorded invariant violation. `Error` is the default
/// (ERROR-level tracing log) used by every breach that actually persists bad
/// state. `Warn` is used ONLY when a downstream guard has *already handled* the
/// condition (e.g. the #3416 enforce path skips the backward inflight write and
/// preserves the offset → zero data loss), so an ERROR is inappropriate noise.
/// The structured `invariant_violation` analytics event (incl. `guard_fires`)
/// is emitted identically in both cases — only the tracing log level changes —
/// so dashboards/PG analytics keep full visibility while the operator-facing
/// ERROR log stays clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvariantSeverity {
    Error,
    Warn,
}

const INVARIANT_WARN_DOWNGRADE_SUFFIX: &str = "(handled by downstream guard — downgraded to WARN)";

// Keep the tracing call and its test witness behind one token. A regression
// that routes `Warn` through the `error` arm (or removes the arm entirely)
// therefore changes both the real emission and the deterministic witness.
macro_rules! emit_invariant_log {
    (error, $invariant:expr, $violation:expr) => {{
        tracing::error!(
            invariant = %$invariant,
            provider = $violation.provider.unwrap_or_default(),
            channel_id = $violation.channel_id.unwrap_or_default(),
            dispatch_id = $violation.dispatch_id.unwrap_or_default(),
            session_key = $violation.session_key.unwrap_or_default(),
            turn_id = $violation.turn_id.unwrap_or_default(),
            code_location = $violation.code_location,
            "[invariant] {}",
            $violation.message
        );
        #[cfg(test)]
        invariant_log_test_capture::record(
            InvariantSeverity::Error,
            &$invariant,
            format!("[invariant] {}", $violation.message),
        );
    }};
    (warn, $invariant:expr, $violation:expr) => {{
        tracing::warn!(
            invariant = %$invariant,
            provider = $violation.provider.unwrap_or_default(),
            channel_id = $violation.channel_id.unwrap_or_default(),
            dispatch_id = $violation.dispatch_id.unwrap_or_default(),
            session_key = $violation.session_key.unwrap_or_default(),
            turn_id = $violation.turn_id.unwrap_or_default(),
            code_location = $violation.code_location,
            "[invariant] {} {}",
            $violation.message,
            INVARIANT_WARN_DOWNGRADE_SUFFIX
        );
        #[cfg(test)]
        invariant_log_test_capture::record(
            InvariantSeverity::Warn,
            &$invariant,
            format!(
                "[invariant] {} {}",
                $violation.message, INVARIANT_WARN_DOWNGRADE_SUFFIX
            ),
        );
    }};
}

#[cfg(test)]
mod invariant_log_test_capture {
    use std::cell::RefCell;

    use super::InvariantSeverity;

    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct CapturedInvariantLog {
        pub(super) severity: InvariantSeverity,
        pub(super) invariant: String,
        pub(super) rendered_message: String,
    }

    thread_local! {
        static CAPTURE: RefCell<Option<Vec<CapturedInvariantLog>>> = const { RefCell::new(None) };
    }

    struct CaptureGuard {
        active: bool,
    }

    impl CaptureGuard {
        fn begin() -> Self {
            CAPTURE.with(|slot| {
                let previous = slot.borrow_mut().replace(Vec::new());
                assert!(previous.is_none(), "nested invariant log test capture");
            });
            Self { active: true }
        }

        fn finish(mut self) -> Vec<CapturedInvariantLog> {
            self.active = false;
            CAPTURE.with(|slot| slot.borrow_mut().take().unwrap_or_default())
        }
    }

    impl Drop for CaptureGuard {
        fn drop(&mut self) {
            if self.active {
                CAPTURE.with(|slot| {
                    slot.borrow_mut().take();
                });
            }
        }
    }

    pub(super) fn record(severity: InvariantSeverity, invariant: &str, rendered_message: String) {
        CAPTURE.with(|slot| {
            if let Some(logs) = slot.borrow_mut().as_mut() {
                logs.push(CapturedInvariantLog {
                    severity,
                    invariant: invariant.to_string(),
                    rendered_message,
                });
            }
        });
    }

    pub(super) fn capture<T>(run: impl FnOnce() -> T) -> (T, Vec<CapturedInvariantLog>) {
        let guard = CaptureGuard::begin();
        let result = run();
        (result, guard.finish())
    }
}

pub fn record_invariant_check_with_severity(
    condition: bool,
    violation: InvariantViolation<'_>,
    severity: InvariantSeverity,
) -> bool {
    if condition {
        return true;
    }

    let invariant = normalize_string(violation.invariant).unwrap_or_else(|| "unknown".to_string());
    match severity {
        InvariantSeverity::Error => emit_invariant_log!(error, invariant, violation),
        InvariantSeverity::Warn => emit_invariant_log!(warn, invariant, violation),
    }

    emit_event(
        "invariant_violation",
        violation.provider,
        violation.channel_id,
        violation.dispatch_id,
        violation.session_key,
        violation.turn_id,
        Some(invariant.as_str()),
        CounterDelta {
            guard_fires: 1,
            ..CounterDelta::default()
        },
        json!({
            "invariant": invariant,
            "code_location": violation.code_location,
            "message": violation.message,
            "details": violation.details,
        }),
    );
    false
}

pub fn emit_dispatch_result(
    dispatch_id: &str,
    kanban_card_id: Option<&str>,
    dispatch_type: Option<&str>,
    from_status: Option<&str>,
    to_status: &str,
    transition_source: &str,
    payload: Option<&Value>,
) {
    emit_event(
        "dispatch_result",
        None,
        None,
        Some(dispatch_id),
        None,
        None,
        Some(to_status),
        CounterDelta::default(),
        json!({
            "kanban_card_id": normalize_string(kanban_card_id.unwrap_or_default()),
            "dispatch_type": normalize_string(dispatch_type.unwrap_or_default()),
            "from_status": normalize_string(from_status.unwrap_or_default()),
            "to_status": normalize_string(to_status),
            "transition_source": normalize_string(transition_source),
            "payload": payload.cloned().unwrap_or_else(|| json!({})),
        }),
    );
}

/// #1984 (codex C — observation): record a structured event whenever a
/// Discord placeholder POST fails inside one of the intake/queue/race code
/// paths. Each call site sets a stable `phase` label so the resulting PG
/// rows can be aggregated into a daily failure count, and a `recovery`
/// label so we can confirm whether the user message stayed in queue, the
/// mailbox slot was released, or it dropped on the floor.
///
/// This is the measurement instrumentation the retro asked for before
/// considering whether to re-apply the rolled-back 7f8184b9 retry guard.
/// The event payload is intentionally minimal — we want raw counts, not a
/// full incident timeline.
pub fn emit_intake_placeholder_post_failed(
    provider: &str,
    channel_id: u64,
    user_msg_id: Option<u64>,
    phase: &str,
    recovery: &str,
    error: &str,
) {
    emit_event(
        "intake_placeholder_post_failed",
        Some(provider),
        Some(channel_id),
        None,
        None,
        None,
        normalize_string(phase).as_deref(),
        CounterDelta::default(),
        json!({
            "phase": normalize_string(phase),
            "recovery": normalize_string(recovery),
            "user_msg_id": user_msg_id,
            "error": normalize_string(error),
        }),
    );
}

/// #3813 Phase 1a: record the intake-side latency spans (observation-only) so
/// the `claim → placeholder → prep → input` durations are PG-queryable and the
/// downstream fast-lane tuning (Phase 2) can be made data-backed. `outcome`
/// is `submitted` (input handed to the turn bridge) or `deferred_busy` (a
/// pre-submission defer), and each `*_ms` is `None` when its milestone was not
/// reached — a partial span, never a bogus zero. Emitted at most once per turn
/// on the intake path (same non-blocking family as `emit_intake_placeholder_post_failed`).
#[allow(clippy::too_many_arguments)]
pub fn emit_intake_latency_spans(
    provider: &str,
    channel_id: u64,
    outcome: &str,
    accept_to_placeholder_ms: Option<u64>,
    placeholder_to_prep_ms: Option<u64>,
    prep_to_input_ms: Option<u64>,
    accept_to_input_ms: Option<u64>,
) {
    emit_event(
        "intake_latency_spans",
        Some(provider),
        Some(channel_id),
        None,
        None,
        None,
        normalize_string(outcome).as_deref(),
        CounterDelta::default(),
        json!({
            "outcome": normalize_string(outcome),
            "accept_to_placeholder_ms": accept_to_placeholder_ms,
            "placeholder_to_prep_ms": placeholder_to_prep_ms,
            "prep_to_input_ms": prep_to_input_ms,
            "accept_to_input_ms": accept_to_input_ms,
        }),
    );
}

/// #3813 AC#1 tail: record the bridge-side latency spans (observation-only) so
/// the trailing half of acceptance criterion #1 — first provider output observed
/// and first Discord relay delivered — is PG-queryable alongside the intake
/// spans (`emit_intake_latency_spans`). Both `*_ms` are measured from the
/// bridge-entry `turn_start` anchor and are `None` when their waypoint was not
/// reached on the bridge-owned relay path (a partial span, never a bogus zero).
/// Emitted at most once per turn on the bridge streaming loop's exit; the caller
/// suppresses the emit when neither waypoint was reached (e.g. a watcher-owned
/// relay), so this is never an all-`None` row.
pub fn emit_bridge_latency_spans(
    provider: &str,
    channel_id: u64,
    turn_start_to_first_output_ms: Option<u64>,
    turn_start_to_first_relay_ms: Option<u64>,
) {
    emit_event(
        "bridge_latency_spans",
        Some(provider),
        Some(channel_id),
        None,
        None,
        None,
        None,
        CounterDelta::default(),
        json!({
            "turn_start_to_first_output_ms": turn_start_to_first_output_ms,
            "turn_start_to_first_relay_ms": turn_start_to_first_relay_ms,
        }),
    );
}

/// #2838 (relay-stability P0-1): record a structured event at each terminal
/// relay delivery decision so the duplicate-emit (root cause #1) and
/// missing-answer (root cause #4) vectors are PG-queryable and attributable to
/// a specific owner. `owner` is one of `turn_bridge` | `watcher_direct` |
/// `session_relay_sink` | `standby` | `recovery`; `op` is `edit` | `post` |
/// `skip`; `committed` reflects whether the answer actually reached Discord.
/// The bridge-side delivery decision is NOT covered by the watcher-side
/// `relay_flight_recorder` tracing, so this closes that observability gap
/// before the delivery-lease consolidation touches the hot path.
pub fn emit_relay_delivery(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    msg_id: Option<u64>,
    owner: &str,
    op: &str,
    byte_range_start: Option<u64>,
    byte_range_end: Option<u64>,
    committed: bool,
    detail: Option<&str>,
) {
    emit_event(
        "relay_delivery",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        turn_id,
        Some(if committed {
            "committed"
        } else {
            "uncommitted"
        }),
        CounterDelta::default(),
        json!({
            "owner": normalize_string(owner),
            "op": normalize_string(op),
            "msg_id": msg_id,
            "byte_range_start": byte_range_start,
            "byte_range_end": byte_range_end,
            "committed": committed,
            "detail": detail.and_then(normalize_string),
        }),
    );
}

/// #3607 durable delete observability: record a structured event at each wired
/// relay-message *delete* decision so terminal-anchor protections and orphan /
/// panel / replay-prefix cleanups are PG-queryable and attributable. Mirrors
/// [`emit_relay_delivery`] (which covers the answer-delivery side) for the
/// destructive side: `source` is the call site (`turn_bridge_watcher_orphan_…`,
/// `full_terminal_replay_prefix`, `placeholder_sweeper`, …), `operation_kind`
/// reuses the `PlaceholderCleanupOperation` vocab (`delete_terminal` /
/// `delete_nonterminal` / `edit_terminal` / `edit_preserve`) or a site-specific
/// descriptive verb, and `outcome` is one of `committed` | `already_gone` |
/// `failed` | `skipped_committed_terminal`. The outcome doubles as the event
/// `status` (correlation column) so a query can split committed deletes from
/// guard-skips without parsing the payload.
#[allow(clippy::too_many_arguments)]
pub fn emit_relay_delete(
    provider: &str,
    channel_id: u64,
    message_id: u64,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    source: &str,
    operation_kind: &str,
    outcome: &str,
    detail: Option<&str>,
) {
    emit_event(
        "relay_delete",
        Some(provider),
        Some(channel_id),
        None,
        session_key,
        turn_id,
        normalize_string(outcome).as_deref(),
        CounterDelta::default(),
        json!({
            "message_id": message_id,
            "source": normalize_string(source),
            "operation_kind": normalize_string(operation_kind),
            "outcome": normalize_string(outcome),
            "detail": detail.and_then(normalize_string),
        }),
    );
}

/// #3607 ergonomic wrapper over [`emit_relay_delete`] for the common
/// delete-then-observe call sites: maps a delete `Result` to the
/// `committed` / `failed` outcome (and the `Err` text into `detail`) so each
/// site stays a single compact call instead of an inline match. `provider` may
/// be empty for sites with no provider scope (idle-recap / monitoring); it is
/// normalized away to none.
pub fn emit_relay_delete_result<E: std::fmt::Display>(
    provider: &str,
    channel_id: u64,
    message_id: u64,
    source: &str,
    operation_kind: &str,
    result: &Result<(), E>,
) {
    let detail = result.as_ref().err().map(|error| error.to_string());
    emit_relay_delete(
        provider,
        channel_id,
        message_id,
        None,
        None,
        source,
        operation_kind,
        if result.is_ok() {
            "committed"
        } else {
            "failed"
        },
        detail.as_deref(),
    );
}

pub fn emit_agent_quality_event(event: AgentQualityEvent) {
    let Some(event_type) = super::helpers::normalize_quality_event_type(&event.event_type) else {
        tracing::warn!(
            event_type = %event.event_type,
            "[quality] dropping unknown agent quality event type"
        );
        return;
    };

    let queued = QueuedQualityEvent {
        source_event_id: event.source_event_id.as_deref().and_then(normalize_string),
        correlation_id: event.correlation_id.as_deref().and_then(normalize_string),
        agent_id: event.agent_id.as_deref().and_then(normalize_string),
        provider: event.provider.as_deref().and_then(normalize_string),
        channel_id: event.channel_id.as_deref().and_then(normalize_string),
        card_id: event.card_id.as_deref().and_then(normalize_string),
        dispatch_id: event.dispatch_id.as_deref().and_then(normalize_string),
        event_type,
        payload_json: serde_json::to_string(&event.payload).unwrap_or_else(|_| "{}".to_string()),
    };

    let channel_id = queued
        .channel_id
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok());
    events::record_emitted(
        "agent_quality_event",
        channel_id,
        queued.provider.as_deref(),
        enrich_payload_with_correlation(
            json!({
                "source_event_id": queued.source_event_id.clone(),
                "correlation_id": queued.correlation_id.clone(),
                "agent_id": queued.agent_id.clone(),
                "provider": queued.provider.clone(),
                "channel_id": queued.channel_id.clone(),
                "card_id": queued.card_id.clone(),
                "dispatch_id": queued.dispatch_id.clone(),
                "quality_event_type": queued.event_type.clone(),
                "payload": event.payload,
            }),
            queued.dispatch_id.as_deref(),
            None,
            None,
            Some(queued.event_type.as_str()),
        ),
    );

    if let Some(sender) = super::worker::worker_sender() {
        let _ = sender.send(WorkerMessage::QualityEvent(queued));
    }
}

pub(super) fn emit_event(
    event_type: &str,
    provider: Option<&str>,
    channel_id: Option<u64>,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    status: Option<&str>,
    counter_delta: CounterDelta,
    payload: Value,
) {
    let Some(event_type) = normalize_string(event_type) else {
        return;
    };

    let provider = provider.and_then(normalize_string);
    let channel_id_string = channel_id.map(|value| value.to_string());
    let dispatch_id = dispatch_id.and_then(normalize_string);
    let session_key = session_key.and_then(normalize_string);
    let turn_id = turn_id.and_then(normalize_string);
    let status = status.and_then(normalize_string);
    if !counter_delta.is_zero()
        && let (Some(provider), Some(channel_id)) = (provider.as_ref(), channel_id_string.as_ref())
    {
        let bucket = runtime()
            .counters
            .entry(CounterKey {
                provider: provider.clone(),
                channel_id: channel_id.clone(),
            })
            .or_insert_with(|| Arc::new(CounterBucket::default()))
            .clone();
        bucket.apply(counter_delta);
    }

    events::record_emitted(
        event_type.as_str(),
        channel_id,
        provider.as_deref(),
        enrich_payload_with_correlation(
            payload.clone(),
            dispatch_id.as_deref(),
            session_key.as_deref(),
            turn_id.as_deref(),
            status.as_deref(),
        ),
    );

    let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    let queued = QueuedEvent {
        event_type,
        provider,
        channel_id: channel_id_string,
        dispatch_id,
        session_key,
        turn_id,
        status,
        payload_json,
    };

    if let Some(sender) = super::worker::worker_sender() {
        let _ = sender.send(WorkerMessage::Event(queued));
    }
}

fn enrich_payload_with_correlation(
    payload: Value,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    status: Option<&str>,
) -> Value {
    let mut fields = match payload {
        Value::Object(fields) => fields,
        other => {
            let mut fields = Map::new();
            fields.insert("payload".to_string(), other);
            fields
        }
    };

    insert_if_absent(&mut fields, "dispatch_id", dispatch_id);
    insert_if_absent(&mut fields, "session_key", session_key);
    insert_if_absent(&mut fields, "turn_id", turn_id);
    insert_if_absent(&mut fields, "status", status);

    Value::Object(fields)
}

fn insert_if_absent(fields: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if fields.contains_key(key) {
        return;
    }
    if let Some(value) = value {
        fields.insert(key.to_string(), json!(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_emit_surface_records_one_recent_event() {
        let _guard = super::super::test_runtime_lock();
        super::super::reset_for_tests();

        emit_turn_started(
            "Codex",
            42,
            Some("dispatch-started"),
            Some("session-started"),
            Some("turn-started"),
        );
        emit_turn_finished_with_dispatch_kind(
            "Codex",
            42,
            Some("dispatch-finished"),
            Some("session-finished"),
            Some("turn-finished"),
            " completed ",
            123,
            false,
            Some("manual"),
        );
        emit_guard_fired(
            "Codex",
            42,
            Some("dispatch-guard"),
            Some("session-guard"),
            Some("turn-guard"),
            "placeholder_suppress",
        );
        emit_watcher_replaced("Codex", 42, "stale_cancel");
        emit_recovery_fired(
            "Codex",
            42,
            Some("dispatch-recovery"),
            Some("session-recovery"),
            Some("turn-recovery"),
            Some("planner"),
            "watchdog",
        );
        emit_turn_cancelled(
            Some("Codex"),
            Some(42),
            Some("dispatch-cancelled"),
            Some("session-cancelled"),
            Some("turn-cancelled"),
            turn_lifecycle::TurnCancellationDetails::new(
                "operator stop",
                "text_stop",
                "mailbox_canonical",
                true,
                true,
                Some(1),
                false,
                true,
            ),
        );
        emit_inflight_lifecycle_event(
            "Codex",
            42,
            Some("dispatch-inflight"),
            Some("session-inflight"),
            Some("turn-inflight"),
            "delegated_to_watcher",
            json!({"owner": "watcher"}),
        );
        assert!(!record_invariant_check(
            false,
            InvariantViolation {
                provider: Some("Codex"),
                channel_id: Some(42),
                dispatch_id: Some("dispatch-invariant"),
                session_key: Some("session-invariant"),
                turn_id: Some("turn-invariant"),
                invariant: "recent_ring_visibility",
                code_location: "src/services/observability/emit.rs:test",
                message: "test violation",
                details: json!({"test": true}),
            },
        ));
        emit_dispatch_result(
            "dispatch-result",
            Some("card-1"),
            Some("implementation"),
            Some("doing"),
            "done",
            "test",
            Some(&json!({"ok": true})),
        );
        emit_intake_placeholder_post_failed(
            "Codex",
            42,
            Some(1234),
            "queue",
            "message_preserved",
            "discord unavailable",
        );
        emit_agent_quality_event(AgentQualityEvent {
            source_event_id: Some("turn-quality".to_string()),
            correlation_id: Some("dispatch-quality".to_string()),
            agent_id: Some("agent-1".to_string()),
            provider: Some("Codex".to_string()),
            channel_id: Some("42".to_string()),
            card_id: Some("card-1".to_string()),
            dispatch_id: Some("dispatch-quality".to_string()),
            event_type: "review_pass".to_string(),
            payload: json!({"verdict": "pass"}),
        });

        let events = events::recent(50);
        for expected in [
            "turn_started",
            "turn_finished",
            "guard_fired",
            "watcher_replaced",
            "recovery_fired",
            "turn_cancelled",
            "inflight_lifecycle",
            "invariant_violation",
            "dispatch_result",
            "intake_placeholder_post_failed",
            "agent_quality_event",
        ] {
            let count = events
                .iter()
                .filter(|event| event.event_type == expected)
                .count();
            assert_eq!(count, 1, "{expected} should be recorded exactly once");
        }

        let dispatch_result = events
            .iter()
            .find(|event| event.event_type == "dispatch_result")
            .expect("dispatch_result should be in recent ring");
        assert_eq!(dispatch_result.payload["dispatch_id"], "dispatch-result");
        assert_eq!(dispatch_result.payload["status"], "done");

        let turn_finished = events
            .iter()
            .find(|event| event.event_type == "turn_finished")
            .expect("turn_finished should be in recent ring");
        assert_eq!(turn_finished.provider.as_deref(), Some("codex"));
        assert_eq!(turn_finished.channel_id, Some(42));
        assert_eq!(turn_finished.payload["dispatch_id"], "dispatch-finished");
        assert_eq!(turn_finished.payload["duration_ms"], 123);
        assert_eq!(turn_finished.payload["status"], "completed");

        let quality = events
            .iter()
            .find(|event| event.event_type == "agent_quality_event")
            .expect("agent_quality_event should be in recent ring");
        assert_eq!(quality.payload["quality_event_type"], "review_pass");
        assert_eq!(quality.payload["dispatch_id"], "dispatch-quality");
        assert_eq!(quality.payload["source_event_id"], "turn-quality");
        assert_eq!(quality.payload["status"], "review_pass");

        // #3562: recovery_fired must carry the turn identity (correlation column +
        // mirrored into payload_json) and the agent_id (payload_json only) so
        // consumers can back-trace which agent/turn triggered the recovery.
        let recovery = events
            .iter()
            .find(|event| event.event_type == "recovery_fired")
            .expect("recovery_fired should be in recent ring");
        assert_eq!(recovery.payload["turn_id"], "turn-recovery");
        assert_eq!(recovery.payload["agent_id"], "planner");
        assert_eq!(recovery.payload["reason"], "watchdog");
    }

    #[test]
    fn relay_delete_records_required_fields_and_outcome() {
        // #3607: every wired delete site funnels through emit_relay_delete. The
        // event must carry the required relay fields (channel_id / message_id /
        // source / operation_kind / outcome) and surface the outcome as both the
        // payload value and the correlation status so committed deletes and
        // guard-skips are queryable apart.
        let _guard = super::super::test_runtime_lock();
        super::super::reset_for_tests();

        emit_relay_delete(
            "Codex",
            42,
            7777,
            Some("session-delete"),
            Some("turn-delete"),
            "turn_bridge_watcher_orphan_spinner_cleanup",
            "delete_nonterminal",
            "skipped_committed_terminal",
            Some(" guarded "),
        );

        let events = events::recent(50);
        let event = events
            .iter()
            .find(|event| event.event_type == "relay_delete")
            .expect("relay_delete should be in recent ring");
        assert_eq!(event.provider.as_deref(), Some("codex"));
        assert_eq!(event.channel_id, Some(42));
        assert_eq!(event.payload["message_id"], 7777);
        assert_eq!(
            event.payload["source"],
            "turn_bridge_watcher_orphan_spinner_cleanup"
        );
        assert_eq!(event.payload["operation_kind"], "delete_nonterminal");
        assert_eq!(event.payload["outcome"], "skipped_committed_terminal");
        assert_eq!(event.payload["detail"], "guarded");
        // outcome doubles as the correlation status.
        assert_eq!(event.payload["status"], "skipped_committed_terminal");
    }

    #[test]
    fn warn_severity_invariant_still_emits_event_without_error_level() {
        // #3552: a violation downgraded to WARN (because a downstream guard
        // already handled it) must keep full structured-event visibility — the
        // analytics `invariant_violation` event (incl. guard_fires) is identical
        // to the ERROR case; only the tracing log LEVEL changes. The recorder
        // still returns `false` (violation observed), so callers/debug_asserts
        // behave exactly as before.
        let _guard = super::super::test_runtime_lock();
        super::super::reset_for_tests();

        // Holds true regardless of severity → no event recorded.
        assert!(record_invariant_check_with_severity(
            true,
            InvariantViolation {
                provider: Some("Codex"),
                channel_id: Some(7),
                dispatch_id: None,
                session_key: None,
                turn_id: None,
                invariant: "last_offset_monotonic",
                code_location: "src/services/observability/emit.rs:test",
                message: "should not fire",
                details: json!({}),
            },
            InvariantSeverity::Warn,
        ));

        // Violation downgraded to WARN → returns false AND records the event.
        assert!(!record_invariant_check_with_severity(
            false,
            InvariantViolation {
                provider: Some("Codex"),
                channel_id: Some(7),
                dispatch_id: None,
                session_key: None,
                turn_id: None,
                invariant: "last_offset_monotonic",
                code_location: "src/services/observability/emit.rs:test",
                message: "handled-by-guard violation",
                details: json!({"downgraded": true}),
            },
            InvariantSeverity::Warn,
        ));

        // Isolation-robust: assert THIS test's specific WARN-downgraded event is
        // present in the (shared, global) recent-events ring — filtered by a
        // unique marker (`details.downgraded` + the verbatim message) — rather
        // than demanding exactly one `invariant_violation` event ring-wide. Other
        // tests that don't hold `test_runtime_lock()` can concurrently push their
        // own `invariant_violation` events into the same global ring on a parallel
        // (self-hosted) runner, which would otherwise inflate the count past 1.
        // The analytics event is byte-for-byte identical across Error/Warn (only
        // the tracing log LEVEL differs), so presence of this exact event — emitted
        // despite the WARN downgrade — is what proves the test's intent.
        let events = events::recent(50);
        let downgraded_events: Vec<_> = events
            .iter()
            .filter(|event| {
                event.event_type == "invariant_violation"
                    && event.payload["message"] == "handled-by-guard violation"
                    && event.payload["details"]["downgraded"] == json!(true)
            })
            .collect();
        assert_eq!(
            downgraded_events.len(),
            1,
            "WARN-downgraded violation must still emit exactly one analytics event for THIS test's \
             unique marker (details.downgraded + message); other concurrent invariant_violation \
             events in the shared ring are tolerated"
        );
        let downgraded_event = downgraded_events[0];
        assert_eq!(
            downgraded_event.payload["invariant"],
            "last_offset_monotonic"
        );
        // The downgrade only changes the tracing log level; the structured event
        // is emitted identically and carries the same provider/channel as ERROR.
        assert_eq!(downgraded_event.provider.as_deref(), Some("codex"));
        assert_eq!(downgraded_event.channel_id, Some(7));
    }

    #[test]
    fn invariant_severity_routes_to_exact_log_level_and_suffix_4422() {
        let ((), logs) = super::invariant_log_test_capture::capture(|| {
            assert!(record_invariant_check_with_severity(
                true,
                InvariantViolation {
                    provider: Some("Codex"),
                    channel_id: Some(44_220),
                    dispatch_id: None,
                    session_key: None,
                    turn_id: None,
                    invariant: "condition_true_must_not_emit",
                    code_location: "src/services/observability/emit.rs:test",
                    message: "no emission",
                    details: json!({}),
                },
                InvariantSeverity::Warn,
            ));
            assert!(!record_invariant_check_with_severity(
                false,
                InvariantViolation {
                    provider: Some("Codex"),
                    channel_id: Some(44_220),
                    dispatch_id: None,
                    session_key: None,
                    turn_id: None,
                    invariant: "warn_route_4422",
                    code_location: "src/services/observability/emit.rs:test",
                    message: "handled rewind",
                    details: json!({}),
                },
                InvariantSeverity::Warn,
            ));
            assert!(!record_invariant_check_with_severity(
                false,
                InvariantViolation {
                    provider: Some("Codex"),
                    channel_id: Some(44_220),
                    dispatch_id: None,
                    session_key: None,
                    turn_id: None,
                    invariant: "error_route_4422",
                    code_location: "src/services/observability/emit.rs:test",
                    message: "persisted rewind",
                    details: json!({}),
                },
                InvariantSeverity::Error,
            ));
        });

        assert_eq!(logs.len(), 2, "condition=true must not emit: {logs:?}");
        assert_eq!(logs[0].severity, InvariantSeverity::Warn);
        assert_eq!(logs[0].invariant, "warn_route_4422");
        assert_eq!(
            logs[0].rendered_message,
            format!("[invariant] handled rewind {INVARIANT_WARN_DOWNGRADE_SUFFIX}")
        );
        assert_eq!(logs[1].severity, InvariantSeverity::Error);
        assert_eq!(logs[1].invariant, "error_route_4422");
        assert_eq!(logs[1].rendered_message, "[invariant] persisted rewind");
    }

    #[test]
    fn relay_root_cause_metric_wrappers_record_persistent_events() {
        let _guard = super::super::test_runtime_lock();
        super::super::reset_for_tests();

        metrics::record_relay_terminal_ack_timeout(77, "Codex");
        metrics::record_relay_uncommitted_inflight_cleared(77, "Codex");
        metrics::record_relay_owner_unknown(77, "Codex");

        let events = events::recent(10);
        let counters = events
            .iter()
            .filter(|event| event.event_type == "relay_root_cause_counter")
            .map(|event| event.payload["counter"].as_str().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(
            counters,
            vec![
                "relay_terminal_ack_timeout",
                "relay_uncommitted_inflight_cleared",
                "relay_owner_unknown"
            ]
        );
        assert!(
            events
                .iter()
                .all(|event| event.provider.as_deref() == Some("codex"))
        );
        assert!(events.iter().all(|event| event.channel_id == Some(77)));
    }
}
