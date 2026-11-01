use super::{CaptureRequest, CaptureResult, MemoryBackend, MemoryFuture};

#[derive(Clone, Copy)]
pub(crate) struct LocalMemoryBackend;

impl MemoryBackend for LocalMemoryBackend {
    fn capture<'a>(&'a self, request: CaptureRequest) -> MemoryFuture<'a, CaptureResult> {
        Box::pin(async move {
            let _ = request;
            CaptureResult::default()
        })
    }
}
