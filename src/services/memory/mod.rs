mod local;
mod memento;
mod memento_throttle;
mod runtime_state;

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use crate::services::discord::DispatchProfile;
use crate::services::discord::settings::{MemoryBackendKind, ResolvedMemorySettings, RoleBinding};
use crate::services::provider::ProviderKind;

pub(crate) use local::LocalMemoryBackend;
pub(crate) use memento::{
    MementoBackend, MementoRememberRequest, MementoToolFeedbackRequest, resolve_memento_agent_id,
    resolve_memento_workspace, sanitize_memento_workspace_segment,
};
pub(crate) use memento_throttle::memento_call_metrics_snapshot;
#[cfg(test)]
pub(crate) use memento_throttle::{
    note_memento_dedup_hit, note_memento_remote_call, note_memento_tool_request,
    reset_memento_throttle_for_tests,
};
pub(crate) use runtime_state::{backend_is_active, backend_state, refresh_backend_health};
#[cfg(test)]
pub(crate) use runtime_state::{
    last_refresh_reason_for_tests, reset_for_tests as reset_backend_health_for_tests,
};

pub(crate) const UNBOUND_MEMORY_ROLE_ID: &str = "__unbound_role__";

pub(crate) type MemoryFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub(crate) fn is_zero(self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0
    }

    pub(crate) fn saturating_add_assign(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct RecallRequest {
    pub provider: ProviderKind,
    pub role_id: String,
    pub channel_id: u64,
    pub session_id: String,
    pub dispatch_profile: DispatchProfile,
    pub user_text: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RecallResponse {
    pub shared_knowledge: Option<String>,
    pub longterm_catalog: Option<String>,
    pub external_recall: Option<String>,
    pub warnings: Vec<String>,
    pub token_usage: TokenUsage,
}

#[derive(Clone, Debug)]
pub(crate) struct CaptureRequest {
    pub provider: ProviderKind,
    pub role_id: String,
    pub channel_id: u64,
    pub session_id: String,
    pub dispatch_id: Option<String>,
    pub user_text: String,
    pub assistant_text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionEndReason {
    #[allow(dead_code)]
    IdleExpiry,
    LocalSessionReset,
    TurnCapReached,
}

impl SessionEndReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::IdleExpiry => "idle_expiry",
            Self::LocalSessionReset => "local_session_reset",
            Self::TurnCapReached => "turn_cap_reached",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReflectRequest {
    pub provider: ProviderKind,
    pub role_id: String,
    pub channel_id: u64,
    pub session_id: String,
    pub reason: SessionEndReason,
    pub transcript: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CaptureResult {
    pub warnings: Vec<String>,
    pub skipped: bool,
    pub token_usage: TokenUsage,
}

pub(crate) trait MemoryBackend: Send + Sync {
    fn recall<'a>(&'a self, request: RecallRequest) -> MemoryFuture<'a, RecallResponse>;
    fn capture<'a>(&'a self, request: CaptureRequest) -> MemoryFuture<'a, CaptureResult>;
    fn reflect<'a>(&'a self, request: ReflectRequest) -> MemoryFuture<'a, CaptureResult> {
        Box::pin(async move {
            let _ = request;
            CaptureResult {
                skipped: true,
                ..CaptureResult::default()
            }
        })
    }
}

pub(crate) fn build_memory_backend(
    role_binding: Option<&RoleBinding>,
) -> (ResolvedMemorySettings, Box<dyn MemoryBackend + Send + Sync>) {
    let settings = crate::services::discord::settings::memory_settings_for_binding(role_binding);
    let backend = build_resolved_memory_backend(&settings);
    (settings, backend)
}

pub(crate) fn build_resolved_memory_backend(
    settings: &ResolvedMemorySettings,
) -> Box<dyn MemoryBackend + Send + Sync> {
    match settings.backend {
        MemoryBackendKind::File => Box::new(LocalMemoryBackend),
        MemoryBackendKind::Memento => Box::new(MementoBackend::new(settings.clone())),
    }
}

pub(crate) fn resolve_memory_role_id(role_binding: Option<&RoleBinding>) -> String {
    role_binding
        .map(|binding| binding.role_id.clone())
        .unwrap_or_else(|| UNBOUND_MEMORY_ROLE_ID.to_string())
}

pub(crate) fn resolve_memory_session_id(session_id: Option<&str>, channel_id: u64) -> String {
    session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| channel_id.to_string())
}

fn parse_token_count(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_i64().and_then(|value| u64::try_from(value).ok())),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
}

