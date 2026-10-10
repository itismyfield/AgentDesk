//! Test builds may stand in for the external TUI turn; every outbox transition around it stays real.
use crate::services::discord::IntakeRequest;
use std::cell::RefCell;

thread_local! {
    static RUNS: RefCell<Option<Vec<u64>>> = const { RefCell::new(None) };
    static FAIL: RefCell<bool> = const { RefCell::new(false) };
    static CHECKPOINT: RefCell<Option<CheckpointHook>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Checkpoint {
    /// The claim picked its row and has not confirmed it yet.
    ClaimSelected,
    AfterClaim,
    PreAccept,
    FinalDb,
    FinalDbDone,
}
type CheckpointHook = Box<dyn FnMut(Checkpoint) -> futures::future::BoxFuture<'static, ()>>;
pub(super) struct Hook;
pub(super) fn hook(callback: CheckpointHook) -> Hook {
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
pub(super) fn fail_execution() {
    FAIL.with(|fail| *fail.borrow_mut() = true);
}

/// While alive, each executed turn records its channel instead of starting a TUI turn.
pub(crate) struct Recorder;

pub(crate) fn record() -> Recorder {
    RUNS.with(|runs| *runs.borrow_mut() = Some(Vec::new()));
    Recorder
}

impl Recorder {
    pub(crate) fn channels(&self) -> Vec<u64> {
        RUNS.with(|runs| runs.borrow().clone().unwrap_or_default())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        RUNS.with(|runs| *runs.borrow_mut() = None);
        FAIL.with(|fail| *fail.borrow_mut() = false);
    }
}

/// Stands in for the executor while a `Recorder` lives, otherwise runs the real one.
pub(crate) async fn execute_intake_turn_core(
    http: &std::sync::Arc<serenity::http::Http>,
    shared: &std::sync::Arc<crate::services::discord::SharedData>,
    token: &str,
    request: IntakeRequest,
    uploads: crate::services::cluster::attachment_transfer::uploads::PendingUploads,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let recorded = RUNS.with(|runs| {
        let mut runs = runs.borrow_mut();
        runs.as_mut()
            .map(|runs| runs.push(request.channel_id.get()))
    });
    if recorded.is_some() {
        if FAIL.with(|fail| *fail.borrow()) {
            return Err(std::io::Error::other("C1 executor fixture failure").into());
        }
        return Ok(());
    }
    crate::services::discord::execute_intake_turn_core(http, shared, token, request, uploads).await
}
