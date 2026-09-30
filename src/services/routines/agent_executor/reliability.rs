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
pub(super) fn fresh_provider_session_probe_allowed(
    run: &RunningAgentRoutineRun,
    now: DateTime<Utc>,
) -> bool {
    run.execution_strategy == "fresh"
        && run.turn_id.is_some()
        && now.signed_duration_since(current_attempt_started_at(run))
            >= Duration::seconds(FRESH_PROVIDER_SESSION_LIVENESS_GRACE_SECS)
}

/// Completion evidence for a headless agent turn: its transcript, or a terminal
/// no-deliverable quality event. Shared by routines and the voice conductor.
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
        let channel_id = crate::services::discord::host_key_derivation::recorded_channel_id(
            result_json.get("channel_id"),
        );
        crate::services::discord::host_key_derivation::routine_session_failure_admitted(
            self.health_registry.as_deref(),
            &self.pool,
            &provider,
            channel_id,
            &session_name,
            observed,
        )
        .await
        .then_some(message)
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

    fn probe_run(provider: &str, name: &str, channel: Option<Value>) -> RunningAgentRoutineRun {
        let started = Utc::now() - Duration::hours(1);
        let mut result = json!({"provider": provider, "agent_id": "agent-1"});
        if let Some(channel) = channel {
            result["channel_id"] = channel;
        }
        RunningAgentRoutineRun {
            name: name.to_string(),
            started_at: started,
            attempts: json!([{"event": "started", "at": started.to_rfc3339()}]),
            result_json: Some(result),
            ..running_run(None)
        }
    }

    fn probe_session(provider: &str, name: &str) -> String {
        let provider = ProviderKind::from_str(provider).unwrap();
        provider.build_tmux_session_name(&routine_agent_session_name(name, "agent-1"))
    }

    // The probe's failure stands only for a found legacy row behind a registered bot hash;
    // a missing, failed, Herdr or unregistered answer leaves the run to its timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn fresh_probe_failure_stands_only_for_a_found_legacy_row_pg() {
        use crate::db::dispatched_sessions::hosted_execution::tests::{
            TOKEN, owner, pending, wire,
        };
        use crate::services::discord::host_key_derivation::tests::seed_row;
        use crate::services::platform::tmux::PaneLiveness;
        use crate::services::tmux_diagnostics::PaneLivenessOverrideGuard;
        use std::os::unix::fs::PermissionsExt;

        let _root = crate::config::TestRuntimeRootGuard::new();
        let fake = tempfile::tempdir().unwrap();
        let tmux = fake.path().join("tmux");
        let pane = "[sending...]\n[API Error: 410 status code (no body)]\n\n▶ Ready for input (type message + Enter)";
        let body =
            format!("#!/bin/sh\n[ \"$2\" = capture-pane ] || exit 1\nprintf '%s\\n' '{pane}'\n");
        std::fs::write(&tmux, body).unwrap();
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![fake.path().to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let path = std::env::join_paths(paths).unwrap();
        let _path = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "PATH",
            std::path::Path::new(&path),
        );

        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let registry = Arc::new(HealthRegistry::new());
        for provider in ["claude", "qwen"] {
            let mut shared = crate::services::discord::make_shared_data_for_tests();
            Arc::get_mut(&mut shared).unwrap().token_hash = TOKEN.to_string();
            registry.register(provider.to_string(), shared).await;
        }
        let channel = |n: u64| 1_479_671_301_387_061_000 + n;
        let seed = |provider: &'static str, hash: &'static str, key: String, n: u64, raw| {
            let pool = pool.clone();
            async move { seed_row(&pool, provider, Some(hash), &key, channel(n), raw).await }
        };
        let current = |provider: &str, hash: &str, name: &str| {
            let host = crate::services::platform::hostname_short();
            format!("{provider}/{hash}/{host}:{}", probe_session(provider, name))
        };
        seed(
            "claude",
            TOKEN,
            current("claude", TOKEN, "w2b legacy"),
            1,
            None,
        )
        .await;
        let moved = format!(
            "claude/{TOKEN}/old-host:{}",
            probe_session("claude", "w2b moved")
        );
        seed("claude", TOKEN, moved, 2, None).await;
        seed(
            "claude",
            "h-old",
            current("claude", "h-old", "w2b rotated"),
            3,
            None,
        )
        .await;
        let herdr = Some(wire(&pending(&owner(&channel(4).to_string()), "n-w2b")));
        seed(
            "claude",
            TOKEN,
            current("claude", TOKEN, "w2b herdr"),
            4,
            herdr,
        )
        .await;
        seed(
            "claude",
            TOKEN,
            current("claude", TOKEN, "w2b marker"),
            5,
            None,
        )
        .await;
        let marker = crate::services::tmux_common::session_temp_path(
            &probe_session("claude", "w2b marker"),
            "host_kind",
        );
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, "herdr").unwrap();
        seed("qwen", TOKEN, current("qwen", TOKEN, "w2b qwen"), 6, None).await;

        let executor = |registry: Option<Arc<HealthRegistry>>| {
            RoutineAgentExecutor::new(Arc::new(pool.clone()), registry, 60)
        };
        let registered = executor(Some(registry.clone()));
        let text = |n: u64| Some(json!(channel(n).to_string()));
        let dead = PaneLiveness::DeadOrAbsent;
        let cases = [
            (
                "found legacy row",
                "claude",
                "w2b legacy",
                text(1),
                dead,
                true,
            ),
            (
                "renamed host, channel as text",
                "claude",
                "w2b moved",
                text(2),
                dead,
                true,
            ),
            (
                "renamed host, channel as number",
                "claude",
                "w2b moved",
                Some(json!(channel(2))),
                dead,
                true,
            ),
            (
                "renamed host, no channel",
                "claude",
                "w2b moved",
                None,
                dead,
                false,
            ),
            ("no row", "claude", "w2b absent", text(7), dead, false),
            (
                "rotated token",
                "claude",
                "w2b rotated",
                text(3),
                dead,
                false,
            ),
            ("Herdr record", "claude", "w2b herdr", text(4), dead, false),
            ("Herdr marker", "claude", "w2b marker", text(5), dead, false),
            (
                "live pane",
                "claude",
                "w2b legacy",
                text(1),
                PaneLiveness::Live,
                false,
            ),
            (
                "failed probe",
                "claude",
                "w2b legacy",
                text(1),
                PaneLiveness::ProbeError,
                false,
            ),
            (
                "Qwen API error, legacy row",
                "qwen",
                "w2b qwen",
                text(6),
                PaneLiveness::Live,
                true,
            ),
            (
                "Qwen API error, no row",
                "qwen",
                "w2b qwen absent",
                text(8),
                PaneLiveness::Live,
                false,
            ),
        ];
        for (label, provider, name, channel, liveness, fails) in cases {
            let _pane = PaneLivenessOverrideGuard::set(&probe_session(provider, name), liveness);
            let run = probe_run(provider, name, channel);
            let failure = registered.fresh_provider_session_failure(&run).await;
            assert_eq!(failure.is_some(), fails, "{label}: {failure:?}");
        }
        let _pane = PaneLivenessOverrideGuard::set(&probe_session("claude", "w2b legacy"), dead);
        let run = probe_run("claude", "w2b legacy", text(1));
        let unregistered = executor(None).fresh_provider_session_failure(&run).await;
        assert_eq!(
            unregistered, None,
            "no registered hash is not a legacy answer"
        );
        pool.close().await;
        let failed = registered.fresh_provider_session_failure(&run).await;
        assert_eq!(failed, None, "a failed lookup is not a legacy answer");
        db.drop().await;
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
