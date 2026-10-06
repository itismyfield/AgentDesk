//! Transient agent-start failures (provider runtime not registered yet, busy mailbox) park the
//! run as `deferred` on a short cadence instead of failing it, until a bounded window closes.
use super::*;
use crate::services::discord::HeadlessTurnStartError;

/// Delay between deferred start attempts.
pub(super) const START_DEFER_RETRY_SECS: i64 = 60;
/// Window opened by the first deferred attempt; a transient failure after it counts as a failure.
pub(super) const START_DEFER_WINDOW_SECS: i64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransientStartKind {
    ProviderNotReady,
    MailboxBusy,
}

impl TransientStartKind {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::ProviderNotReady => "provider_not_ready",
            Self::MailboxBusy => "mailbox_busy",
        }
    }
}

/// Agent-start error the executor re-attempts on the deferral cadence.
#[derive(Debug)]
pub(crate) struct TransientStartError {
    pub(crate) kind: TransientStartKind,
    message: String,
}

impl std::fmt::Display for TransientStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TransientStartError {}

/// Checked before any thread, reservation or run-row side effect, so a runtime that has not
/// registered yet (right after a restart) defers the run instead of failing it.
pub(super) async fn ensure_provider_registered(
    registry: &HealthRegistry,
    provider: &ProviderKind,
) -> Result<()> {
    if !registry.registered_token_hashes(provider).await.is_empty() {
        return Ok(());
    }
    Err(TransientStartError {
        kind: TransientStartKind::ProviderNotReady,
        message: format!("provider runtime not registered: {}", provider.as_str()),
    }
    .into())
}

pub(crate) fn classify_headless_start_error(
    error: &HeadlessTurnStartError,
) -> Option<TransientStartKind> {
    match error {
        HeadlessTurnStartError::Conflict(_) => Some(TransientStartKind::MailboxBusy),
        error if error.is_original_start_deferred() => Some(TransientStartKind::MailboxBusy),
        HeadlessTurnStartError::Internal(message)
            if crate::services::scheduled_messages::is_runtime_unavailable_message(message) =>
        {
            Some(TransientStartKind::ProviderNotReady)
        }
        HeadlessTurnStartError::Internal(_) | HeadlessTurnStartError::InvalidTarget(_) => None,
    }
}

/// Wraps a headless start error, keeping busy-mailbox (409) and not-ready runtimes retryable.
pub(super) fn headless_start_error(agent_id: &str, error: HeadlessTurnStartError) -> anyhow::Error {
    let message = format!("start routine agent turn for {agent_id}: {error}");
    match classify_headless_start_error(&error) {
        Some(kind) => TransientStartError { kind, message }.into(),
        None => anyhow!(message),
    }
}

fn deferred(result_json: Option<&Value>) -> Option<&Value> {
    result_json.filter(|result| result.get("status").and_then(Value::as_str) == Some("deferred"))
}

/// Attempt kind and DM target for re-starting a running agent run: a deferred run repeats its
/// own start, any other pending run is a plain retry.
pub(super) fn reattempt(result_json: Option<&Value>) -> (String, Option<String>) {
    let Some(result) = deferred(result_json) else {
        return ("retry".to_string(), None);
    };
    let text = |key: &str| result.get(key).and_then(Value::as_str).map(str::to_string);
    let attempt_kind = text("attempt_kind").unwrap_or_else(|| "primary".to_string());
    (attempt_kind, text("dm_user_id"))
}

/// `(deferred_since, next_retry_at)` while the window opened by the first deferral is open.
pub(super) fn start_deferral(
    prior_result: Option<&Value>,
    now: DateTime<Utc>,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let since = deferred(prior_result)
        .and_then(|result| result.get("deferred_since"))
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map_or(now, |value| value.with_timezone(&Utc));
    (now.signed_duration_since(since) < Duration::seconds(START_DEFER_WINDOW_SECS))
        .then(|| (since, now + Duration::seconds(START_DEFER_RETRY_SECS)))
}

/// One agent start attempt, as needed to re-attempt it after a deferral.
pub(super) struct PendingAgentStart<'a> {
    pub(super) claimed: &'a ClaimedRoutineRun,
    pub(super) agent_id: &'a str,
    pub(super) attempt_kind: &'a str,
    pub(super) prompt: &'a str,
    pub(super) dm_user_id: Option<&'a str>,
    pub(super) checkpoint: &'a Option<Value>,
    pub(super) next_due_at: Option<DateTime<Utc>>,
}

