//! Loops meant to run for the whole process: a panic or return is logged instead of
//! silently ending the loop, and health reads which loops are still running.

use std::any::Any;
use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Mutex, Once, OnceLock};

use futures::FutureExt;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LoopState {
    Running,
    Returned,
    Panicked,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ResidentLoop {
    pub name: String,
    pub state: LoopState,
    pub since: chrono::DateTime<chrono::Utc>,
}

fn registry() -> &'static Mutex<BTreeMap<String, ResidentLoop>> {
    static LOOPS: OnceLock<Mutex<BTreeMap<String, ResidentLoop>>> = OnceLock::new();
    LOOPS.get_or_init(Default::default)
}

fn record(name: &str, state: LoopState) {
    let (name, since) = (name.to_string(), chrono::Utc::now());
    let mut loops = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loops.insert(name.clone(), ResidentLoop { name, state, since });
}

/// Spawns a process-lifetime loop; its end, by return or by panic, is logged and kept for health.
pub(crate) fn spawn_resident<F>(name: impl Into<String>, future: F) -> tokio::task::JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let name = name.into();
    record(&name, LoopState::Running);
    tokio::spawn(Box::pin(async move {
        match AssertUnwindSafe(future).catch_unwind().await {
            Ok(()) => {
                record(&name, LoopState::Returned);
                tracing::warn!(task_name = %name, "resident loop returned");
            }
            Err(payload) => {
                record(&name, LoopState::Panicked);
                let panic = panic_message(payload.as_ref());
                tracing::error!(task_name = %name, %panic, "resident loop panicked");
            }
        }
    }))
}

pub(crate) fn snapshot() -> Vec<ResidentLoop> {
    let loops = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loops.values().cloned().collect()
}

/// Health detail: how many loops run, and each loop that no longer does.
pub(crate) fn health_json() -> serde_json::Value {
    let loops = snapshot();
    let running = loops
        .iter()
        .filter(|l| l.state == LoopState::Running)
        .count();
    let stopped: Vec<_> = loops
        .into_iter()
        .filter(|l| l.state != LoopState::Running)
        .collect();
    serde_json::json!({ "running": running, "stopped": stopped })
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "non-string panic payload".to_string()
}

/// Logs every panic through tracing before the previous hook runs, so it reaches the
/// configured log sink and not only stderr.
pub(crate) fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map(|at| format!("{}:{}", at.file(), at.line()));
            let thread = std::thread::current().name().map(str::to_string);
            let panic = panic_message(info.payload());
            tracing::error!(location, thread, %panic, "panic");
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_of(name: &str) -> Option<LoopState> {
        snapshot()
            .into_iter()
            .find(|l| l.name == name)
            .map(|l| l.state)
    }

    #[tokio::test]
    async fn a_panicking_loop_is_reported_instead_of_vanishing() {
        let handle = spawn_resident("resident-test-panics", async { panic!("boom") });

        assert!(
            handle.await.is_ok(),
            "the panic must be caught inside the task"
        );
        assert_eq!(state_of("resident-test-panics"), Some(LoopState::Panicked));
        let stopped = health_json()["stopped"].to_string();
        assert!(stopped.contains("resident-test-panics"), "{stopped}");
    }

    #[tokio::test]
    async fn a_running_loop_counts_as_running_and_a_returned_one_does_not() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = spawn_resident("resident-test-returns", async {
            let _ = rx.await;
        });
        assert_eq!(state_of("resident-test-returns"), Some(LoopState::Running));

        tx.send(()).unwrap();
        handle.await.unwrap();
        assert_eq!(state_of("resident-test-returns"), Some(LoopState::Returned));
    }
}
