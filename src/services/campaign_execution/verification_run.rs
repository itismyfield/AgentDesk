pub use crate::domain::campaign::verification_run::*;

#[cfg(test)]
use super::model::{Action, ActionKind, ActionState, AttemptBinding, HostGate, Repository, Stage};
#[cfg(test)]
use super::profile::{
    Candidate, EnvironmentComponent, ExecutedTest, ResourceIdentity, TestOutcome,
};
#[cfg(test)]
use std::collections::BTreeMap;

#[cfg(any(test, feature = "test-support"))]
#[path = "verification_run_test_support.rs"]
pub mod test_support;

#[cfg(test)]
#[path = "verification_run_tests.rs"]
pub(crate) mod tests;
