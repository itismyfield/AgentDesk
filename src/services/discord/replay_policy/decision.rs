use super::*;

/// Takes the token's live attempt for its one decision; a protected turn without one decides nothing.
async fn decide_live<'a>(
    pool: Option<&'a PgPool>,
    token: &'a CancelToken,
    terminal: bool,
) -> Option<(&'a PgPool, &'a LiveAttempt, ExactAttempt, RawObservation)> {
    let live = token.replay_attempt()?.as_ref();
    let Some(pool) = pool else {
        live.settle(Verdict::Held);
        return None;
    };
    let (attempt, observation) = if terminal {
        live.take_for_terminal(READER_SETTLE).await?
    } else {
        live.take_for_decision(READER_SETTLE).await?
    };
    Some((pool, live, attempt, observation))
}

/// Decides a stale resume: the legacy retry for a legacy or ordinary turn, and for a protected
/// one an exact retry only on committed no-effect evidence from its own live attempt.
pub(in crate::services::discord) async fn stale_resume(
    pool: Option<&PgPool>,
    token: &CancelToken,
    inflight: &InflightTurnState,
) -> StaleResume {
    if ordinary_allowed(token, inflight) {
        return StaleResume::Legacy;
    }
    let legacy_raced = bar_legacy_retry(inflight);
    let Some((pool, live, attempt, observation)) = decide_live(pool, token, false).await else {
        return StaleResume::Hold;
    };
    let receipt = attempt.receipt_id();
    if legacy_raced {
        withhold(
            pool,
            &attempt,
            "legacy retry claimed the request",
            Some(inflight),
        )
        .await;
        live.settle(Verdict::Held);
        return StaleResume::Hold;
    }
    match decide_stale(pool, Some(attempt), &observation, Some(inflight)).await {
        ReplayDecision::AllowStartupRetry(retry) => {
            live.settle(Verdict::Retried);
            StaleResume::Exact(retry)
        }
        decision => {
            live.settle(Verdict::Held);
            if matches!(
                decision,
                ReplayDecision::WithholdReplay { persisted: false }
            ) {
                tracing::warn!(
                    receipt,
                    "stale protected attempt held without a committed hold"
                );
            }
            StaleResume::Hold
        }
    }
}

/// Classifies a protected attempt's terminal from a complete observation with exactly one
/// provider terminal; anything else holds.
async fn classify_terminal(
    pool: Option<&PgPool>,
    token: &CancelToken,
    inflight: &InflightTurnState,
) -> bool {
    if !protected(token, inflight) {
        return true;
    }
    if let Some(verdict) = token.replay_attempt().and_then(|live| live.verdict()) {
        return verdict == Verdict::Ordinary;
    }
    let unmatched_stale = token.replay_attempt().is_some_and(|live| live.stale());
    let Some((pool, live, attempt, observation)) = decide_live(pool, token, !unmatched_stale).await
    else {
        return false;
    };
    let ordinary = match (unmatched_stale, observation.terminal_evidence()) {
        (false, Some(evidence)) => write::classify_normal(pool, &attempt, &evidence)
            .await
            .inspect_err(|error| {
                tracing::warn!(receipt = attempt.receipt_id(), kind = ?evidence.kind(), %error,
                    "ordinary terminal was not classified; the request stays held")
            })
            .is_ok(),
        _ => {
            let reason = if unmatched_stale {
                "reader stale without a bridge decision"
            } else {
                observation.hold_reason()
            };
            withhold(pool, &attempt, reason, Some(inflight)).await;
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

/// Whether a provider error may take the ordinary recovery, presentation and reset path; a
/// protected stale resume is left to the stale-resume decision.
pub(in crate::services::discord) async fn error_is_ordinary(
    pool: Option<&PgPool>,
    token: &CancelToken,
    inflight: &InflightTurnState,
    stale_resume: bool,
) -> bool {
    if stale_resume && !ordinary_allowed(token, inflight) {
        token.replay_attempt().inspect(|live| live.mark_stale());
        bar_legacy_retry(inflight);
        return false;
    }
    ordinary_outcome(pool, token, inflight).await
}

/// Whether ordinary completion, failure and session effects may run: always for a legacy turn,
/// and for a protected one once its terminal is classified ordinary.
pub(in crate::services::discord) async fn ordinary_outcome(
    pool: Option<&PgPool>,
    token: &CancelToken,
    inflight: &InflightTurnState,
) -> bool {
    let ordinary = classify_terminal(pool, token, inflight).await;
    if !ordinary {
        bar_legacy_retry(inflight);
    }
    ordinary
}

/// Holds a protected request at a recovery sentinel, which is never no-effect evidence; returns
/// whether the request was held.
pub(in crate::services::discord) async fn hold_recovery(
    pool: Option<&PgPool>,
    token: &CancelToken,
    inflight: &InflightTurnState,
) -> bool {
    if ordinary_allowed(token, inflight) {
        return false;
    }
    bar_legacy_retry(inflight);
    if let Some((pool, live, attempt, _)) = decide_live(pool, token, false).await {
        withhold(
            pool,
            &attempt,
            "recovery sentinel after start",
            Some(inflight),
        )
        .await;
        live.settle(Verdict::Held);
    }
    true
}
