use super::{
    CaptureRequest, CaptureResult, LocalMemoryBackend, MemoryBackend, MemoryFuture, RecallRequest,
    RecallResponse,
};

const MEMENTO_FALLBACK_WARNING: &str =
    "memento backend selected; falling back to file-backed memory until MCP integration is enabled";

#[derive(Clone, Copy)]
pub(crate) struct MementoBackend;

impl MemoryBackend for MementoBackend {
    fn recall<'a>(&'a self, request: RecallRequest) -> MemoryFuture<'a, RecallResponse> {
        Box::pin(async move {
            let mut response = LocalMemoryBackend.recall(request).await;
            response.warnings.push(MEMENTO_FALLBACK_WARNING.to_string());
            response
        })
    }

    fn capture<'a>(&'a self, request: CaptureRequest) -> MemoryFuture<'a, CaptureResult> {
        Box::pin(async move {
            let mut result = LocalMemoryBackend.capture(request).await;
            result.warnings.push(MEMENTO_FALLBACK_WARNING.to_string());
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::DispatchProfile;
    use crate::services::provider::ProviderKind;

    #[tokio::test]
    async fn test_memento_backend_warns_when_falling_back_to_file_backend() {
        let backend = MementoBackend;
        let recall = backend
            .recall(RecallRequest {
                provider: ProviderKind::Codex,
                role_id: "codex".to_string(),
                channel_id: 1,
                session_id: "session-1".to_string(),
                dispatch_profile: DispatchProfile::Full,
                user_text: "hello".to_string(),
            })
            .await;

        assert!(
            recall
                .warnings
                .iter()
                .any(|warning| warning.contains("falling back to file-backed memory"))
        );

        let capture = backend
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

        assert!(
            capture
                .warnings
                .iter()
                .any(|warning| warning.contains("falling back to file-backed memory"))
        );
    }
}
