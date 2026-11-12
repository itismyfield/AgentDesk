pub use crate::domain::campaign::profile::*;

#[cfg(test)]
use super::definition::{AcceptanceCondition, DefinitionHashes, PROFILE_VERSION, profile_hash};
#[cfg(test)]
use super::model::{AttemptBinding, ExecutionReadiness, Repository, Role, Stage};
#[cfg(test)]
use chrono::{DateTime, Utc};

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
