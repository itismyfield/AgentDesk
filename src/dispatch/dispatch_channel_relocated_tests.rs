use super::provider_from_channel_suffix;

#[test]
fn provider_from_channel_suffix_supports_registry_providers() {
    assert_eq!(provider_from_channel_suffix("agent-cc"), Some("claude"));
    assert_eq!(provider_from_channel_suffix("agent-cdx"), Some("codex"));
    assert_eq!(provider_from_channel_suffix("agent-gm"), Some("gemini"));
    assert_eq!(provider_from_channel_suffix("agent-oc"), Some("opencode"));
    assert_eq!(provider_from_channel_suffix("agent-qw"), Some("qwen"));
    assert_eq!(provider_from_channel_suffix("agent"), None);
}
