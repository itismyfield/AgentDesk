//! #2049: `emit_*` family + invariant violation recorder split out of
//! `mod.rs`. Each function still talks to the same global runtime via
//! `super::runtime()`. Public re-exports preserve `crate::services::
//! observability::emit_turn_started` import paths.

use std::sync::Arc;

use serde_json::{Value, json};

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
    events::record(events::StructuredEvent::new(
        "turn_started",
        Some(channel_id),
        Some(provider),
        json!({
            "dispatch_id": dispatch_id,
            "session_key": session_key,
            "turn_id": turn_id,
        }),
    ));
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

pub fn emit_turn_finished(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    outcome: &str,
    duration_ms: i64,
    tmux_handoff: bool,
) {
    emit_turn_finished_with_dispatch_kind(
        provider,
        channel_id,
        dispatch_id,
        session_key,
        turn_id,
        outcome,
        duration_ms,
        tmux_handoff,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
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
    events::record(events::StructuredEvent::new(
        "turn_finished",
        Some(channel_id),
        Some(provider),
        json!({
            "dispatch_id": dispatch_id,
            "session_key": session_key,
            "turn_id": turn_id,
            "outcome": normalized_outcome,
            "duration_ms": duration_ms.max(0),
            "tmux_handoff": tmux_handoff,
            "dispatch_kind": dispatch_kind.as_deref(),
        }),
    ));
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
    events::record(events::StructuredEvent::new(
        "guard_fired",
        Some(channel_id),
        Some(provider),
        json!({
            "dispatch_id": dispatch_id,
            "session_key": session_key,
            "turn_id": turn_id,
            "guard_type": normalize_string(guard_type),
        }),
    ));
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
    events::record(events::StructuredEvent::new(
        "watcher_replaced",
        Some(channel_id),
        Some(provider),
        json!({ "source": normalize_string(source) }),
    ));
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
    reason: &str,
) {
    emit_event(
        "recovery_fired",
        Some(provider),
        Some(channel_id),
        dispatch_id,
        session_key,
        None,
        normalize_string(reason).as_deref(),
        CounterDelta {
            recovery_fires: 1,
            ..CounterDelta::default()
        },
        json!({
            "reason": normalize_string(reason),
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
    events::record(events::StructuredEvent::new(
        "turn_cancelled",
        channel_id,
        provider,
        payload.clone(),
    ));
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
/// `"delegated_to_watcher"`, `"cleared_by_bridge"`, `"cleared_by_watcher"`,
/// `"leak_detected_completed_stale"`. `delegated_to_watcher` and
/// `cleared_by_watcher` should pair 1:1; a sustained drift between counters
/// indicates the bridge handed off cleanup but the watcher never executed it.
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

pub fn record_invariant_check(condition: bool, violation: InvariantViolation<'_>) -> bool {
    if condition {
        return true;
    }

    let invariant = normalize_string(violation.invariant).unwrap_or_else(|| "unknown".to_string());
    tracing::error!(
        invariant = %invariant,
        provider = violation.provider.unwrap_or_default(),
        channel_id = violation.channel_id.unwrap_or_default(),
        dispatch_id = violation.dispatch_id.unwrap_or_default(),
        session_key = violation.session_key.unwrap_or_default(),
        turn_id = violation.turn_id.unwrap_or_default(),
        code_location = violation.code_location,
        "[invariant] {}",
        violation.message
    );

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
    let event_type = normalize_string(event_type);
    if event_type.is_none() {
        return;
    }

    let provider = provider.and_then(normalize_string);
    let channel_id = channel_id.map(|value| value.to_string());
    if !counter_delta.is_zero()
        && let (Some(provider), Some(channel_id)) = (provider.as_ref(), channel_id.as_ref())
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

    let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    let queued = QueuedEvent {
        event_type: event_type.unwrap_or_default(),
        provider,
        channel_id,
        dispatch_id: dispatch_id.and_then(normalize_string),
        session_key: session_key.and_then(normalize_string),
        turn_id: turn_id.and_then(normalize_string),
        status: status.and_then(normalize_string),
        payload_json,
    };

    if let Some(sender) = super::worker::worker_sender() {
        let _ = sender.send(WorkerMessage::Event(queued));
    }
}