pub(crate) fn extract_token_usage(value: &Value) -> Option<TokenUsage> {
    const INPUT_KEYS: &[&str] = &[
        "input_tokens",
        "inputTokens",
        "prompt_tokens",
        "promptTokens",
        "promptTokenCount",
        "request_tokens",
        "requestTokens",
    ];
    const OUTPUT_KEYS: &[&str] = &[
        "output_tokens",
        "outputTokens",
        "completion_tokens",
        "completionTokens",
        "completionTokenCount",
        "response_tokens",
        "responseTokens",
    ];
    const PREFERRED_CHILD_KEYS: &[&str] = &[
        "usage",
        "tokenUsage",
        "token_usage",
        "meta",
        "metadata",
        "_meta",
        "result",
        "response",
        "data",
    ];

    match value {
        Value::Object(map) => {
            let input_tokens = INPUT_KEYS
                .iter()
                .find_map(|key| map.get(*key).and_then(parse_token_count));
            let output_tokens = OUTPUT_KEYS
                .iter()
                .find_map(|key| map.get(*key).and_then(parse_token_count));

            if input_tokens.is_some() || output_tokens.is_some() {
                return Some(TokenUsage {
                    input_tokens: input_tokens.unwrap_or(0),
                    output_tokens: output_tokens.unwrap_or(0),
                });
            }

            for key in PREFERRED_CHILD_KEYS {
                if let Some(child) = map.get(*key) {
                    if let Some(usage) = extract_token_usage(child) {
                        return Some(usage);
                    }
                }
            }

            for child in map.values() {
                if let Some(usage) = extract_token_usage(child) {
                    return Some(usage);
                }
            }

            None
        }
        Value::Array(items) => items.iter().find_map(extract_token_usage),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::settings::{
        MemoryBackendKind, ResolvedMemorySettings, RoleBinding,
    };
    use crate::services::provider::ProviderKind;
    use serde_json::json;

    #[test]
    fn test_build_resolved_memory_backend_file_and_memento() {
        let file = build_resolved_memory_backend(&ResolvedMemorySettings::default());
        let _ = file;

        let memento = build_resolved_memory_backend(&ResolvedMemorySettings {
            backend: MemoryBackendKind::Memento,
            ..ResolvedMemorySettings::default()
        });
        let _ = memento;
    }

    #[test]
    fn test_build_memory_backend_resolves_binding_memory_settings_before_building() {
        let binding = RoleBinding {
            role_id: "codex".to_string(),
            prompt_file: "/tmp/codex.md".to_string(),
            provider: Some(ProviderKind::Codex),
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: true,
            memory: ResolvedMemorySettings {
                backend: MemoryBackendKind::Memento,
                ..ResolvedMemorySettings::default()
            },
        };

        let (resolved, backend) = build_memory_backend(Some(&binding));
        let _ = backend;
        assert_eq!(resolved.backend, MemoryBackendKind::Memento);

        let (default_settings, default_backend) = build_memory_backend(None);
        let _ = default_backend;
        assert_eq!(default_settings.backend, MemoryBackendKind::File);
    }

    #[test]
    fn test_resolve_memory_role_id_uses_sentinel_when_binding_missing() {
        assert_eq!(resolve_memory_role_id(None), UNBOUND_MEMORY_ROLE_ID);

        let binding = RoleBinding {
            role_id: "codex".to_string(),
            prompt_file: "/tmp/codex.md".to_string(),
            provider: Some(ProviderKind::Codex),
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: true,
            memory: ResolvedMemorySettings::default(),
        };
        assert_eq!(resolve_memory_role_id(Some(&binding)), "codex");
    }

    #[test]
    fn test_resolve_memory_session_id_falls_back_to_channel_id() {
        assert_eq!(resolve_memory_session_id(None, 42), "42");
        assert_eq!(resolve_memory_session_id(Some(""), 42), "42");
        assert_eq!(resolve_memory_session_id(Some("sess-1"), 42), "sess-1");
    }

    #[test]
    fn test_extract_token_usage_prefers_common_usage_shapes() {
        let usage = extract_token_usage(&json!({
            "usage": {
                "inputTokens": 17,
                "outputTokens": 9
            }
        }))
        .expect("usage should be extracted");
        assert_eq!(
            usage,
            TokenUsage {
                input_tokens: 17,
                output_tokens: 9,
            }
        );

        let nested = extract_token_usage(&json!({
            "result": {
                "meta": {
                    "prompt_tokens": "23",
                    "completion_tokens": 4
                }
            }
        }))
        .expect("nested usage should be extracted");
        assert_eq!(
            nested,
            TokenUsage {
                input_tokens: 23,
                output_tokens: 4,
            }
        );
    }
}
