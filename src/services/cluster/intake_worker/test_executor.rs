//! Test builds may stand in for the external TUI turn; every outbox transition around it stays real.
use crate::services::discord::IntakeRequest;
use std::cell::RefCell;

thread_local! {
    static RUNS: RefCell<Option<Vec<u64>>> = const { RefCell::new(None) };
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
        return Ok(());
    }
    crate::services::discord::execute_intake_turn_core(http, shared, token, request, uploads).await
}
