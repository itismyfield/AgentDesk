#[cfg(test)]
use super::verification_run::{VerificationCollection, VerificationRun};
#[cfg(test)]
use crate::domain::campaign::evidence::*;
#[cfg(test)]
use crate::domain::campaign::evidence_gate::GateReason;
#[cfg(test)]
use crate::domain::campaign::model::{CampaignState, Role};

#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;
