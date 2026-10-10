//! One decision for a stale protected attempt: keep the legacy path, retry the exact attempt once, or hold.

pub(crate) mod live;
pub(crate) mod permit;
#[cfg(test)]
mod permit_tests;
#[cfg(test)]
mod policy_tests;

use crate::db::replay_disposition::write::{self, ExactAttempt};
use live::RawObservation;
use permit::RetryPermit;
use sqlx::PgPool;

#[derive(Debug)]
pub(crate) enum ReplayDecision {
    /// No protected attempt is attached; the existing path runs unchanged.
    NotApplicable,
    AllowStartupRetry(RetryPermit),
    /// The attempt keeps blocking reruns; `persisted` says whether the hold itself was acknowledged.
    WithholdReplay {
        persisted: bool,
    },
}

/// Decides a stale attempt; anything short of committed no-effect evidence withholds.
pub(crate) async fn decide_stale(
    pool: &PgPool,
    attempt: Option<ExactAttempt>,
    observation: &RawObservation,
) -> ReplayDecision {
    let Some(attempt) = attempt else {
        return ReplayDecision::NotApplicable;
    };
    if let Some(evidence) = observation.no_effect_evidence() {
        return match write::classify_no_effect(pool, attempt, &evidence).await {
            Ok(ack) => ReplayDecision::AllowStartupRetry(RetryPermit::from_no_effect(ack)),
            Err(_) => ReplayDecision::WithholdReplay { persisted: false },
        };
    }
    let reason = observation.hold_reason();
    let preserved = serde_json::json!({ "hold": { "nonce": attempt.nonce(), "reason": reason } });
    let persisted = write::withhold(pool, &attempt, reason, &preserved)
        .await
        .is_ok();
    ReplayDecision::WithholdReplay { persisted }
}
