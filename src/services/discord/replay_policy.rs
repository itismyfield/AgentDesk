//! Decisions for a protected attempt's stale resume or terminal: keep the legacy path, retry the
//! exact attempt once, classify it as ordinary, or hold it.

pub(crate) mod live;
#[cfg(all(test, unix))]
mod live_tests;
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

/// Admission and restart recovery compare the same process owner incarnation.
static INCARNATION: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| uuid::Uuid::new_v4().to_string());

pub(crate) fn incarnation() -> &'static str {
    &INCARNATION
}

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
    capture: Option<&InflightTurnState>,
) -> ReplayDecision {
    let Some(attempt) = attempt else {
        return ReplayDecision::NotApplicable;
    };
    if attempt.retry_of().is_none()
        && let Some(evidence) = observation.no_effect_evidence()
    {
        return match write::classify_no_effect(pool, attempt, &evidence).await {
            Ok(ack) => ReplayDecision::AllowStartupRetry(RetryPermit::from_no_effect(ack)),
            Err(_) => ReplayDecision::WithholdReplay { persisted: false },
        };
    }
    let reason = if attempt.retry_of().is_some() {
        "startup retry budget exhausted"
    } else {
        observation.hold_reason()
    };
    let persisted = withhold(pool, &attempt, reason, capture).await;
    ReplayDecision::WithholdReplay { persisted }
}

async fn withhold(
    pool: &PgPool,
    attempt: &ExactAttempt,
    reason: &str,
    capture: Option<&InflightTurnState>,
) -> bool {
    let preserved = serde_json::json!({
        "hold": { "nonce": attempt.nonce(), "reason": reason },
        "recovery_snapshot": capture,
    });
    write::withhold_and_notify(
        pool, attempt, reason, &preserved,
        crate::db::replay_disposition::hold_notice::NoticeRouting {
            bot: "notify",
            session_key: capture.and_then(|state| state.session_key.as_deref()),
        },
    )
    .await
    .inspect_err(|error| {
        tracing::warn!(receipt = attempt.receipt_id(), %error, "replay hold and notice were not committed")
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

/// The original request an exact retry starts again, and the permit only its next start may take.
#[derive(Debug)]
pub(crate) struct ExactRetry {
    pub(crate) user_message_id: poise::serenity_prelude::MessageId,
    pub(crate) sources: Vec<poise::serenity_prelude::MessageId>,
    pub(crate) request_owner: poise::serenity_prelude::UserId,
    pub(crate) text: String,
    pub(crate) reply_context: Option<String>,
    pub(crate) permit: RetryPermit,
}

/// Whether the request's next start took the exact continuation and began its attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExactHandoff {
    Started,
    /// Nothing started; the receipt keeps blocking reruns.
    Refused,
}

struct Handover {
    permit: Option<RetryPermit>,
    started: bool,
}

tokio::task_local! {
    static CONTINUATION: std::cell::RefCell<Handover>;
}

/// Runs `start` with the continuation in reach of this task's next start only.
pub(crate) async fn hand_over<F: std::future::Future>(
    permit: RetryPermit,
    start: F,
) -> (F::Output, ExactHandoff) {
    let handover = Handover {
        permit: Some(permit),
        started: false,
    };
    CONTINUATION
        .scope(std::cell::RefCell::new(handover), async {
            let output = start.await;
            let started = CONTINUATION.with(|slot| slot.borrow().started);
            let handoff = match started {
                true => ExactHandoff::Started,
                false => ExactHandoff::Refused,
            };
            (output, handoff)
        })
        .await
}

/// The continuation handed to this task's start, taken at most once.
pub(crate) fn take_continuation() -> Option<RetryPermit> {
    CONTINUATION
        .try_with(|slot| slot.borrow_mut().permit.take())
        .ok()
        .flatten()
}

/// Records that the start which took the continuation committed its begin.
pub(crate) fn continuation_started() {
    let _ = CONTINUATION.try_with(|slot| slot.borrow_mut().started = true);
}

/// What a stale-resume branch may do with its turn.
#[derive(Debug)]
pub(in crate::services::discord) enum StaleResume {
    /// No protected request, or one already classified ordinary: the legacy reset and retry run.
    Legacy,
    /// Committed no-effect evidence: only this continuation may start the request again.
    Exact(RetryPermit),
    /// The body, session and request stay as they are; nothing reruns.
    Hold,
}

/// How long a decision waits for the attempt's reader to finish before judging what it saw.
const READER_SETTLE: std::time::Duration = std::time::Duration::from_secs(10);

/// Per channel and source message, which of the legacy history retry and the protected decision
/// claimed it first; the loser never reruns the request in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryClaim {
    /// Held or handed to its exact continuation.
    Barred,
    /// A legacy history retry committed to store its history and enqueue.
    Legacy,
}

static RETRY_CLAIMS: std::sync::LazyLock<dashmap::DashMap<(u64, u64), RetryClaim>> =
    std::sync::LazyLock::new(dashmap::DashMap::new);

/// Whether the turn carries a protected request; reads memory only, so a legacy turn does no IO.
fn protected(token: &CancelToken, inflight: &InflightTurnState) -> bool {
    token.replay_attempt().is_some()
        || inflight.replay_receipt_id.is_some()
        || inflight.replay_rerun_blocked()
}

/// Whether ordinary completion, failure, recovery and session effects may run: always for a
/// legacy turn, and for a protected one only after its ordinary classification committed.
fn ordinary_allowed(token: &CancelToken, inflight: &InflightTurnState) -> bool {
    !protected(token, inflight)
        || token.replay_attempt().and_then(|live| live.verdict()) == Some(Verdict::Ordinary)
}

/// Bars the legacy history retry; true when one already committed, which keeps its claim so no
/// later decision of the request retries it exactly either.
fn bar_legacy_retry(inflight: &InflightTurnState) -> bool {
    let messages = std::iter::once(inflight.user_msg_id).chain(inflight.source_message_ids.clone());
    let claims = messages.filter(|&id| id != 0).map(|message| {
        let claim = RETRY_CLAIMS
            .entry((inflight.channel_id, message))
            .or_insert(RetryClaim::Barred);
        *claim == RetryClaim::Legacy
    });
    claims.fold(false, |raced, legacy| raced | legacy)
}

/// Whether the legacy history retry must refuse this request.
pub(crate) fn legacy_retry_barred(channel_id: u64, message_id: u64) -> bool {
    RETRY_CLAIMS
        .get(&(channel_id, message_id))
        .is_some_and(|claim| *claim == RetryClaim::Barred)
}

/// Claims the request for the legacy history retry right before its history write and enqueue;
/// false when the protected decision barred it first.
pub(crate) fn claim_legacy_retry(channel_id: u64, message_id: u64) -> bool {
    use dashmap::mapref::entry::Entry;
    match RETRY_CLAIMS.entry((channel_id, message_id)) {
        Entry::Occupied(claim) if *claim.get() == RetryClaim::Barred => false,
        entry => {
            entry.insert(RetryClaim::Legacy);
            true
        }
    }
}

mod decision;
pub(in crate::services::discord) use decision::{
    error_is_ordinary, hold_recovery, ordinary_outcome, stale_resume,
};
