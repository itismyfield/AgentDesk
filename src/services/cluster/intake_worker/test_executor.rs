//! Test builds may stand in for the external TUI turn; every outbox transition around it stays real.
//! The stand-in executor and its port live with the discord-layer tests; these hooks stay beside the worker.
use std::cell::RefCell;

thread_local! {
    pub(crate) static RUNS: RefCell<Option<Vec<u64>>> = const { RefCell::new(None) };
    pub(crate) static FAIL: RefCell<bool> = const { RefCell::new(false) };
    static CHECKPOINT: RefCell<Option<CheckpointHook>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Checkpoint {
    /// The claim picked its row and has not confirmed it yet.
    ClaimSelected,
    AfterClaim,
    PreAccept,
    Spawned,
    FinalDb,
    FinalDbDone,
}
type CheckpointHook = Box<dyn FnMut(Checkpoint) -> futures::future::BoxFuture<'static, ()>>;
pub(crate) struct Hook;
pub fn hook(callback: CheckpointHook) -> Hook {
    CHECKPOINT.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(callback);
    });
    Hook
}
impl Drop for Hook {
    fn drop(&mut self) {
        CHECKPOINT.with(|slot| *slot.borrow_mut() = None);
    }
}
pub(super) async fn checkpoint(point: Checkpoint) {
    let pending = CHECKPOINT.with(|slot| slot.borrow_mut().as_mut().map(|hook| hook(point)));
    if let Some(pending) = pending {
        pending.await;
    }
}
/// The claim's checkpoint between its SELECT and its confirming UPDATE.
pub(crate) async fn claim_selected() {
    checkpoint(Checkpoint::ClaimSelected).await;
}
pub fn fail_execution() {
    FAIL.with(|fail| *fail.borrow_mut() = true);
}

/// While alive, each executed turn records its channel instead of starting a TUI turn.
pub(crate) struct Recorder;

pub fn record() -> Recorder {
    RUNS.with(|runs| *runs.borrow_mut() = Some(Vec::new()));
    Recorder
}

impl Recorder {
    pub fn channels(&self) -> Vec<u64> {
        RUNS.with(|runs| runs.borrow().clone().unwrap_or_default())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        RUNS.with(|runs| *runs.borrow_mut() = None);
        FAIL.with(|fail| *fail.borrow_mut() = false);
    }
}

/// No channel's checkpoint waits in the worker's own unit tests: they read rows as a dormant
/// channel's worker does, which is what the router's check answers until a channel is activated.
pub(crate) const CHECKPOINT_WAITS: super::CheckpointWaits = |_| false;
