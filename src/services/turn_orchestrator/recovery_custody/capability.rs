//! A limited right to apply one armed stage of one operation. Only the custody core issues it, after
//! the stage's PG arm committed; nothing converts an input permit, PID or reference into one.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::operation::Stage;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CapabilityRefusal {
    Revoked,
    WrongTarget,
}

#[derive(Debug, Default)]
struct Revocation(AtomicBool);

/// Not `Clone`: one applier holds it, and the core's [`RevokeHandle`] can end it.
#[derive(Debug)]
pub(crate) struct ApplyCapability {
    channel_id: String,
    operation_id: String,
    stage: Stage,
    arm_id: String,
    revocation: Arc<Revocation>,
}

#[derive(Clone, Debug)]
pub(crate) struct RevokeHandle(Arc<Revocation>);

impl RevokeHandle {
    pub(crate) fn revoke(&self) {
        self.0.0.store(true, Ordering::SeqCst);
    }
}

impl ApplyCapability {
    pub(super) fn issue(
        channel_id: &str,
        operation_id: &str,
        stage: Stage,
        arm_id: &str,
    ) -> (Self, RevokeHandle) {
        let revocation = Arc::new(Revocation::default());
        let capability = Self {
            channel_id: channel_id.to_string(),
            operation_id: operation_id.to_string(),
            stage,
            arm_id: arm_id.to_string(),
            revocation: revocation.clone(),
        };
        (capability, RevokeHandle(revocation))
    }

    pub(crate) fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub(crate) fn stage(&self) -> Stage {
        self.stage
    }

    /// The arm that issued it; another arm of the same operation and stage is another target.
    pub(crate) fn arm_id(&self) -> &str {
        &self.arm_id
    }

    /// Admits the apply only for its own channel, operation and stage while not revoked.
    pub(crate) fn check(
        &self,
        channel_id: &str,
        operation_id: &str,
        stage: Stage,
    ) -> Result<(), CapabilityRefusal> {
        if self.revocation.0.load(Ordering::SeqCst) {
            return Err(CapabilityRefusal::Revoked);
        }
        if (channel_id, operation_id, stage)
            != (
                self.channel_id.as_str(),
                self.operation_id.as_str(),
                self.stage,
            )
        {
            return Err(CapabilityRefusal::WrongTarget);
        }
        Ok(())
    }
}
