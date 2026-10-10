//! One decision for a stale protected attempt: keep the legacy path, retry the exact attempt once, or hold.

pub(crate) mod live;
#[cfg(test)]
pub(crate) mod permit;
#[cfg(test)]
mod permit_tests;
#[cfg(test)]
mod policy_tests;

use crate::db::replay_disposition::write::{self, ExactAttempt, NoEffectAck};
use crate::services::discord::inflight::InflightTurnState;
use crate::services::provider::CancelToken;
use live::{LiveAttempt, RawObservation, Verdict};
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
    let persisted = withhold(pool, &attempt, observation.hold_reason()).await;
    ReplayDecision::WithholdReplay { persisted }
}

async fn withhold(pool: &PgPool, attempt: &ExactAttempt, reason: &str) -> bool {
    let preserved = serde_json::json!({ "hold": { "nonce": attempt.nonce(), "reason": reason } });
    write::withhold(pool, attempt, reason, &preserved)
        .await
        .inspect_err(|error| {
            tracing::warn!(receipt = attempt.receipt_id(), %error, "replay hold was not committed")
        })
        .is_ok()
}

/// The right to prepare one exact retry of a committed no-effect nonce.
#[derive(Debug)]
pub(crate) struct RetryPermit {
    receipt_id: i64,
    previous_nonce: String,
    next_nonce: String,
}

impl RetryPermit {
    pub(crate) fn from_no_effect(ack: NoEffectAck) -> Self {
        let (receipt_id, previous_nonce) = ack.into_parts();
        Self {
            receipt_id,
            previous_nonce,
            next_nonce: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub(crate) fn receipt_id(&self) -> i64 {
        self.receipt_id
    }

    pub(crate) fn previous_nonce(&self) -> &str {
        &self.previous_nonce
    }

    pub(crate) fn next_nonce(&self) -> &str {
        &self.next_nonce
    }
}

/// What a stale-resume branch may do with its turn.
#[derive(Debug)]
pub(crate) enum StaleResume {
    /// No protected request: the legacy reset and history retry run unchanged.
    Legacy,
    /// Committed no-effect evidence: only this continuation may start the request again.
    Exact(RetryPermit),
    /// The body, session and request stay as they are; nothing reruns.
    Hold,
}

/// How long a decision waits for the attempt's reader to finish before judging what it saw.
const READER_SETTLE: std::time::Duration = std::time::Duration::from_secs(10);

/// Whether the turn carries a protected request; reads memory only, so a legacy turn does no IO.
pub(crate) fn protected(token: &CancelToken, inflight: &InflightTurnState) -> bool {
    token.replay_attempt().is_some()
        || inflight.replay_receipt_id.is_some()
        || inflight.replay_rerun_blocked()
}

/// Takes the token's live attempt for its single decision; any other protected turn has none.
async fn decide_live(
    pool: Option<&PgPool>,
    token: &CancelToken,
) -> Option<(&PgPool, &LiveAttempt, ExactAttempt, RawObservation)> {
    let live = token.replay_attempt()?;
    let Some(pool) = pool else {
        live.settle(Verdict::Held);
        return None;
    };
    let (attempt, observation) = live.take_for_decision(READER_SETTLE).await?;
    Some((pool, live, attempt, observation))
}

/// Decides a stale resume: a legacy turn keeps the existing retry, and a protected one retries
/// only on committed no-effect evidence from its own live attempt.
pub(crate) async fn stale_resume(
    pool: Option<&PgPool>,
    token: &CancelToken,
    inflight: &InflightTurnState,
) -> StaleResume {
    if !protected(token, inflight) {
        return StaleResume::Legacy;
    }
    let Some((pool, live, attempt, observation)) = decide_live(pool, token).await else {
        return StaleResume::Hold;
    };
    let receipt = attempt.receipt_id();
    match decide_stale(pool, Some(attempt), &observation).await {
        ReplayDecision::AllowStartupRetry(retry) => {
            live.settle(Verdict::Retried);
            StaleResume::Exact(retry)
        }
        decision => {
            live.settle(Verdict::Held);
            if matches!(decision, ReplayDecision::WithholdReplay { persisted: false }) {
                tracing::warn!(receipt, "stale protected attempt held without a committed hold");
            }
            StaleResume::Hold
        }
    }
}

/// Classifies a protected attempt's ordinary terminal before ordinary completion, failure or
/// provider recovery may run; an observation without one provider terminal holds instead.
pub(crate) async fn ordinary_terminal(
    pool: Option<&PgPool>,
    token: &CancelToken,
    inflight: &InflightTurnState,
) -> bool {
    if !protected(token, inflight) {
        return true;
    }
    if let Some(verdict) = token.replay_attempt().and_then(LiveAttempt::verdict) {
        return verdict == Verdict::Ordinary;
    }
    let Some((pool, live, attempt, observation)) = decide_live(pool, token).await else {
        return false;
    };
    let ordinary = match observation.terminal_evidence() {
        Some(evidence) => write::classify_normal(pool, &attempt, &evidence)
            .await
            .inspect_err(|error| {
                tracing::warn!(receipt = attempt.receipt_id(), kind = ?evidence.kind(), %error,
                    "ordinary terminal was not classified; the request stays held")
            })
            .is_ok(),
        None => {
            withhold(pool, &attempt, observation.hold_reason()).await;
            false
        }
    };
    live.settle(if ordinary {
        Verdict::Ordinary
    } else {
        Verdict::Held
    });
    ordinary
}

/// Whether ordinary completion and session effects may run: always for a legacy turn, and for a
/// protected one only after its ordinary classification committed.
pub(crate) fn ordinary_allowed(token: &CancelToken, inflight: &InflightTurnState) -> bool {
    !protected(token, inflight)
        || token.replay_attempt().and_then(LiveAttempt::verdict) == Some(Verdict::Ordinary)
}

/// Holds a live or projected protected request at a recovery sentinel, which is never no-effect
/// evidence; returns whether the request was protected.
pub(crate) async fn hold_recovery(
    pool: Option<&PgPool>,
    token: &CancelToken,
    inflight: &InflightTurnState,
) -> bool {
    if token.replay_attempt().is_none() && inflight.replay_receipt_id.is_none() {
        return false;
    }
    if let Some((pool, live, attempt, _)) = decide_live(pool, token).await {
        withhold(pool, &attempt, "recovery sentinel after start").await;
        live.settle(Verdict::Held);
    }
    true
}
