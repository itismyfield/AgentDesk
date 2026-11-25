//! The automatic end of a Claude Pending execution that never started: asked only by the front
//! that waited a due on it, it closes nothing once a start is logged or any turn holds the channel.
use std::sync::Arc;

use sqlx::PgPool;

use super::{ChannelId, OperatorTerminate, SharedData, TerminationResult, terminate_scoped};
use crate::db::dispatched_sessions::hosted_execution::{HostedExecution, HostedState};
use crate::services::discord::queue_io::input_owner::drive::Front;
use crate::services::session_host::{HerdrTarget, TerminationFence};

/// One front's request to end the execution its wait named; nothing here is an operator's word.
pub(in crate::services::discord) struct NeverStartedTerminate {
    pub(in crate::services::discord) session_key: String,
    pub(in crate::services::discord) execution_nonce: String,
    /// The native session the launch named, whose logged start means the execution started.
    pub(in crate::services::discord) session_id: String,
    pub(in crate::services::discord) front: Front,
    pub(in crate::services::discord) claim_number: u64,
}

impl NeverStartedTerminate {
    /// Only a Pending execution with launch evidence is ended this way; a Bound one never is.
    pub(super) fn admits(&self, record: &HostedExecution) -> bool {
        record.state == HostedState::Pending && record.expected.is_some()
    }

    /// Shuts the execution's fence only when the log was read and holds no start. The read and the
    /// shut share the pane's source authority with every writer of a start, so one wins whole.
    pub(super) async fn fence(
        &self,
        shared: &Arc<SharedData>,
        channel: ChannelId,
        record: &HostedExecution,
        target: &HerdrTarget,
    ) -> Result<TerminationFence, TerminationResult> {
        use TerminationResult::Refused;
        let (front, number) = (self.front.clone(), self.claim_number);
        let asked = self.execution_nonce.clone();
        let Some(mailbox) = shared.mailbox_peek(channel) else {
            return Err(Refused("claim no longer qualifies".into()));
        };
        let qualified = mailbox.v2_request(move |slot| {
            let named = slot
                .as_ref()
                .and_then(|owner| owner.never_started_close_claimed((&front, number)));
            Ok(named.as_deref() == Some(asked.as_str()))
        });
        if qualified.await != Ok(true) {
            return Err(Refused("claim no longer qualifies".into()));
        }
        let logical = record.owner.logical_key.as_str();
        crate::services::tmux_common::with_tmux_source_authority(logical, |_| {
            let (nonce, session) = (&self.execution_nonce, &self.session_id);
            let read = crate::services::claude::herdr_turn::start_log_read;
            match read(channel.get(), logical, nonce, session) {
                Ok(false) => {}
                Ok(true) => return Err(Refused("execution started".into())),
                Err(error) => return Err(Refused(format!("start log unreadable: {error}"))),
            }
            target
                .termination_fence()
                .map_err(|error| Refused(format!("{error:?}")))
        })
    }
}

/// Ends the execution `request` names if it still never started; `Retired` only once its pane and
/// provider are gone. No turn is captured or settled, and the asking drive is not woken.
pub(in crate::services::discord) async fn terminate_never_started(
    request: NeverStartedTerminate,
    shared: Arc<SharedData>,
    pool: &PgPool,
) -> TerminationResult {
    let scoped = OperatorTerminate {
        session_key: request.session_key.clone(),
        execution_nonce: request.execution_nonce.clone(),
    };
    match terminate_scoped(scoped, shared, pool, None, false, false, Some(&request)).await {
        Ok((exclusion, _wake)) => {
            drop(exclusion);
            TerminationResult::Retired
        }
        Err(result) => result,
    }
}
