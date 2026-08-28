use std::borrow::Cow;

use crate::services::discord::bot_role::UtilityBotRole;

/// Actionable operational alerts are delivered by the announce bot first so
/// the channel's resident AgentDesk role receives a human-visible notice. The
/// delivery path stamps non-turn provenance; the outbox worker falls back to
/// the notify bot only when that primary delivery fails (#4449).
pub(crate) const ACTIONABLE_OPS_ALERT_BOT: &str = UtilityBotRole::Announce.alias();

pub(crate) fn is_actionable_ops_alert(source: &str, reason_code: Option<&str>) -> bool {
    matches!(
        (source, reason_code),
        ("outbox_delivery_alert", Some("outbox_delivery_failed"))
            | ("github_sync", Some("github_sync.terminal_open_issue"))
            | ("long_turn_watchdog", Some("long_turn_cluster"))
            | ("relay_signal_rollup", Some("relay_signal.threshold"))
            | ("slo_alerter", Some("slo_threshold_breach"))
            | ("dispatch_watchdog", Some("dispatch_stuck"))
            | ("routine-runtime", Some("routine_paused_stale"))
            | ("auto-queue", Some("auto_queue.entry_dispatch_failed"))
            | ("auto-queue-monitor", Some("auto_queue.monitor_stuck"))
            | ("auto-queue-monitor", Some("auto_queue.monitor_anomaly"))
    )
}

/// Operational outbox alerts are human-visible notifications, not turn input.
/// Keep this classification separate from `is_actionable_ops_alert`: some
/// notify-only alerts still need the same intake provenance if their bot is
/// later configured as an allowed sender.
pub(crate) fn is_non_turn_operational_alert(source: &str, reason_code: Option<&str>) -> bool {
    is_actionable_ops_alert(source, reason_code)
        || matches!(
            source,
            "stall_watchdog" | "quality_regression_alerter" | "queue_overflow_notice"
        )
        || (source == "auto-queue-monitor"
            && reason_code.is_some_and(|reason| reason.starts_with("auto_queue.monitor_")))
}

pub(crate) fn normalized_session_key(target: &str, session_key: Option<&str>) -> Option<String> {
    session_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let target = target.trim();
            (!target.is_empty()).then(|| target.to_string())
        })
}

pub(crate) fn normalized_reason_code(reason_code: Option<&str>) -> Option<&str> {
    reason_code.map(str::trim).filter(|value| !value.is_empty())
}

fn parse_channel_target(target: &str) -> Option<u64> {
    target
        .trim()
        .strip_prefix("channel:")?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
}

fn is_dm_session_channel_segment(value: &str) -> bool {
    value
        .strip_prefix("dm-")
        .is_some_and(|user_id| !user_id.is_empty() && user_id.chars().all(|ch| ch.is_ascii_digit()))
}

fn private_session_provider_from_key(session_key: Option<&str>) -> Option<String> {
    let session_key = session_key
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let parsed = crate::services::discord::session_identity::SessionIdentity::parse(session_key);
    let tmux_name = parsed
        .as_ref()
        .map(|identity| identity.tmux_name.as_str())
        .unwrap_or(session_key);
    let (provider, channel_segment) =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_name)?;
    is_dm_session_channel_segment(&channel_segment).then(|| provider.as_str().to_string())
}

pub(crate) fn delivery_bot_for_target_session<'a>(
    target: &str,
    configured_bot: &'a str,
    session_key: Option<&str>,
) -> Cow<'a, str> {
    if parse_channel_target(target).is_some()
        && let Some(provider_bot) = private_session_provider_from_key(session_key)
    {
        return Cow::Owned(provider_bot);
    }
    Cow::Borrowed(configured_bot)
}

pub(crate) fn dedupe_key_for_message(
    target: &str,
    content: &str,
    reason_code: Option<&str>,
    session_key: Option<&str>,
) -> Option<String> {
    let session_key = session_key
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let reason_code = normalized_reason_code(reason_code);
    let identity_kind = if reason_code.is_some() {
        "reason_code"
    } else {
        "content"
    };
    let content_identity = reason_code.is_none().then_some(content).unwrap_or("");
    let mut hasher = blake3::Hasher::new();
    for part in [
        "message_outbox:v1",
        identity_kind,
        target.trim(),
        session_key,
        reason_code.unwrap_or("").trim(),
        content_identity,
    ] {
        hasher.update(&(part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    Some(format!("message_outbox:v1:{}", hasher.finalize().to_hex()))
}

/// Test-only accessor for [`dedupe_key_for_message`] so sibling modules can
/// assert their dedupe identity is stable (e.g. `long_turn_watchdog` verifying
/// its cluster alert dedupes across scans). Not part of the runtime API.
#[cfg(test)]
pub(crate) fn dedupe_key_for_message_for_test(
    target: &str,
    content: &str,
    reason_code: Option<&str>,
    session_key: Option<&str>,
) -> Option<String> {
    dedupe_key_for_message(target, content, reason_code, session_key)
}
