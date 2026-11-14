//! What a run's claim pinned about its failures: no second turn once one was submitted, and no
//! primary-channel fallback for a thread that could not be prepared.
use super::*;

/// What is known about a failed attempt; only a submitted turn can have reached the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Submission {
    NotSent,
    Submitted,
}

/// A start failure raised after the provider start call had returned.
#[derive(Debug)]
pub(super) struct SubmittedStartError(pub(super) String);

impl std::fmt::Display for SubmittedStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SubmittedStartError {}

impl Submission {
    /// A start call that returned an error submitted nothing, unless it failed afterwards.
    pub(super) fn of_start_error(error: &anyhow::Error) -> Self {
        match error.downcast_ref::<SubmittedStartError>() {
            Some(_) => Self::Submitted,
            None => Self::NotSent,
        }
    }

    /// Whether the claim forbids any further turn after this failure.
    pub(super) async fn forbids_new_turn(self, store: &RoutineStore, run_id: &str) -> Result<bool> {
        Ok(self == Self::Submitted && store.run_forbids_retry_after_start(run_id).await?)
    }
}
