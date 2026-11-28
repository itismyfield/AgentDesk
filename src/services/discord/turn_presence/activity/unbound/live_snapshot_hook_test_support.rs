use super::lock;
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

pub(in crate::services::discord) struct Pause {
    pub(in crate::services::discord) at: usize,
    pub(in crate::services::discord) count: AtomicUsize,
    pub(in crate::services::discord) entered: tokio::sync::Notify,
    pub(in crate::services::discord) resume: tokio::sync::Notify,
}
pub(in crate::services::discord) static PAUSES: std::sync::LazyLock<
    Mutex<BTreeMap<u64, Arc<Pause>>>,
> = std::sync::LazyLock::new(Default::default);
pub(in crate::services::discord) async fn after_files(channel: u64) {
    let pause = lock(&PAUSES).get(&channel).cloned();
    if let Some(pause) = pause {
        if pause.count.fetch_add(1, Ordering::SeqCst) + 1 == pause.at {
            pause.entered.notify_one();
            pause.resume.notified().await;
        }
    }
}
pub(in crate::services::discord) struct PauseGuard(pub(in crate::services::discord) u64);
impl Drop for PauseGuard {
    fn drop(&mut self) {
        lock(&PAUSES).remove(&self.0);
    }
}
