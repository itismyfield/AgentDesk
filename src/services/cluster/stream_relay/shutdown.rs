use super::*;

#[derive(Debug, PartialEq, Eq)]
pub enum ShutdownOutcome {
    Joined,
    NoTask,
}

impl StreamRelayHandle {
    /// Compatibility entry point: discard the shutdown join result.
    #[allow(dead_code)] // Compatibility wrapper; production observes shutdown_with_result.
    pub async fn shutdown(self) {
        let _ = self.shutdown_with_result().await;
    }

    /// Close producer admission and wait for the worker's best-effort drain.
    /// A successful join is not proof of delivery success or detached-work completion.
    /// Panic/cancellation remain JoinError; NoTask does not certify a join.
    pub async fn shutdown_with_result(self) -> Result<ShutdownOutcome, tokio::task::JoinError> {
        let StreamRelayHandle {
            queue,
            shutdown,
            shutdown_notify,
            task,
            ..
        } = self;
        shutdown.store(true, Ordering::Release);
        // Wake the relay loop's `select!` so it observes the flag and exits.
        // `notify_one` (not `notify_waiters`) stores a single permit so the
        // wakeup survives the pre-waiter race: if shutdown lands while the
        // loop is mid-`deliver_frame` (no `Notified` future armed), the
        // permit is consumed by the next `notified().await`. The
        // `shutdown.load()` guard at the top of each loop iteration is the
        // fail-closed backstop against any residual missed-notify.
        shutdown_notify.notify_one();
        queue.close();
        if let Some(handle) = task {
            handle.await?;
            return Ok(ShutdownOutcome::Joined);
        }
        Ok(ShutdownOutcome::NoTask)
    }
}
