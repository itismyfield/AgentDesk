use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::ENDING;

type Hook = Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// Hooks a scenario places at a named point of the end task.
static HOOKS: Mutex<Vec<(&'static str, Hook)>> = Mutex::new(Vec::new());

/// Runs every hook registered for `point`, in order.
pub(super) async fn hook(point: &'static str) {
    let hooks: Vec<Hook> = HOOKS
        .lock()
        .unwrap()
        .iter()
        .filter(|(at, _)| *at == point)
        .map(|(_, hook)| hook.clone())
        .collect();
    for run in hooks {
        run().await;
    }
}

pub(in crate::services::discord) fn on(
    point: &'static str,
    hook: impl Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync + 'static,
) {
    HOOKS.lock().unwrap().push((point, Arc::new(hook)));
}

/// `hook` at `point` until the guard drops, for a scenario outside this module.
pub(in crate::services::discord) fn on_while(
    point: &'static str,
    hook: impl Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync + 'static,
) -> Hooked {
    on(point, hook);
    Hooked(point)
}

pub(in crate::services::discord) struct Hooked(&'static str);

impl Drop for Hooked {
    fn drop(&mut self) {
        let point = self.0;
        HOOKS.lock().unwrap().retain(|(at, _)| *at != point);
    }
}

pub(in crate::services::discord) async fn settled_tasks() {
    let idle = wait(Duration::from_secs(30), || {
        ENDING.lock().unwrap().is_empty()
    })
    .await;
    assert!(idle, "the end task finished");
}
pub(in crate::services::discord) async fn wait(
    limit: Duration,
    mut done: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    done()
}

pub(in crate::services::discord) const PRIOR: u64 = 4;

pub(in crate::services::discord) const CURRENT: u64 = 5;
