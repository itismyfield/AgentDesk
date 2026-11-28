//! Test-only apply barriers scoped to the candidate session key.
//! Installers hold the PostgreSQL test lifecycle guard so they share one slot.

use std::sync::{Mutex, OnceLock};

pub(crate) struct StaleSweepApplyBarrierPoint(tokio::sync::Barrier);

impl StaleSweepApplyBarrierPoint {
    pub(crate) fn new(parties: usize) -> Self {
        Self(tokio::sync::Barrier::new(parties))
    }

    pub(crate) async fn wait(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(10), self.0.wait())
            .await
            .expect("timed out waiting for stale-sweep apply barrier");
    }
}

#[derive(Clone)]
pub(crate) struct StaleSweepApplyBarrier {
    pub(crate) reached: std::sync::Arc<StaleSweepApplyBarrierPoint>,
    pub(crate) resume: std::sync::Arc<StaleSweepApplyBarrierPoint>,
}

type KeyedBarrier = (String, StaleSweepApplyBarrier);

fn slot() -> &'static Mutex<Option<KeyedBarrier>> {
    static SLOT: OnceLock<Mutex<Option<KeyedBarrier>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub(crate) struct StaleSweepApplyBarrierGuard(Option<KeyedBarrier>);

impl Drop for StaleSweepApplyBarrierGuard {
    fn drop(&mut self) {
        *slot().lock().unwrap_or_else(|poison| poison.into_inner()) = self.0.take();
    }
}

pub(crate) fn install_stale_sweep_apply_barrier(
    key: &str,
    barrier: StaleSweepApplyBarrier,
) -> StaleSweepApplyBarrierGuard {
    let previous = slot()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .replace((key.to_owned(), barrier));
    StaleSweepApplyBarrierGuard(previous)
}

pub(super) async fn await_stale_sweep_apply_barrier(key: &str) {
    let barrier = slot()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .as_ref()
        .filter(|(session, _)| session.as_str() == key)
        .map(|(_, barrier)| barrier.clone());
    if let Some(barrier) = barrier {
        barrier.reached.wait().await;
        barrier.resume.wait().await;
    }
}
