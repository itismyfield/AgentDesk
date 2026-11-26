#[cfg(test)]
use crate::domain::campaign::evidence::*;
#[cfg(test)]
use crate::domain::campaign::evidence_gate::GateReason;
#[cfg(test)]
use crate::domain::campaign::model::{CampaignState, Role};
#[cfg(test)]
use crate::domain::campaign::verification_run::{VerificationCollection, VerificationRun};

#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;
