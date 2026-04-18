use super::{CaptureRequest, CaptureResult, MemoryBackend, MemoryFuture, RecallRequest, RecallResponse};
use crate::services::discord::settings::load_longterm_memory_catalog;
use crate::services::discord::shared_memory::load_shared_knowledge;

#[derive(Clone, Copy)]
pub(crate) struct LocalMemoryBackend;

impl MemoryBackend for LocalMemoryBackend {
    fn recall<'a>(&'a self, request: RecallRequest) -> MemoryFuture<'a, RecallResponse> {
        Box::pin(async move {
            RecallResponse {
                shared_knowledge: load_shared_knowledge(),
                longterm_catalog: load_longterm_memory_catalog(&request.role_id),
                external_recall: None,
                warnings: Vec::new(),
                token_usage: Default::default(),
            }
        })
    }

    fn capture<'a>(&'a self, request: CaptureRequest) -> MemoryFuture<'a, CaptureResult> {
        Box::pin(async move {
            let _ = request;
            CaptureResult::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::memory::RecallMode;
    use crate::runtime_layout::{long_term_memory_root, shared_agent_knowledge_path};
    use crate::services::discord::DispatchProfile;
    use crate::services::provider::ProviderKind;
    use tempfile::TempDir;

    fn install_temp_root() -> (
        std::sync::MutexGuard<'static, ()>,
        TempDir,
        Option<std::ffi::OsString>,
    ) {
        let guard = crate::services::discord::runtime_store::lock_test_env();
        let temp = TempDir::new().unwrap();
        let root = temp.path().join(".adk");
        let shared = shared_agent_knowledge_path(&root);
        let role_mem = long_term_memory_root(&root).join("codex");
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&role_mem).unwrap();
        std::fs::write(&shared, "Remember this").unwrap();
        std::fs::write(
            role_mem.join("facts.md"),
            "---\ndescription: Test facts\n---\n# Facts\ncontent",
        )
        .unwrap();
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &root) };
        (guard, temp, prev)
    }

    fn restore_temp_root(prev: Option<std::ffi::OsString>) {
        match prev {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    #[tokio::test]
    async fn test_local_memory_backend_reuses_existing_readers() {
        let (_guard, _temp, prev) = install_temp_root();
        let backend = LocalMemoryBackend;
        let recall = backend
            .recall(RecallRequest {
                mode: RecallMode::Query,
                provider: ProviderKind::Codex,
                role_id: "codex".to_string(),
                channel_id: 1,
                session_id: "session-1".to_string(),
                dispatch_profile: DispatchProfile::Full,
                user_text: "hello".to_string(),
            })
            .await;
        restore_temp_root(prev);
        assert_eq!(
            recall.shared_knowledge.as_deref(),
            Some("[Shared Agent Knowledge]\nRemember this")
        );
        assert!(
            recall
                .longterm_catalog
                .as_deref()
                .is_some_and(|catalog| catalog.contains("facts.md"))
        );
        assert!(recall.external_recall.is_none());
    }

    #[tokio::test]
    async fn test_local_memory_backend_capture_is_noop_success() {
        let backend = LocalMemoryBackend;
        let result = backend
            .capture(CaptureRequest {
                provider: ProviderKind::Codex,
                role_id: "codex".to_string(),
                channel_id: 1,
                session_id: "session-1".to_string(),
                dispatch_id: None,
                user_text: "user".to_string(),
                assistant_text: "assistant".to_string(),
            })
            .await;
        assert_eq!(result, CaptureResult::default());
    }
}
