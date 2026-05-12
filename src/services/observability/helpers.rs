//! #2049: helpers/utilities split out of `mod.rs` to keep the observability
//! façade focused on the public surface. Pure functions only — no globals,
//! no I/O. All call sites import via `super::*` re-export, so external
//! consumers see no API change.

use serde_json::{Value, json};

use super::{
    AGENT_QUALITY_EVENT_TYPES, AnalyticsCounterSnapshot, AnalyticsFilters, CounterValues,
    DEFAULT_COUNTER_LIMIT, DEFAULT_EVENT_LIMIT, DEFAULT_INVARIANT_LIMIT,
    DEFAULT_QUALITY_DAILY_LIMIT, DEFAULT_QUALITY_DAYS, DEFAULT_QUALITY_LIMIT,
    DEFAULT_QUALITY_RANKING_LIMIT, InvariantViolationRecord, MAX_COUNTER_LIMIT, MAX_EVENT_LIMIT,
    MAX_INVARIANT_LIMIT, MAX_QUALITY_DAILY_LIMIT, MAX_QUALITY_DAYS, MAX_QUALITY_LIMIT,
    MAX_QUALITY_RANKING_LIMIT,
};

pub(super) fn counter_snapshot_from_values(
    provider: &str,
    channel_id: &str,
    values: CounterValues,
    source: &str,
    snapshot_at: String,
) -> AnalyticsCounterSnapshot {
    // #2049 Finding 20: divide by the *actual* observation count and elide the
    // ratio when no successes/failures observed instead of saturating the
    // denominator to 1. The previous formulation produced "100% of 0" when
    // emit_turn_started was lost but emit_turn_finished still fired (race),
    // which misled the dashboards. Using `successes + failures` as the
    // denominator keeps the rates self-consistent even when `turn_attempts`
    // is briefly skewed.
    let total = values.turn_successes.saturating_add(values.turn_failures);
    let (success_rate, failure_rate) = if total == 0 {
        (None, None)
    } else {
        let denom = total as f64;
        (
            Some(values.turn_successes as f64 / denom),
            Some(values.turn_failures as f64 / denom),
        )
    };
    AnalyticsCounterSnapshot {
        provider: provider.to_string(),
        channel_id: channel_id.to_string(),
        turn_attempts: values.turn_attempts,
        guard_fires: values.guard_fires,
        watcher_replacements: values.watcher_replacements,
        recovery_fires: values.recovery_fires,
        turn_successes: values.turn_successes,
        turn_failures: values.turn_failures,
        success_rate,
        failure_rate,
        snapshot_at,
        source: source.to_string(),
    }
}

pub(super) fn matches_filters(
    filters: Option<&AnalyticsFilters>,
    provider: &str,
    channel_id: &str,
    event_type: Option<&str>,
) -> bool {
    let Some(filters) = filters else {
        return true;
    };
    if let Some(expected) = filters.provider.as_deref()
        && expected != provider
    {
        return false;
    }
    if let Some(expected) = filters.channel_id.as_deref()
        && expected != channel_id
    {
        return false;
    }
    if let Some(expected) = filters.event_type.as_deref()
        && event_type != Some(expected)
    {
        return false;
    }
    true
}

pub(super) fn normalize_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub(super) fn normalize_quality_event_type(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
    AGENT_QUALITY_EVENT_TYPES
        .iter()
        .any(|candidate| *candidate == normalized)
        .then_some(normalized)
}

pub(super) fn value_as_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

pub(super) fn invariant_record_from_parts(
    id: i64,
    invariant: String,
    provider: Option<String>,
    channel_id: Option<String>,
    dispatch_id: Option<String>,
    session_key: Option<String>,
    turn_id: Option<String>,
    payload: Value,
    created_at: String,
) -> InvariantViolationRecord {
    let message = payload.get("message").and_then(value_as_string);
    let code_location = payload.get("code_location").and_then(value_as_string);
    let details = payload.get("details").cloned().unwrap_or_else(|| json!({}));
    InvariantViolationRecord {
        id,
        invariant,
        provider,
        channel_id,
        dispatch_id,
        session_key,
        turn_id,
        message,
        code_location,
        details,
        created_at,
    }
}

pub(super) fn normalized_event_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_EVENT_LIMIT,
        value => value.min(MAX_EVENT_LIMIT),
    }
}

pub(super) fn normalized_counter_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_COUNTER_LIMIT,
        value => value.min(MAX_COUNTER_LIMIT),
    }
}

pub(super) fn normalized_invariant_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_INVARIANT_LIMIT,
        value => value.min(MAX_INVARIANT_LIMIT),
    }
}

pub(super) fn normalized_quality_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_QUALITY_LIMIT,
        value => value.min(MAX_QUALITY_LIMIT),
    }
}

pub(super) fn normalized_quality_daily_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_QUALITY_DAILY_LIMIT,
        value => value.min(MAX_QUALITY_DAILY_LIMIT),
    }
}

pub(super) fn normalized_quality_ranking_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_QUALITY_RANKING_LIMIT,
        value => value.min(MAX_QUALITY_RANKING_LIMIT),
    }
}

pub(super) fn normalized_quality_days(days: i64) -> i64 {
    match days {
        value if value <= 0 => DEFAULT_QUALITY_DAYS,
        value => value.min(MAX_QUALITY_DAYS),
    }
}

pub(super) fn now_kst() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub(super) fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
