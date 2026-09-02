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
}
