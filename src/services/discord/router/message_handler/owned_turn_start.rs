//! The managed birth paired with an owned queue attempt; neither callback follows the channel head.
use crate::services::claude_tui::{busy_inject::Outcome, owner_input::NotSentProof};
use crate::services::discord::inflight::{self, InflightTurnState, ManagedSubmission};
use crate::services::turn_host::TurnHost;
use crate::services::turn_orchestrator::ChannelMailboxHandle;
use serde_json::Value;

/// Where the PG owner start permit is spent once its junction lands: `prepare` before the leaf,
/// `submit(frame, native_binding, raw)` around its first payload. [`NoStartPermit`] runs as main.
pub(crate) trait StartPermit {
    fn submit<T>(self, frame: &str, native_binding: Option<&Value>, raw: impl FnOnce() -> T) -> T;
}

/// No owner start junction: the leaf runs unwrapped.
pub(crate) struct NoStartPermit;

impl StartPermit for NoStartPermit {
    fn submit<T>(
        self,
        _frame: &str,
        _native_binding: Option<&Value>,
        raw: impl FnOnce() -> T,
    ) -> T {
        raw()
    }
}

pub(crate) struct OwnedTurnStart {
    submission: ManagedSubmission,
    key: Value,
    ran: bool,
    ended: bool,
}

impl OwnedTurnStart {
    pub(crate) fn install(
        state: &mut InflightTurnState,
        host: &TurnHost,
        key: Value,
    ) -> Result<Self, String> {
        if key["identity"]["primary"].as_u64() != Some(state.user_msg_id)
            || key["attempt"]["nonce"].as_str() != state.turn_nonce.as_deref()
            || key["attempt"]["provider"].as_str() != Some(state.provider.as_str())
            || key["attempt"]["target"]["owner"].as_str() != state.tmux_session_name.as_deref()
        {
            return Err("queue attempt does not name this managed birth".into());
        }
        let submission = inflight::install_managed_submission_boundary(state, host)
            .ok_or("this turn has no managed submission boundary")?;
        Ok(Self {
            submission,
            key,
            ran: false,
            ended: false,
        })
    }

    pub(crate) fn key(&self) -> &Value {
        &self.key
    }

    pub(crate) fn fence(
        &self,
    ) -> std::sync::Arc<dyn crate::services::claude_tui::submission_fence::SubmissionFence> {
        self.submission.fence()
    }

    /// The actual leaf supplies the result; only its live no-content proof ends this birth.
    pub(crate) fn run(
        &mut self,
        permit: impl StartPermit,
        frame: &str,
        leaf: impl FnOnce() -> Outcome,
    ) -> Result<Outcome, String> {
        if self.ran {
            return Err("owned start already spent".into());
        }
        self.ran = true;
        let (outcome, proof) = self
            .submission
            .run_leaf(|| permit.submit(frame, None, leaf));
        if let Some(proof) = proof {
            self.submission.conclude_not_submitted(proof)?;
            self.ended = true;
        }
        Ok(outcome)
    }

    /// The leaf has returned its writer before this handoff. Both durable attempts keep the
    /// original; an incomplete handoff returns no capability and never re-admits it.
    pub(crate) async fn successor(
        &self,
        handle: &ChannelMailboxHandle,
        proof: NotSentProof,
    ) -> Result<Self, String> {
        if !self.ended {
            return Err("owned start has not ended".into());
        }
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let next_nonce = nonce.clone();
        let key = self.key.clone();
        let next = handle
            .v2_request(move |slot| {
                let owner = slot.as_mut().ok_or("no input owner")?;
                let key = serde_json::from_value(key).map_err(|e| e.to_string())?;
                let successor = owner.successor_not_sent(&key, proof, next_nonce)?;
                serde_json::to_value(successor).map_err(|e| e.to_string())
            })
            .await?;
        let submission = self.submission.successor(nonce)?;
        Ok(Self {
            submission,
            key: next,
            ran: false,
            ended: false,
        })
    }
}

#[cfg(unix)]
#[path = "owned_turn_start_tests.rs"]
mod tests;
