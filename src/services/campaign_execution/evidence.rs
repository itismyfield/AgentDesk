pub use crate::domain::campaign::evidence::*;

#[cfg(test)]
use super::evidence_gate::GateReason;
#[cfg(test)]
use super::model::{CampaignState, Role};
#[cfg(test)]
use super::verification_run::{VerificationCollection, VerificationRun};

#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;
