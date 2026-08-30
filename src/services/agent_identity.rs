//! Agent runtime identity formatter.
//!
//! Pure functions only: no I/O, no env reads. This is the single source of
//! `model_short`, `identity_short`, and `identity_label`.

use crate::services::provider::ProviderKind;

const FAMILY_TOKENS: &[&str] = &["luna", "terra", "opus", "sonnet", "haiku"];
const DEFAULT_MODEL_SHORT: &str = "default-model";
const DEFAULT_PROFILE: &str = "default";
const MODEL_SHORT_MAX: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRuntimeIdentity {
    pub agent_id: String,
    pub display_name: String,
    pub provider: ProviderKind,
    pub model: Option<String>,
    pub model_short: String,
    pub auth_profile: String,
}

pub fn normalize_auth_profile(profile: Option<&str>) -> String {
    match profile.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => value.to_string(),
        None => DEFAULT_PROFILE.to_string(),
    }
}

pub fn identity_from_parts(
    agent_id: impl Into<String>,
    display_name: impl Into<String>,
    provider: ProviderKind,
    model: Option<String>,
    auth_profile: Option<&str>,
) -> AgentRuntimeIdentity {
    let model_short = model_short(model.as_deref(), &provider);
    AgentRuntimeIdentity {
        agent_id: agent_id.into(),
        display_name: display_name.into(),
        provider,
        model,
        model_short,
        auth_profile: normalize_auth_profile(auth_profile),
    }
}

pub fn model_short(model: Option<&str>, provider: &ProviderKind) -> String {
    let Some(raw) = model.map(str::trim).filter(|value| !value.is_empty()) else {
        return DEFAULT_MODEL_SHORT.to_string();
    };
    let base = raw.rsplit('/').next().unwrap_or(raw);
    if let Some(family) = base
        .rsplit('-')
        .next()
        .filter(|token| FAMILY_TOKENS.iter().any(|family| family == token))
    {
        return family.to_string();
    }
    if matches!(provider, ProviderKind::Grok) {
        if let Some(version) = base.strip_prefix("grok-").filter(|value| !value.is_empty()) {
            return version.to_string();
        }
    }
    truncate_chars(base, MODEL_SHORT_MAX)
}

pub fn identity_short(id: &AgentRuntimeIdentity) -> String {
    format!("{} {}", id.provider.as_str(), id.model_short)
}

pub fn identity_label(id: &AgentRuntimeIdentity) -> String {
    format!(
        "{} · {} {} · {}",
        id.agent_id,
        id.provider.as_str(),
        id.model_short,
        id.auth_profile
    )
}

pub fn identity_json(id: &AgentRuntimeIdentity) -> serde_json::Value {
    serde_json::json!({
        "agent_id": id.agent_id,
        "provider": id.provider.as_str(),
        "model": id.model,
        "model_short": id.model_short,
        "auth_profile": id.auth_profile,
        "label": identity_label(id),
        "short": identity_short(id),
    })
}

fn truncate_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(
        agent_id: &str,
        provider: ProviderKind,
        model: Option<&str>,
        profile: Option<&str>,
    ) -> AgentRuntimeIdentity {
        identity_from_parts(
            agent_id,
            agent_id,
            provider,
            model.map(str::to_string),
            profile,
        )
    }

    #[test]
    fn test_009_model_short_and_identity_short() {
        let coder = identity("coder", ProviderKind::Codex, Some("gpt-5.6-terra"), None);
        assert_eq!(coder.model_short, "terra");
        assert_eq!(identity_short(&coder), "codex terra");
        assert_eq!(coder.auth_profile, "default");

        let spark = identity(
            "spark",
            ProviderKind::Codex,
            Some("gpt-5.6-luna"),
            Some("luna"),
        );
        assert_eq!(spark.model_short, "luna");
        assert_eq!(identity_label(&spark), "spark · codex luna · luna");

        let claude = identity("claude", ProviderKind::Grok, Some("grok-4.6"), None);
        assert_eq!(claude.model_short, "4.6");
        assert_eq!(identity_label(&claude), "claude · grok 4.6 · default");

        let qwen = identity(
            "qwen",
            ProviderKind::Qwen,
            Some("nvidia/nemotron-3-ultra-550b-a55b"),
            Some("qwen-b"),
        );
        assert_eq!(qwen.model_short, "nemotron-3-ultra-550b-a55b");
        assert_eq!(
            identity_label(&qwen),
            "qwen · qwen nemotron-3-ultra-550b-a55b · qwen-b"
        );

        let gemini = identity("gemini", ProviderKind::Antigravity, None, None);
        assert_eq!(gemini.model_short, "default-model");
        assert_eq!(
            identity_label(&gemini),
            "gemini · antigravity default-model · default"
        );
    }

    #[test]
    fn identity_json_has_no_secret_fields() {
        let id = identity(
            "coder",
            ProviderKind::Codex,
            Some("gpt-5.6-terra"),
            Some("work"),
        );
        let value = identity_json(&id);
        let encoded = value.to_string();
        assert!(!encoded.contains("token"));
        assert!(!encoded.contains("sk-"));
        assert_eq!(value["label"], "coder · codex terra · work");
        assert_eq!(value["short"], "codex terra");
        assert_eq!(value["auth_profile"], "work");
    }

    #[test]
    fn agy_alias_interns_to_antigravity_via_provider_kind() {
        let provider = ProviderKind::from_str("agy").expect("agy alias");
        assert_eq!(provider.as_str(), "antigravity");
        let id = identity("gemini", provider, None, None);
        assert_eq!(identity_short(&id), "antigravity default-model");
    }
}
