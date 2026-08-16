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
    use crate::services::discord::{
        ChannelId, TmuxWatcherHandle, make_shared_data_for_tests, tmux_watcher_now_ms,
    };

    fn watcher_handle(tmux_session_name: &str) -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: format!("/tmp/{tmux_session_name}.jsonl"),
            paused: Arc::new(AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(
                std::sync::atomic::AtomicI64::new(tmux_watcher_now_ms()),
            ),
        }
    }

    // #5071 T3-A2 regression lock (no behaviour change): the watcher task guard
    // already removes through the registry's current-handle CAS. Pin both arms
    // so the post-stream-exit sibling that now makes the same call cannot be
    // "simplified" back into an unconditional or channel-keyed removal here.
    #[test]
    fn task_guard_removes_the_registry_entry_it_still_owns() {
        let shared = make_shared_data_for_tests();
        let channel = ChannelId::new(5071);
        let tmux = "AgentDesk-claude-adk-cc";

        let handle = watcher_handle(tmux);
        let cancel = handle.cancel.clone();
        shared.tmux_watchers.insert(channel, handle);

        drop(TmuxWatcherTaskGuard {
            shared: shared.clone(),
            tmux_session_name: tmux.to_string(),
            cancel,
        });

        assert!(!shared.tmux_watchers.has_live_watcher_handle(tmux));
        assert_eq!(
            shared.tmux_watchers.owner_channel_for_tmux_session(tmux),
            None
        );
        assert!(!shared.tmux_watchers.contains_key(&channel));
    }

    #[test]
    fn task_guard_leaves_a_replacement_watcher_entry_in_place() {
        let shared = make_shared_data_for_tests();
        let channel = ChannelId::new(4794);
        let tmux = "AgentDesk-claude-adk-cc";

        let old_handle = watcher_handle(tmux);
        let old_cancel = old_handle.cancel.clone();
        shared.tmux_watchers.insert(channel, old_handle);

        let new_handle = watcher_handle(tmux);
        let new_cancel = new_handle.cancel.clone();
        shared.tmux_watchers.insert(channel, new_handle);

        drop(TmuxWatcherTaskGuard {
            shared: shared.clone(),
            tmux_session_name: tmux.to_string(),
            cancel: old_cancel,
        });

        let registered = shared
            .tmux_watchers
            .by_tmux_session
            .get(tmux)
            .map(|entry| entry.cancel.clone())
            .expect("the replacement watcher entry must survive the outgoing task's guard");
        assert!(
            Arc::ptr_eq(&registered, &new_cancel),
            "the guard must only remove the handle it was spawned with"
        );
    }

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
