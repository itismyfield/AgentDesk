use super::*;

type BoxedIntake<'a, T = ()> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<T, super::super::super::Error>> + Send + 'a>,
>;

/// Boxed inside a plain function so the turn's large future never sits in the intake frames above it.
pub(super) fn run<'a, 'b: 'a>(
    deps: &'a IntakeDeps<'b>,
    submission: IntakeSubmission,
    preserve_on_cancel: bool,
) -> BoxedIntake<'a, super::super::IntakeExecution> {
    #[cfg(all(test, unix))]
    if let Some(result) = super::super::herdr_consent::admission::tests::final_local(&submission) {
        return Box::pin(async move {
            if matches!(&result, Ok(super::super::IntakeExecution::Executed))
                && let Some(owner) = submission.owner
            {
                owner.complete_locally().await;
            }
            result
        });
    }
    let (queued_drain, queued) = match submission.origin {
        IntakeOrigin::QueuedDrain(route) => (true, route),
        _ => (false, None),
    };
    let discord_message = matches!(
        submission.origin,
        IntakeOrigin::LiveMessage | IntakeOrigin::RawAttachment
    )
    .then_some(submission.request.user_msg_id);
    Box::pin(message_handler::finish_admitted_local(
        deps,
        submission.request,
        preserve_on_cancel,
        (queued_drain, queued, submission.owner),
        submission.preloaded_uploads,
        discord_message,
    ))
}
