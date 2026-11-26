//! Test-only pause points inside the run-completion writer; the acceptance tests arm them.
//! It stays beside the writer because `runs.rs` calls it under `cfg(test)`.

#[derive(Clone)]
pub struct CompletionBarrier {
    pub(crate) point: &'static str,
    pub reached: std::sync::Arc<tokio::sync::Notify>,
    pub resume: std::sync::Arc<tokio::sync::Notify>,
}

tokio::task_local! {
    pub static COMPLETION_BARRIER: CompletionBarrier;
}

pub(super) async fn pause_completion_at(point: &str) {
    if let Ok(barrier) = COMPLETION_BARRIER.try_with(Clone::clone)
        && barrier.point == point
    {
        barrier.reached.notify_one();
        barrier.resume.notified().await;
    }
}
