//! Opaque host-close proof and reply data; issuer policy stays with the parent.

use std::borrow::Cow;

#[cfg(unix)]
use super::herdr_terminate::{HerdrTerminateResult, OperatorTerminateWarrant};
use crate::services::session_host::{HostKind, HostRefusal, HostedRuntimeLocator};

/// Operation name carried by the `Unsupported` refusal.
pub(crate) const TERMINATE_OP: &str = "terminate";

/// Proof that the existing owners cleared this exact target. Not a lease or a
/// new authority: it only carries their verdicts and is consumed by one call.
pub struct HostTerminateWarrant<'a> {
    pub(super) proof: TerminateProof<'a>,
}

pub(super) enum TerminateProof<'a> {
    Identity(Cow<'a, HostedRuntimeLocator>),
    #[cfg(unix)]
    Operator(OperatorTerminateWarrant<'a>),
}

impl HostTerminateWarrant<'_> {
    pub(crate) fn kind(&self) -> HostKind {
        match &self.proof {
            TerminateProof::Identity(target) => target.host_kind,
            #[cfg(unix)]
            TerminateProof::Operator(_) => HostKind::Herdr,
        }
    }
}
#[cfg(unix)]
impl<'a> HostTerminateWarrant<'a> {
    pub(crate) fn from_operator(warrant: OperatorTerminateWarrant<'a>) -> Self {
        Self {
            proof: TerminateProof::Operator(warrant),
        }
    }

    pub(crate) fn into_operator(self) -> Option<OperatorTerminateWarrant<'a>> {
        match self.proof {
            TerminateProof::Operator(warrant) => Some(warrant),
            TerminateProof::Identity(_) => None,
        }
    }
}

/// A physical close reply retains every detail; only the service can confirm disappearance.
#[derive(Debug, PartialEq, Eq)]
pub enum HostTerminateEffect {
    Refused(HostRefusal),
    #[cfg(unix)]
    Close(HerdrTerminateResult),
}
