//! Stored-versus-current comparison for a Herdr-hosted execution, owned by the session host's
//! recover port; the stored record stays the expectation.

pub(crate) use crate::services::session_host::{HerdrExecutionMatch, HerdrMismatch, HerdrUnknown};

#[cfg(test)]
#[path = "herdr_observation_tests.rs"]
mod tests;
