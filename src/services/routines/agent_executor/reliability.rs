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
pub(super) fn provider_error_from_completion(completion: &AgentTurnCompletion) -> Option<String> {
    if !completion.evidence.confirms_assistant_delivery() {
        return None;
    }
    let message = completion.assistant_message.as_deref()?;
    is_strong_provider_error_transcript(message).then(|| assistant_preview(message))
}
pub(super) fn fresh_provider_session_probe_allowed(
    run: &RunningAgentRoutineRun,
    now: DateTime<Utc>,
) -> bool {
    run.execution_strategy == "fresh"
        && run.turn_id.is_some()
        && now.signed_duration_since(current_attempt_started_at(run))
            >= Duration::seconds(FRESH_PROVIDER_SESSION_LIVENESS_GRACE_SECS)
}

impl RoutineAgentExecutor {
    pub(super) async fn find_turn_completion(
        &self,
        run: &RunningAgentRoutineRun,
    ) -> Result<Option<AgentTurnCompletion>> {
        let Some(turn_id) = run.turn_id.as_deref() else {
            return Ok(None);
        };
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
        .bind(run.started_at)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|error| {
            anyhow!(
                "lookup routine agent transcript {} for run {}: {error}",
                turn_id,
                run.run_id
            )
        })?;
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
        .bind(run.started_at)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|error| {
            anyhow!(
                "lookup routine agent terminal turn {} for run {}: {error}",
                turn_id,
                run.run_id
            )
        })?;

        Ok(terminal.and_then(terminal_completion_from_quality_event))
    }
    /// A fresh managed-tmux turn that lost its pane cannot produce a
    /// transcript or terminal quality event. Detect that state before the
    /// routine's long completion timeout so the existing cleanup and
    /// retry/fallback policy can take over. Probe errors are deliberately
    /// ignored by `probe_tmux_session_pane_liveness` callers: only a definitive
    /// dead/absent result is actionable.
    pub(super) async fn fresh_provider_session_failure(
        &self,
        run: &RunningAgentRoutineRun,
    ) -> Option<String> {
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
        match crate::services::tmux_diagnostics::probe_tmux_session_pane_liveness(&session_name)
            .await
        {
            crate::services::platform::tmux::PaneLiveness::DeadOrAbsent => Some(format!(
                "routine fresh provider session ended before completion ({provider_name})"
            )),
            crate::services::platform::tmux::PaneLiveness::Live
            | crate::services::platform::tmux::PaneLiveness::ProbeError => None,
        }
    }


#[cfg(test)]
mod tests {
    use super::super::tests::{completion_with_evidence, running_run};
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
        let mut completion =
            completion_with_evidence(AgentTurnCompletionEvidence::AssistantTranscript);
        completion.assistant_message =
            Some("Error summary: the PR check failed, and the remediation is ready.".to_string());

        assert_eq!(provider_error_from_completion(&completion), None);
    }

    #[test]
    fn provider_error_from_completion_ignores_terminal_evidence() {
        let completion = completion_with_evidence(AgentTurnCompletionEvidence::TerminalTurn);

        assert_eq!(provider_error_from_completion(&completion), None);
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
