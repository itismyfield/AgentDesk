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
            RecallResponse {
                shared_knowledge: load_shared_knowledge(),
                longterm_catalog: load_longterm_memory_catalog(&request.role_id),
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
