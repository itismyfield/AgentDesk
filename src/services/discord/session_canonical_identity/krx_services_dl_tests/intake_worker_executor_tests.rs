//! Stand-in for the discord intake executor while a `Recorder` lives; moved here from the worker's
//! test rig because it runs the discord-layer executor.

use crate::services::cluster::intake_worker::test_executor;
use crate::services::discord::IntakeRequest;

pub(crate) const PORT: crate::services::cluster::intake_worker::IntakeTurnPort =
    crate::services::cluster::intake_worker::IntakeTurnPort {
        checkpoint_waits: test_executor::CHECKPOINT_WAITS,
        execute: boxed_execute,
    };

fn boxed_execute<'a>(
    http: &'a std::sync::Arc<serenity::http::Http>,
    shared: &'a std::sync::Arc<crate::services::discord::SharedData>,
    token: &'a str,
    request: IntakeRequest,
    uploads: crate::services::cluster::attachment_transfer::uploads::PendingUploads,
) -> crate::services::cluster::intake_worker::IntakeTurnFuture<'a> {
    Box::pin(execute_intake_turn_core(
        http, shared, token, request, uploads,
    ))
}

/// Stands in for the executor while a `Recorder` lives, otherwise runs the real one.
pub(crate) async fn execute_intake_turn_core(
    http: &std::sync::Arc<serenity::http::Http>,
    shared: &std::sync::Arc<crate::services::discord::SharedData>,
    token: &str,
    request: IntakeRequest,
    uploads: crate::services::cluster::attachment_transfer::uploads::PendingUploads,
) -> Result<crate::services::discord::IntakeExecution, Box<dyn std::error::Error + Send + Sync>> {
    let recorded = test_executor::RUNS.with(|runs| {
        let mut runs = runs.borrow_mut();
        runs.as_mut()
            .map(|runs| runs.push(request.channel_id.get()))
    });
    if recorded.is_some() {
        if test_executor::FAIL.with(|fail| *fail.borrow()) {
            return Err(std::io::Error::other("C1 executor fixture failure").into());
        }
        return Ok(crate::services::discord::IntakeExecution::Executed);
    }
    crate::services::discord::execute_intake_turn_core(http, shared, token, request, uploads).await
}
