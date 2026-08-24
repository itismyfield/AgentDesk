//! Test-only synchronization at the stale-turn candidate/apply seam.
//!
//! The module is always registered, matching the manual-rebind precedent, while
//! every item and every caller is `cfg(test)`. Production control flow therefore
//! contains no barrier branch.
//!
//! The slot is process-global. Every current installer holds a
//! `TestPostgresDb`, whose lifecycle guard serializes PostgreSQL tests for the
//! whole database lifetime; the curated PG lane also uses `--test-threads=1`.
//! A future non-PG installer must add its own serialization before sharing this
//! slot, or concurrent installs can replace the barrier another test awaits.

#[cfg(test)]
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
pub(crate) struct StaleSweepApplyBarrierPoint(tokio::sync::Barrier);

#[cfg(test)]
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

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct StaleSweepApplyBarrier {
    pub(crate) reached: std::sync::Arc<StaleSweepApplyBarrierPoint>,
    pub(crate) resume: std::sync::Arc<StaleSweepApplyBarrierPoint>,
}

#[cfg(test)]
fn slot() -> &'static Mutex<Option<StaleSweepApplyBarrier>> {
    static SLOT: OnceLock<Mutex<Option<StaleSweepApplyBarrier>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
pub(crate) struct StaleSweepApplyBarrierGuard(Option<StaleSweepApplyBarrier>);

#[cfg(test)]
impl Drop for StaleSweepApplyBarrierGuard {
    fn drop(&mut self) {
        *slot().lock().unwrap_or_else(|poison| poison.into_inner()) = self.0.take();
    }
}

#[cfg(test)]
pub(crate) fn install_stale_sweep_apply_barrier(
    barrier: StaleSweepApplyBarrier,
) -> StaleSweepApplyBarrierGuard {
    let previous = slot()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .replace(barrier);
    StaleSweepApplyBarrierGuard(previous)
}

#[cfg(test)]
pub(super) async fn await_stale_sweep_apply_barrier() {
    let barrier = slot()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if let Some(barrier) = barrier {
        barrier.reached.wait().await;
        barrier.resume.wait().await;
    }
}
