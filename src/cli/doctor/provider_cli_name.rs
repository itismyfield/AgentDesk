use crate::services::provider::ProviderKind;

pub(super) fn provider_cli_check_name(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Claude => "claude CLI",
        ProviderKind::Codex => "codex CLI",
        ProviderKind::Gemini => "gemini CLI",
        ProviderKind::OpenCode => "opencode CLI",
        ProviderKind::Qwen => "qwen CLI",
        ProviderKind::Grok => "grok CLI",
        ProviderKind::Antigravity => "agy CLI",
        ProviderKind::Unsupported(_) => "provider CLI",
    }
}
