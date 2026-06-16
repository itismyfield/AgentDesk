//! Token-context display logic for the idle-recap card header.
//!
//! Extracted verbatim from the parent `idle_recap` module (#3479): the
//! live/latest-turn/session token selection state machine, freshness and
//! provider-match guards, and the human-readable token/duration formatters.
//! Behavior is unchanged — only the module boundary moved.

use super::*;

pub(super) fn format_token_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecapContextDisplay {
    Known { used: u64, window: u64 },
    Stale,
    Unknown,
}

pub(super) fn select_recap_context(
    snapshot: &RecapSnapshot,
    now: DateTime<Utc>,
) -> RecapContextDisplay {
    let known = |used, window| RecapContextDisplay::Known {
        used: display_context_tokens(snapshot, used, window),
        window,
    };
    if let Some(live) = snapshot
        .live_context_usage
        .filter(|live| live.used_tokens > 0 && live.context_window_tokens > 0)
    {
        return known(live.used_tokens, live.context_window_tokens);
    }

    let window = context_window_for(snapshot);
    if let Some(used) = latest_turn_context_tokens(snapshot) {
        return known(used, window);
    }
    if session_tokens_are_stale_or_incompatible(snapshot, now) {
        return RecapContextDisplay::Stale;
    }
    fresh_session_tokens(snapshot, now)
        .map_or(RecapContextDisplay::Unknown, |used| known(used, window))
}

fn display_context_tokens(snapshot: &RecapSnapshot, used: u64, window: u64) -> u64 {
    match ProviderKind::from_str(&snapshot.provider) {
        Some(ProviderKind::Codex) if window > 0 => used.min(window),
        _ => used,
    }
}

fn latest_turn_context_tokens(snapshot: &RecapSnapshot) -> Option<u64> {
    latest_turn_matches_active_session(snapshot).then_some(())?;
    let input = non_negative_i64_to_u64(snapshot.latest_turn_input_tokens?)?;
    let used = input
        .saturating_add(non_negative_i64_to_u64(
            snapshot.latest_turn_cache_create_tokens.unwrap_or(0),
        )?)
        .saturating_add(non_negative_i64_to_u64(
            snapshot.latest_turn_cache_read_tokens.unwrap_or(0),
        )?);
    (used > 0).then_some(used)
}

fn latest_turn_matches_active_session(snapshot: &RecapSnapshot) -> bool {
    snapshot.latest_turn_finished_at.is_some()
        && same_normalized_opt(
            snapshot.latest_turn_provider.as_deref(),
            Some(snapshot.provider.as_str()),
        )
        && (same_normalized_opt(
            snapshot.latest_turn_session_key.as_deref(),
            Some(snapshot.session_key.as_str()),
        ) || snapshot
            .latest_turn_session_id
            .as_deref()
            .and_then(normalized_text)
            .is_some_and(|latest| provider_session_ids(snapshot).any(|active| active == latest)))
}

pub(super) fn provider_session_ids(snapshot: &RecapSnapshot) -> impl Iterator<Item = &str> {
    [
        snapshot.claude_session_id.as_deref(),
        snapshot.raw_provider_session_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter_map(normalized_text)
}

fn session_tokens_are_stale_or_incompatible(snapshot: &RecapSnapshot, now: DateTime<Utc>) -> bool {
    let Some(tokens) = snapshot.tokens.filter(|value| *value > 0) else {
        return false;
    };
    non_negative_i64_to_u64(tokens).is_none()
        || (snapshot.latest_turn_finished_at.is_some()
            && !latest_turn_matches_active_session(snapshot))
        || !session_tokens_are_fresh(snapshot, now)
}

fn fresh_session_tokens(snapshot: &RecapSnapshot, now: DateTime<Utc>) -> Option<u64> {
    let tokens = non_negative_i64_to_u64(snapshot.tokens?)?;
    (tokens > 0 && session_tokens_are_fresh(snapshot, now)).then_some(tokens)
}

fn session_tokens_are_fresh(snapshot: &RecapSnapshot, now: DateTime<Utc>) -> bool {
    snapshot.tokens_updated_at.is_some_and(|updated_at| {
        let age = now - updated_at;
        age.num_seconds() >= 0 && age.num_seconds() <= SESSION_TOKEN_FRESHNESS_MAX_SECS
    })
}

fn non_negative_i64_to_u64(value: i64) -> Option<u64> {
    u64::try_from(value).ok()
}

fn same_normalized_opt(left: Option<&str>, right: Option<&str>) -> bool {
    matches!(
        (left.and_then(normalized_text), right.and_then(normalized_text),),
        (Some(left), Some(right)) if left.eq_ignore_ascii_case(right)
    )
}

pub(super) fn normalized_text(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn context_window_for(snapshot: &RecapSnapshot) -> u64 {
    ProviderKind::from_str(&snapshot.provider).map_or(FALLBACK_CONTEXT_WINDOW_TOKENS, |provider| {
        provider.resolve_context_window(snapshot.model.as_deref())
    })
}

pub(super) fn format_korean_duration(dur: chrono::Duration) -> String {
    let secs = dur.num_seconds().max(0);
    if secs >= 86_400 {
        format!("{}일", secs / 86_400)
    } else if secs >= 3_600 {
        format!("{}시간", secs / 3_600)
    } else if secs >= 60 {
        format!("{}분", secs / 60)
    } else {
        format!("{}초", secs)
    }
}
