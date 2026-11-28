//! Routine reliability helpers: completion evidence, fresh-session liveness, and attempt timing.
use super::*;

pub(super) const FRESH_PROVIDER_SESSION_LIVENESS_GRACE_SECS: i64 = 120;

pub(super) fn current_attempt_started_at(run: &RunningAgentRoutineRun) -> DateTime<Utc> {
    run.attempts
        .as_array()
        .into_iter()
        .flat_map(|attempts| attempts.iter().rev())
        .filter(|attempt| {
            attempt
                .get("event")
                .and_then(Value::as_str)
                .is_some_and(|event| event == "started")
        })
        .find_map(|attempt| {
            attempt
                .get("at")
                .and_then(Value::as_str)
                .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.with_timezone(&Utc))
        })
        .unwrap_or(run.started_at)
}
pub(crate) fn provider_error_from_completion(completion: &AgentTurnCompletion) -> Option<String> {
    if !completion.evidence.confirms_assistant_delivery() {
        return None;
    }
    let message = completion.assistant_message.as_deref()?;
    is_strong_provider_error_transcript(message).then(|| assistant_preview(message))
}
/// A terminal `turn_error` waits this long so a transcript persisted right after
/// the quality event still completes the run.
pub(super) const TERMINAL_TURN_ERROR_SETTLE_SECS: i64 = 30;

/// Turn-bridge outcomes that end a turn without a deliverable and without an
/// in-place retry; `empty_response` is a completion, `recovery_retry` reruns.
const TERMINAL_TURN_ERROR_OUTCOMES: [&str; 3] = ["cancelled", "transport_error", "prompt_too_long"];

#[derive(Debug, Clone, sqlx::FromRow)]
pub(super) struct TerminalTurnErrorRow {
    outcome: String,
    created_at: DateTime<Utc>,
}

/// The settled terminal outcome of a fresh run's turn, if any. Persistent-session
/// runs keep waiting for their transcript or the timeout.
pub(super) fn settled_terminal_turn_error(
    run: &RunningAgentRoutineRun,
    row: Option<TerminalTurnErrorRow>,
    now: DateTime<Utc>,
) -> Option<String> {
    let row = row?;
    (run.execution_strategy == "fresh"
        && now.signed_duration_since(row.created_at)
            >= Duration::seconds(TERMINAL_TURN_ERROR_SETTLE_SECS))
    .then_some(row.outcome)
}

pub(super) fn fresh_provider_session_probe_allowed(
    run: &RunningAgentRoutineRun,
    now: DateTime<Utc>,
) -> bool {
    run.execution_strategy == "fresh"
        && !owner_input::needs_owner(run)
        && execution::AgentExecutionMode::from_result(run.result_json.as_ref())
            != execution::AgentExecutionMode::Process
        && run.turn_id.is_some()
        && now.signed_duration_since(current_attempt_started_at(run))
            >= Duration::seconds(FRESH_PROVIDER_SESSION_LIVENESS_GRACE_SECS)
}

