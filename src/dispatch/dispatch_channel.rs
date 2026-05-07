use crate::services::provider::ProviderKind;

pub(super) fn dispatch_uses_alt_channel(dispatch_type: &str) -> bool {
    matches!(dispatch_type, "review" | "e2e-test" | "consultation")
}

pub(super) fn resolve_dispatch_channel_id(channel: &str) -> Option<u64> {
    channel
        .parse::<u64>()
        .ok()
        .or_else(|| crate::server::routes::dispatches::resolve_channel_alias_pub(channel))
}

pub fn is_unified_thread_channel_active(channel_id: u64) -> bool {
    let _ = channel_id;
    false
}

/// Extract thread channel ID from a channel name's `-t{15+digit}` suffix.
/// Pure parsing — no DB access. Used by both production guards and tests.
#[cfg_attr(not(test), allow(dead_code))]
pub fn extract_thread_channel_id(channel_name: &str) -> Option<u64> {
    let pos = channel_name.rfind("-t")?;
    let suffix = &channel_name[pos + 2..];
    if suffix.len() >= 15 && suffix.chars().all(|c| c.is_ascii_digit()) {
        let id: u64 = suffix.parse().ok()?;
        if id == 0 { None } else { Some(id) }
    } else {
        None
    }
}

/// Check whether a channel name (from tmux session parsing) belongs to an active
/// unified-thread auto-queue run. Extracts the thread channel ID from the
/// `-t{15+digit}` suffix in the channel name.
pub fn is_unified_thread_channel_name_active(channel_name: &str) -> bool {
    let _ = channel_name;
    false
}

pub fn drain_unified_thread_kill_signals() -> Vec<String> {
    Vec::new()
}

/// Determine provider from a Discord channel name suffix.
pub(super) fn provider_from_channel_suffix(channel: &str) -> Option<&'static str> {
    ProviderKind::from_channel_suffix(channel).and_then(|provider| match provider {
        ProviderKind::Claude => Some("claude"),
        ProviderKind::Codex => Some("codex"),
        ProviderKind::Gemini => Some("gemini"),
        ProviderKind::OpenCode => Some("opencode"),
        ProviderKind::Qwen => Some("qwen"),
        ProviderKind::Unsupported(_) => None,
    })
}

pub(crate) fn dispatch_destination_provider_override(
    dispatch_type: Option<&str>,
    context_json: Option<&str>,
) -> Option<String> {
    let key = match dispatch_type {
        Some("review") => "target_provider",
        Some("review-decision") => "from_provider",
        _ => return None,
    };
    let context =
        context_json.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())?;
    context
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[path = "dispatch_channel_relocated_tests.rs"]
mod relocated_tests;
