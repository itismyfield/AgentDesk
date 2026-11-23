//! A spawned background task whose panic is logged instead of lost.

use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures::FutureExt;

pub(crate) fn spawn_observed<F>(task_name: &'static str, future: F) -> tokio::task::JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    // Boxed before the hand-off: unoptimized builds otherwise copy the whole future through
    // tokio::spawn's own frame, which overflowed 2 MiB test threads on the turn-start path.
    tokio::spawn(Box::pin(async move {
        match AssertUnwindSafe(future).catch_unwind().await {
            Ok(()) => {}
            Err(payload) => {
                tracing::error!(
                    target: "agentdesk::services::discord::task_supervisor",
                    task_name,
                    panic = %panic_payload_summary(payload.as_ref()),
                    "discord background task panicked"
                );
            }
        }
    }))
}

pub(crate) fn panic_payload_summary(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "non-string panic payload".to_string()
}