/// Completion evidence for a headless agent turn: its transcript, or a terminal
/// no-deliverable quality event.
pub(crate) async fn find_headless_turn_completion(
    pool: &PgPool,
    turn_id: &str,
    since: DateTime<Utc>,
) -> sqlx::Result<Option<AgentTurnCompletion>> {
    let transcript = sqlx::query_as::<_, AgentTranscriptCompletionRow>(
        r#"
        SELECT assistant_message, duration_ms::bigint AS duration_ms, created_at
        FROM session_transcripts
        WHERE turn_id = $1
          AND created_at >= $2
          AND BTRIM(assistant_message) <> ''
        ORDER BY created_at ASC
        LIMIT 1
        "#,
    )
    .bind(turn_id)
    .bind(since)
    .fetch_optional(pool)
    .await?;
    if let Some(transcript) = transcript {
        let evidence = if assistant_message_is_no_reply(&transcript.assistant_message) {
            AgentTurnCompletionEvidence::NoReplyTranscript
        } else {
            AgentTurnCompletionEvidence::AssistantTranscript
        };
        return Ok(Some(AgentTurnCompletion {
            assistant_message: Some(transcript.assistant_message),
            duration_ms: transcript.duration_ms,
            created_at: transcript.created_at,
            evidence,
            terminal_status: None,
        }));
    }

    let terminal = sqlx::query_as::<_, AgentQualityCompletionRow>(
        r#"
        SELECT event_type::text AS event_type,
               payload #>> '{details,outcome}' AS outcome,
               CASE
                   WHEN payload #>> '{details,duration_ms}' ~ '^-?[0-9]+$'
                   THEN (payload #>> '{details,duration_ms}')::bigint
                   ELSE NULL
               END AS duration_ms,
               created_at
        FROM agent_quality_event
        WHERE correlation_id = $1
          AND source_event_id = $1
          AND created_at >= $2
          AND event_type = 'turn_error'::agent_quality_event_type
          AND payload #>> '{details,outcome}' = 'empty_response'
        ORDER BY created_at ASC, id ASC
        LIMIT 1
        "#,
    )
    .bind(turn_id)
    .bind(since)
    .fetch_optional(pool)
    .await?;

    Ok(terminal.and_then(terminal_completion_from_quality_event))
}

impl RoutineAgentExecutor {
    pub(super) async fn find_turn_completion(
        &self,
        run: &RunningAgentRoutineRun,
    ) -> Result<Option<AgentTurnCompletion>> {
        let Some(turn_id) = run.turn_id.as_deref() else {
            return Ok(None);
        };
        find_headless_turn_completion(&self.pool, turn_id, run.started_at)
            .await
            .map_err(|error| {
                anyhow!(
                    "lookup routine agent turn completion {} for run {}: {error}",
                    turn_id,
                    run.run_id
                )
            })
    }
    /// Failure that ends a running fresh turn before the completion timeout:
    /// a settled terminal turn error, or a provider session that died.
    pub(super) async fn fresh_run_failure(
        &self,
        run: &RunningAgentRoutineRun,
    ) -> Result<Option<(&'static str, String)>> {
        if let Some(outcome) = self.fresh_turn_terminal_error(run).await? {
            let message = format!("routine agent turn ended without a deliverable ({outcome})");
            return Ok(Some(("turn_failed", message)));
        }
        Ok(self
            .fresh_provider_session_failure(run)
            .await
            .map(|message| ("provider_session_dead", message)))
    }

    /// Terminal `turn_error` for the run's turn, unless a `turn_complete`
    /// for the same turn shows it finished normally.
    async fn fresh_turn_terminal_error(
        &self,
        run: &RunningAgentRoutineRun,
    ) -> Result<Option<String>> {
        let Some(turn_id) = run
            .turn_id
            .as_deref()
            .filter(|_| run.execution_strategy == "fresh")
        else {
            return Ok(None);
        };
        let row = sqlx::query_as::<_, TerminalTurnErrorRow>(
            r#"
            SELECT payload #>> '{details,outcome}' AS outcome, created_at
            FROM agent_quality_event
            WHERE correlation_id = $1
              AND source_event_id = $1
              AND created_at >= $2
              AND event_type = 'turn_error'::agent_quality_event_type
              AND payload #>> '{details,outcome}' = ANY($3)
              AND NOT EXISTS (
                  SELECT 1 FROM agent_quality_event done
                  WHERE done.correlation_id = $1
                    AND done.source_event_id = $1
                    AND done.created_at >= $2
                    AND done.event_type = 'turn_complete'::agent_quality_event_type
              )
            ORDER BY created_at ASC, id ASC
            LIMIT 1
            "#,
        )
        .bind(turn_id)
        .bind(run.started_at)
        .bind(TERMINAL_TURN_ERROR_OUTCOMES.as_slice())
        .fetch_optional(&*self.pool)
        .await
        .map_err(|error| {
            anyhow!(
                "lookup routine agent turn error {} for run {}: {error}",
                turn_id,
                run.run_id
            )
        })?;
        Ok(settled_terminal_turn_error(run, row, Utc::now()))
    }
    /// A fresh managed-tmux turn that lost its pane cannot produce a
    /// transcript or terminal quality event. Detect that state before the
    /// routine's long completion timeout so the existing cleanup and
    /// retry/fallback policy can take over. Probe errors are deliberately
    /// ignored by `probe_tmux_session_pane_liveness` callers: only a definitive
    /// dead/absent result is actionable. A failure the probe finds stands only
    /// when the host guard admits the session's sessions row; otherwise the timeout decides.
    pub(super) async fn fresh_provider_session_failure(
        &self,
        run: &RunningAgentRoutineRun,
    ) -> Option<String> {
        use crate::services::platform::tmux::PaneLiveness;
        use crate::services::session_host::HostLiveness;
        if !fresh_provider_session_probe_allowed(run, Utc::now()) {
            return None;
        }
        let result_json = run.result_json.as_ref()?;
        let provider_name = result_json.get("provider").and_then(Value::as_str)?;
        let provider = ProviderKind::from_str(provider_name)?;
        if !provider.uses_managed_tmux_backend() {
            return None;
        }
        let agent_id =
            current_agent_id_from_result(Some(result_json)).or(run.agent_id.as_deref())?;
        let session_name =
            provider.build_tmux_session_name(&routine_agent_session_name(&run.name, agent_id));
        let (message, observed) =
            match crate::services::tmux_diagnostics::probe_tmux_session_pane_liveness(&session_name)
                .await
            {
                PaneLiveness::DeadOrAbsent => (
                    format!(
                        "routine fresh provider session ended before completion ({provider_name})"
                    ),
                    HostLiveness::DeadOrAbsent,
                ),
                PaneLiveness::Live if provider_name == "qwen" => {
                    let target = session_name.clone();
                    let pane = tokio::task::spawn_blocking(move || {
                        crate::services::platform::tmux::capture_pane_timeout(
                            &target,
                            -40,
                            std::time::Duration::from_secs(2),
                        )
                    })
                    .await
                    .ok()
                    .flatten()?;
                    let error =
                        crate::services::provider_error_transcript::qwen_terminal_api_error(&pane)?;
                    (
                        format!("routine Qwen terminal API failure: {error}"),
                        HostLiveness::Live,
                    )
                }
                PaneLiveness::Live | PaneLiveness::ProbeError => return None,
            };
        let channel_id = recorded_channel_id(result_json.get("channel_id"));
        self.ports
            .failure_admission
            .admit(&self.pool, &provider, channel_id, &session_name, observed)
            .await
            .then_some(message)
    }
}

/// The turn channel a routine run recorded: a decimal string as the executor writes it.
fn recorded_channel_id(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    match value.as_str() {
        Some(text) => text.parse().ok(),
        None => value.as_u64(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::running_run;
    use super::super::tests::completion_with_evidence;
    use super::*;
    use chrono::{DateTime, Duration, Utc};
    use serde_json::json;

    #[test]
    fn provider_error_from_completion_detects_known_error_only_transcript() {
        let mut completion =
            completion_with_evidence(AgentTurnCompletionEvidence::AssistantTranscript);
        completion.assistant_message =
            Some("Error: AI_APICallError: Too Many Requests (429)".to_string());

        assert_eq!(
            provider_error_from_completion(&completion).as_deref(),
            Some("Error: AI_APICallError: Too Many Requests (429)")
        );
    }

    #[test]
    fn provider_error_from_completion_allows_normal_error_reports() {
        for message in [
            "Error summary: the PR check failed, and the remediation is ready.",
            "API Error: The rate limit incident was resolved; today's briefing was delivered.",
            "API Error: An APIError explanation is ready for the report.",
            "Provider documentation updated.\n||```text\nWhen \"The usage limit has been reached\" appears, check the account quota.\n```||",
        ] {
            let mut completion =
                completion_with_evidence(AgentTurnCompletionEvidence::AssistantTranscript);
            completion.assistant_message = Some(message.to_string());

            assert_eq!(
                provider_error_from_completion(&completion),
                None,
                "normal report must not be classified as provider failure: {message}"
            );
        }
    }

    #[test]
    fn provider_error_from_completion_ignores_terminal_evidence() {
        let completion = completion_with_evidence(AgentTurnCompletionEvidence::TerminalTurn);

        assert_eq!(provider_error_from_completion(&completion), None);
    }

    #[test]
    fn terminal_turn_error_settles_only_for_fresh_runs() {
        let now = DateTime::parse_from_rfc3339("2026-10-18T00:01:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let row = |age: i64| {
            Some(TerminalTurnErrorRow {
                outcome: "cancelled".to_string(),
                created_at: now - Duration::seconds(age),
            })
        };
        let mut run = running_run(None);
        run.execution_strategy = "fresh".to_string();
        let settle = TERMINAL_TURN_ERROR_SETTLE_SECS;

        assert_eq!(
            settled_terminal_turn_error(&run, row(settle - 1), now),
            None
        );
        assert_eq!(
            settled_terminal_turn_error(&run, row(settle), now).as_deref(),
            Some("cancelled")
        );
        assert_eq!(settled_terminal_turn_error(&run, None, now), None);
        run.execution_strategy = "persistent".to_string();
        assert_eq!(settled_terminal_turn_error(&run, row(settle), now), None);
    }

    #[test]
    fn fresh_provider_session_probe_waits_for_grace_period() {
        let mut run = running_run(None);
        run.turn_id = Some("discord:123:456".to_string());
        run.started_at = DateTime::parse_from_rfc3339("2026-08-30T04:00:00Z")
            .expect("valid start")
            .with_timezone(&Utc);
        run.attempts = json!([{
            "event": "started",
            "at": "2026-08-30T04:00:00Z"
        }]);

        assert!(!fresh_provider_session_probe_allowed(
            &run,
            run.started_at + Duration::seconds(FRESH_PROVIDER_SESSION_LIVENESS_GRACE_SECS - 1)
        ));
        assert!(fresh_provider_session_probe_allowed(
            &run,
            run.started_at + Duration::seconds(FRESH_PROVIDER_SESSION_LIVENESS_GRACE_SECS)
        ));
    }

    #[test]
    fn process_attempt_never_probes_legacy_tmux_after_grace() {
        let mut run = running_run(None);
        run.result_json = Some(json!({"execution_mode":"process"}));
        run.execution_strategy = "fresh".into();
        run.turn_id = Some("discord:123:456".into());
        assert!(!fresh_provider_session_probe_allowed(
            &run,
            run.started_at + Duration::hours(3)
        ));
        run.result_json = Some(json!({"execution_mode":"tmux"}));
        assert!(fresh_provider_session_probe_allowed(
            &run,
            run.started_at + Duration::hours(3)
        ));
    }

    #[test]
    fn current_attempt_started_at_uses_latest_started_attempt() {
        let mut run = running_run(None);
        run.started_at = DateTime::parse_from_rfc3339("2026-08-30T04:00:00Z")
            .expect("valid start")
            .with_timezone(&Utc);
        run.attempts = json!([
            {
                "event": "started",
                "kind": "primary",
                "at": "2026-08-30T04:05:00Z"
            },
            {
                "event": "started",
                "kind": "fallback",
                "at": "2026-08-30T05:10:00+00:00"
            }
        ]);

        assert_eq!(
            current_attempt_started_at(&run),
            DateTime::parse_from_rfc3339("2026-08-30T05:10:00Z")
                .expect("valid attempt start")
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn current_attempt_started_at_ignores_malformed_attempts_and_falls_back() {
        let mut run = running_run(None);
        run.started_at = DateTime::parse_from_rfc3339("2026-08-30T04:00:00Z")
            .expect("valid start")
            .with_timezone(&Utc);
        run.attempts = json!([
            {
                "event": "started",
                "kind": "primary",
                "at": "2026-08-30T04:05:00Z"
            },
            {
                "event": "started",
                "kind": "fallback",
                "at": "not-a-timestamp"
            }
        ]);

        assert_eq!(
            current_attempt_started_at(&run),
            DateTime::parse_from_rfc3339("2026-08-30T04:05:00Z")
                .expect("valid attempt start")
                .with_timezone(&Utc)
        );

        run.attempts = json!([]);
        assert_eq!(current_attempt_started_at(&run), run.started_at);
    }
}
