use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use futures::FutureExt;

use super::SharedData;

pub(in crate::services::discord) fn spawn_observed<F>(
    task_name: &'static str,
    future: F,
) -> tokio::task::JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        match AssertUnwindSafe(future).catch_unwind().await {
            Ok(()) => {}
            Err(payload) => {
                tracing::error!(
                    task_name,
                    panic = %panic_payload_summary(payload.as_ref()),
                    "discord background task panicked"
                );
            }
        }
    })
}

pub(in crate::services::discord) fn spawn_observed_tmux_watcher<F>(
    task_name: &'static str,
    shared: Arc<SharedData>,
    tmux_session_name: String,
    cancel: Arc<AtomicBool>,
    future: F,
) -> tokio::task::JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    spawn_observed(task_name, async move {
        let _cleanup_guard = TmuxWatcherTaskGuard {
            shared,
            tmux_session_name,
            cancel,
        };
        future.await;
    })
}

struct TmuxWatcherTaskGuard {
    shared: Arc<SharedData>,
    tmux_session_name: String,
    cancel: Arc<AtomicBool>,
}

impl Drop for TmuxWatcherTaskGuard {
    fn drop(&mut self) {
        if let Some((owner_channel_id, _handle)) = self
            .shared
            .tmux_watchers
            .remove_tmux_session_if_current(&self.tmux_session_name, &self.cancel)
        {
            tracing::info!(
                channel_id = owner_channel_id.get(),
                tmux_session_name = %self.tmux_session_name,
                "tmux watcher task exited; removed matching watcher registry entry"
            );
        }
    }
}

fn panic_payload_summary(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "non-string panic payload".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_payload_summary_handles_common_payloads() {
        assert_eq!(panic_payload_summary(&"boom"), "boom");
        assert_eq!(panic_payload_summary(&"owned".to_string()), "owned");
    }

    #[tokio::test]
    async fn spawn_observed_contains_child_panic() {
        let handle = spawn_observed("unit-test-panic", async {
            panic!("observed panic");
        });

        assert!(
            handle.await.is_ok(),
            "observer task should catch and log child panic instead of propagating JoinError"
        );
    }
}
