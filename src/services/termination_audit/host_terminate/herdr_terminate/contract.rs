//! Operator-close proof and replies; only the parent can issue the proof.

use crate::services::session_host::{HerdrGateRefusal, HerdrTarget};

/// Explicit operator intent bound to one gate; a cancel warrant cannot convert into it.
pub(crate) struct OperatorTerminateWarrant<'a> {
    pub(super) target: HerdrTarget,
    pub(super) fence: Option<&'a crate::services::session_host::TerminationFence>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TerminateRefusal {
    Disabled,
    Gate(HerdrGateRefusal),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HerdrTerminateResult {
    Refused(TerminateRefusal),
    Acknowledged,
    NotSent(String),
    Indeterminate(String),
    ConfirmationRequired,
}

impl<'a> OperatorTerminateWarrant<'a> {
    pub(crate) fn into_parts(
        self,
    ) -> (
        HerdrTarget,
        Option<&'a crate::services::session_host::TerminationFence>,
    ) {
        (self.target, self.fence)
    }
}
