mod local;
mod mem0;

use std::future::Future;
use std::pin::Pin;

use crate::services::discord::DispatchProfile;
use crate::services::discord::settings::{MemoryBackendKind, ResolvedMemorySettings, RoleBinding};
use crate::services::provider::ProviderKind;

pub(crate) use local::LocalMemoryBackend;
pub(crate) use mem0::Mem0Backend;

pub(crate) const UNBOUND_MEMORY_ROLE_ID: &str = "__unbound_role__";

pub(crate) type MemoryFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CaptureResult {
    pub warnings: Vec<String>,
    pub skipped: bool,
}

pub(crate) trait MemoryBackend: Send + Sync {
    fn recall<'a>(&'a self, request: RecallRequest) -> MemoryFuture<'a, RecallResponse>;
    fn capture<'a>(&'a self, request: CaptureRequest) -> MemoryFuture<'a, CaptureResult>;
}

pub(crate) fn build_memory_backend(
    settings: &ResolvedMemorySettings,
) -> Box<dyn MemoryBackend + Send + Sync> {
    match settings.backend {
        MemoryBackendKind::Local => Box::new(LocalMemoryBackend),
        MemoryBackendKind::Mem0 => Box::new(Mem0Backend::new(settings.clone())),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::settings::{
        MemoryBackendKind, ResolvedMemorySettings, RoleBinding,
    };
    use crate::services::provider::ProviderKind;

    #[test]
    fn test_build_memory_backend_local_and_mem0() {
        let local = build_memory_backend(&ResolvedMemorySettings::default());
        let _ = local;

        let mem0 = build_memory_backend(&ResolvedMemorySettings {
            backend: MemoryBackendKind::Mem0,
            ..ResolvedMemorySettings::default()
        });
        let _ = mem0;
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
}
