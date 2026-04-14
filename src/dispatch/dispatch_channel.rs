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

/// Determine provider from a Discord channel name suffix.
pub(super) fn provider_from_channel_suffix(channel: &str) -> Option<&'static str> {
    ProviderKind::from_channel_suffix(channel).and_then(|provider| match provider {
        ProviderKind::Claude => Some("claude"),
        ProviderKind::Codex => Some("codex"),
        ProviderKind::Gemini => Some("gemini"),
        ProviderKind::Qwen => Some("qwen"),
        ProviderKind::Unsupported(_) => None,
    })
}