impl RoutineAgentExecutor {
    /// Parks the run as `deferred(<reason>)` when `error` is transient and the window is open;
    /// `None` hands the error to the regular retry/fallback/fail policy.
    pub(super) async fn defer_transient_start(
        &self,
        start: PendingAgentStart<'_>,
        prior_result: Option<&Value>,
        error: &anyhow::Error,
    ) -> Result<Option<RoutineRunOutcome>> {
        let Some(kind) = error
            .downcast_ref::<TransientStartError>()
            .map(|error| error.kind)
        else {
            return Ok(None);
        };
        let Some((since, next_retry_at)) = start_deferral(prior_result, Utc::now()) else {
            return Ok(None);
        };
        let claimed = start.claimed;
        let message = error.to_string();
        let result_json = json!({
            "status": "deferred",
            "deferred_reason": kind.reason(),
            "deferred_since": since.to_rfc3339(),
            "next_retry_at": next_retry_at.to_rfc3339(),
            "error": message,
            "agent_id": start.agent_id,
            "attempt_kind": start.attempt_kind,
            "prompt": start.prompt,
            "dm_user_id": start.dm_user_id,
            "routine_id": claimed.routine_id,
            "run_id": claimed.run_id,
            "script_ref": claimed.script_ref,
            "fresh_context_guaranteed": false,
            "checkpoint": start.checkpoint,
            "next_due_at": start.next_due_at.map(|value| value.to_rfc3339()),
        });
        // Same re-arm as a scheduled retry, except that `retry_count` is left untouched.
        let deferred = sqlx::query(
            r#"
            UPDATE routine_runs
            SET action = 'agent',
                turn_id = NULL,
                next_retry_at = $2,
                result_json = $3,
                error = $4,
                owned_tmux_session = NULL,
                lease_expires_at = NOW() + ($5::bigint * INTERVAL '1 second'),
                attempts = COALESCE(attempts, '[]'::jsonb) || jsonb_build_array(
                    jsonb_build_object(
                        'event', 'deferred',
                        'reason', $8,
                        'agent_id', $6,
                        'kind', $7,
                        'error', $4,
                        'next_retry_at', $2,
                        'at', NOW()
                    )
                ),
                updated_at = NOW()
            WHERE id = $1
              AND status = 'running'
            "#,
        )
        .bind(&claimed.run_id)
        .bind(next_retry_at)
        .bind(&result_json)
        .bind(&message)
        .bind(super::super::store::ROUTINE_RUN_LEASE_SECS as i64)
        .bind(start.agent_id)
        .bind(start.attempt_kind)
        .bind(kind.reason())
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("defer routine agent start {}: {e}", claimed.run_id))?;
        if deferred.rows_affected() != 1 {
            return Err(anyhow!(
                "routine agent run {} was already closed before start deferral",
                claimed.run_id
            ));
        }
        tracing::info!(
            routine_id = %claimed.routine_id,
            run_id = %claimed.run_id,
            reason = kind.reason(),
            next_retry_at = %next_retry_at,
            "routine agent start deferred"
        );
        Ok(Some(RoutineRunOutcome {
            run_id: claimed.run_id.clone(),
            routine_id: claimed.routine_id.clone(),
            script_ref: claimed.script_ref.clone(),
            action: "agent".to_string(),
            status: "deferred".to_string(),
            result_json: Some(result_json),
            error: Some(message),
            fresh_context_guaranteed: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_mailbox_and_unready_runtime_starts_are_deferred_not_failed() {
        let busy = HeadlessTurnStartError::Conflict("agent mailbox is busy for channel 1".into());
        assert_eq!(
            classify_headless_start_error(&busy),
            Some(TransientStartKind::MailboxBusy)
        );
        let unregistered =
            HeadlessTurnStartError::Internal("provider runtime not registered: claude".into());
        assert_eq!(
            classify_headless_start_error(&unregistered),
            Some(TransientStartKind::ProviderNotReady)
        );
        for permanent in [
            HeadlessTurnStartError::Internal("tmux spawn failed".into()),
            HeadlessTurnStartError::InvalidTarget("channel 1 is not a thread".into()),
        ] {
            assert_eq!(classify_headless_start_error(&permanent), None);
        }

        let error = headless_start_error("agent-a", busy);
        let transient = error.downcast_ref::<TransientStartError>().expect("typed");
        assert_eq!(transient.kind.reason(), "mailbox_busy");
        assert!(
            error.to_string().contains("agent mailbox is busy"),
            "{error}"
        );
    }

    #[test]
    fn a_start_held_back_by_a_watcher_recovery_is_deferred_not_failed() {
        let error =
            headless_start_error("agent-a", HeadlessTurnStartError::original_start_deferred());
        let transient = error
            .downcast_ref::<TransientStartError>()
            .expect("a deferred original start must stay retryable");
        assert_eq!(transient.kind, TransientStartKind::MailboxBusy);
    }
}
