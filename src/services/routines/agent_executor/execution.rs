//! Durable execution choice: an attempt never guesses a tmux target for a process run.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AgentExecutionMode {
    Tmux,
    Process,
}

impl AgentExecutionMode {
    pub(super) fn for_new(strategy: &str, headless_fresh_agents: bool) -> Self {
        if strategy == "fresh" && headless_fresh_agents {
            Self::Process
        } else {
            Self::Tmux
        }
    }

    /// Missing metadata is a pre-upgrade tmux run, including its retries/fallbacks.
    pub(super) fn from_result(result: Option<&Value>) -> Self {
        if result
            .and_then(|value| value.get("execution_mode"))
            .and_then(Value::as_str)
            == Some("process")
        {
            Self::Process
        } else {
            Self::Tmux
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
            Self::Process => "process",
        }
    }

    pub(super) fn ensure_provider(self, provider: &ProviderKind) -> Result<()> {
        if self == Self::Process && !matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
        {
            return Err(anyhow!(
                "routine process execution requires Claude or Codex; configured provider is {}",
                provider.as_str()
            ));
        }
        Ok(())
    }

    pub(super) fn session_label(
        self,
        claimed: &ClaimedRoutineRun,
        agent_id: &str,
        turn_id: &str,
    ) -> String {
        let label = routine_agent_session_name(&claimed.name, agent_id);
        match self {
            Self::Tmux => label,
            // Keep old tmux rows separate and give every retry its own session identity.
            Self::Process => format!(
                "routine-process {} {label}",
                turn_id.rsplit(':').next().unwrap_or(turn_id)
            ),
        }
    }
}

pub(super) fn with_mode(mut result: Value, mode: AgentExecutionMode) -> Value {
    result["execution_mode"] = json!(mode.as_str());
    result
}

/// A terminal script result may replace result_json; the latest started attempt still owns its route.
fn close_routing_result(result: Option<Value>, attempts: &Value) -> Value {
    let mut result = result.filter(Value::is_object).unwrap_or_else(|| json!({}));
    if let Some(started) = attempts.as_array().and_then(|attempts| {
        attempts
            .iter()
            .rev()
            .find(|attempt| attempt.get("event").and_then(Value::as_str) == Some("started"))
    }) {
        for key in ["execution_mode", "provider", "channel_id", "turn_id"] {
            if let Some(value) = started.get(key).filter(|value| !value.is_null()) {
                result[key] = value.clone();
            }
        }
    }
    result
}

impl RoutineAgentExecutor {
    /// Script-close paths must read the owning attempt, never the current config or a guessed tmux name.
    pub(crate) async fn teardown_fresh_agent_run(
        &self,
        store: &RoutineStore,
        routine_id: &str,
        run_id: &str,
        reason: &str,
    ) {
        let attempt: Result<Option<(Option<Value>, Value)>, _> = sqlx::query_as(
            "SELECT result_json, attempts FROM routine_runs WHERE id = $1 AND turn_id IS NOT NULL",
        )
        .bind(run_id)
        .fetch_optional(&*self.pool)
        .await;
        match attempt {
            Ok(Some((result, attempts))) => {
                let result = close_routing_result(result, &attempts);
                self.teardown_fresh_agent_session(store, routine_id, Some(&result), reason)
                    .await;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(run_id, %error, "fresh routine teardown skipped: attempt mode lookup failed")
            }
        }
    }

