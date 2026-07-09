use super::{
    CaptureRequest, CaptureResult, MemoryBackend, MemoryFuture, RecallRequest, RecallResponse,
};
use crate::services::discord::settings::load_longterm_memory_catalog;
use crate::services::discord::shared_memory::load_shared_knowledge;

#[derive(Clone, Copy)]
pub(crate) struct LocalMemoryBackend;

impl MemoryBackend for LocalMemoryBackend {
    fn recall<'a>(&'a self, request: RecallRequest) -> MemoryFuture<'a, RecallResponse> {
        Box::pin(async move {
            let shared_knowledge = load_shared_knowledge();
            let longterm_catalog = load_longterm_memory_catalog(&request.role_id);

            // Empty-catalog robustness (#4316): a missing or empty memories
            // directory (fresh install, local archive) is not an error. Warn
            // once so the empty state is visible rather than silently emitting
            // empty guidance, then return normally.
            if longterm_catalog.is_none() {
                tracing::warn!(
                    role_id = %request.role_id,
                    "local memory recall: long-term catalog is empty (no memories on disk); returning empty guidance"
                );
            }

            RecallResponse {
                shared_knowledge,
                longterm_catalog,
                external_recall: None,
                memento_context_loaded: false,
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
    use crate::services::discord::DispatchProfile;
    use crate::services::memory::RecallMode;
    use crate::services::provider::ProviderKind;

    fn recall_request(role_id: &str) -> RecallRequest {
        RecallRequest {
            provider: ProviderKind::Claude,
            role_id: role_id.to_string(),
            channel_id: 1,
            channel_name: None,
            session_id: "session".to_string(),
            dispatch_profile: DispatchProfile::Full,
            user_text: "hello".to_string(),
            mode: RecallMode::Full,
        }
    }

    #[tokio::test]
    async fn recall_with_empty_catalog_returns_empty_guidance_without_panicking() {
        // #4316: a missing/empty memories directory must degrade gracefully —
        // no panic, an empty long-term catalog, and no synthetic warnings on the
        // response. (The empty state is surfaced via a warn log, not an error.)
        let backend = LocalMemoryBackend;
        let response = backend
            .recall(recall_request("nonexistent-role-4316"))
            .await;

        assert!(
            response.longterm_catalog.is_none(),
            "empty memories dir must yield no long-term catalog, got: {:?}",
            response.longterm_catalog
        );
        assert!(!response.memento_context_loaded);
        assert!(response.warnings.is_empty());
        assert!(response.external_recall.is_none());
    }
}
