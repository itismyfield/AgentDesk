use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy, Debug)]
pub(in crate::services::discord) enum Site {
    Collect,
    Scope,
    Verdict,
}

static TRACE: std::sync::LazyLock<std::sync::Mutex<BTreeMap<u64, Arc<[AtomicUsize; 3]>>>> =
    std::sync::LazyLock::new(Default::default);
pub(in crate::services::discord) fn reached(channel: u64, site: Site) {
    if let Some(count) = TRACE.lock().unwrap().get(&channel) {
        count[site as usize].fetch_add(1, Ordering::SeqCst);
    }
}
pub(in crate::services::discord) struct Trace(u64, Arc<[AtomicUsize; 3]>);
impl Trace {
    pub(in crate::services::discord) fn new(channel: u64) -> Self {
        let count = Arc::new(std::array::from_fn(|_| AtomicUsize::new(0)));
        assert!(
            TRACE
                .lock()
                .unwrap()
                .insert(channel, count.clone())
                .is_none()
        );
        Self(channel, count)
    }
    pub(in crate::services::discord) fn calls(&self, site: Site) -> usize {
        self.1[site as usize].load(Ordering::SeqCst)
    }
}
impl Drop for Trace {
    fn drop(&mut self) {
        TRACE.lock().unwrap().remove(&self.0);
    }
}