    pub(super) async fn stop_process_attempt(&self, result: Option<&Value>, reason: &str) {
        let Some(result) = result else {
            return;
        };
        let Some(registry) = self.health_registry.as_deref() else {
            return;
        };
        let Some(provider) = result
            .get("provider")
            .and_then(Value::as_str)
            .and_then(ProviderKind::from_str)
        else {
            return;
        };
        let Some(channel) = result
            .get("channel_id")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|id| *id != 0)
        else {
            return;
        };
        let Some(turn_id) = result.get("turn_id").and_then(Value::as_str) else {
            return;
        };
        crate::services::discord::health::cancel_routine_process_turn(
            registry,
            &provider,
            poise::serenity_prelude::ChannelId::new(channel),
            turn_id,
            reason,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::claimed_run;
    use super::*;

    #[test]
    fn close_uses_durable_started_attempt_after_terminal_script_replaces_result() {
        let attempts = json!([
            {"event":"started", "execution_mode":"tmux", "turn_id":"old"},
            {"event":"started", "execution_mode":"process", "turn_id":"new", "provider":"claude", "channel_id":"123"},
        ]);
        let result = close_routing_result(Some(json!({"status":"completed"})), &attempts);
        assert_eq!(
            AgentExecutionMode::from_result(Some(&result)),
            AgentExecutionMode::Process
        );
        assert_eq!(result["turn_id"], "new");
        assert_eq!(result["channel_id"], "123");
        assert_eq!(result["status"], "completed");
    }

    #[test]
    fn process_is_opt_in_and_only_for_fresh_runs() {
        assert_eq!(
            AgentExecutionMode::for_new("fresh", true),
            AgentExecutionMode::Process
        );
        for (strategy, enabled) in [
            ("fresh", false),
            ("persistent", true),
            ("persistent", false),
        ] {
            assert_eq!(
                AgentExecutionMode::for_new(strategy, enabled),
                AgentExecutionMode::Tmux
            );
        }
    }

    #[test]
    fn durable_mode_keeps_process_and_legacy_attempts_across_config_changes() {
        for mode in [AgentExecutionMode::Process, AgentExecutionMode::Tmux] {
            let result = with_mode(json!({"status":"retry_scheduled"}), mode);
            assert_eq!(AgentExecutionMode::from_result(Some(&result)), mode);
        }
        assert_eq!(
            AgentExecutionMode::from_result(None),
            AgentExecutionMode::Tmux
        );
        assert_eq!(
            AgentExecutionMode::from_result(Some(&json!({"status":"started"}))),
            AgentExecutionMode::Tmux
        );
    }

    #[test]
    fn process_labels_do_not_replace_legacy_or_other_attempt_session_rows() {
        let mut claimed = claimed_run();
        claimed.name = "a very long routine name that fills the tmux label prefix".into();
        let label = |mode: AgentExecutionMode, turn| {
            ProviderKind::Claude
                .build_tmux_session_name(&mode.session_label(&claimed, "agent", turn))
        };
        assert_ne!(
            label(AgentExecutionMode::Process, "turn-1"),
            label(AgentExecutionMode::Tmux, "turn-1")
        );
        assert_ne!(
            label(AgentExecutionMode::Process, "turn-1"),
            label(AgentExecutionMode::Process, "turn-2")
        );
    }

    #[test]
    fn process_mode_refuses_unsupported_providers_instead_of_launching_tmux() {
        assert!(
            AgentExecutionMode::Process
                .ensure_provider(&ProviderKind::Claude)
                .is_ok()
        );
        assert!(
            AgentExecutionMode::Process
                .ensure_provider(&ProviderKind::Codex)
                .is_ok()
        );
        assert!(
            AgentExecutionMode::Process
                .ensure_provider(&ProviderKind::Qwen)
                .is_err()
        );
        assert!(
            AgentExecutionMode::Tmux
                .ensure_provider(&ProviderKind::Qwen)
                .is_ok()
        );
    }
}

#[cfg(test)]
mod pg_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::routines::store::NewRoutine;

    #[tokio::test(flavor = "current_thread")]
    async fn deferred_process_run_keeps_mode_after_executor_config_changes_pg() {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_cc) VALUES ('process-agent', 'process agent', 'claude', '6551001')")
            .execute(&pool).await.unwrap();
        let store = RoutineStore::new_with_timezone_and_checkpoint_limit(
            Arc::new(pool.clone()),
            "Asia/Seoul",
            256 * 1024,
        );
        let routine = store
            .attach_routine(NewRoutine {
                agent_id: Some("process-agent".into()),
                fallback_agent_id: None,
                max_retries: Some(0),
                script_ref: "process-test.js".into(),
                name: "process test".into(),
                status: None,
                execution_strategy: "fresh".into(),
                schedule: None,
                next_due_at: Some(Utc::now() - Duration::seconds(1)),
                checkpoint: None,
                discord_thread_id: None,
                timeout_secs: None,
            })
            .await
            .unwrap();
        let claimed = store.claim_run_now(&routine.id).await.unwrap().unwrap();
        let run_id = claimed.run_id.clone();
        let registry = Arc::new(HealthRegistry::new());
        let enabled = RoutineAgentExecutor::new(Arc::new(pool.clone()), Some(registry.clone()), 60)
            .with_headless_fresh_agents(true);
        let outcome = enabled
            .start_agent_run(&store, claimed, "run".into(), None, None, None, None, false)
            .await
            .unwrap();
        assert_eq!(outcome.status, "deferred");
        assert_eq!(outcome.result_json.unwrap()["execution_mode"], "process");
        sqlx::query(
            "UPDATE routine_runs SET next_retry_at = NOW() - INTERVAL '1 second' WHERE id = $1",
        )
        .bind(&run_id)
        .execute(&pool)
        .await
        .unwrap();
        let disabled = RoutineAgentExecutor::new(Arc::new(pool.clone()), Some(registry), 60);
        let outcomes = disabled.poll_agent_runs(&store, 10, false).await.unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, "deferred");
        assert_eq!(
            outcomes[0].result_json.as_ref().unwrap()["execution_mode"],
            "process"
        );
        let row: Value = sqlx::query_scalar("SELECT result_json FROM routine_runs WHERE id=$1")
            .bind(&run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row["execution_mode"], "process");
        pool.close().await;
        db.drop().await;
    }
}
