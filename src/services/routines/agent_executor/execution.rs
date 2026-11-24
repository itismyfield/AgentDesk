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

    /// A provider without process turns runs a fresh run as a fresh channel session instead.
    pub(super) fn for_provider(self, provider: &ProviderKind) -> Self {
        match self {
            Self::Process if Self::Process.ensure_provider(provider).is_err() => Self::Tmux,
            mode => mode,
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

/// A terminal script result may replace result_json; the latest intent still names its route.
fn close_routing_result(result: Option<Value>, attempts: &Value) -> Value {
    let mut result = result.filter(Value::is_object).unwrap_or_else(|| json!({}));
    if let Some(started) = attempts.as_array().and_then(|attempts| {
        attempts.iter().rev().find(|attempt| {
            matches!(
                attempt.get("event").and_then(Value::as_str),
                Some("started" | "input_pending")
            )
        })
    }) {
        for key in [
            "execution_mode",
            "provider",
            "channel_id",
            "turn_id",
            "owner_input",
        ] {
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
                self.teardown_fresh_agent_session(store, routine_id, run_id, Some(&result), reason)
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
        let channel = poise::serenity_prelude::ChannelId::new(channel);
        registry
            .cancel_routine_process_turn(&provider, channel, turn_id, reason)
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
    fn terminal_script_keeps_the_owner_intent_even_before_a_started_event() {
        let owner = json!({"ticket":"held-ticket","context":{"run_id":"run"}});
        let result = close_routing_result(
            Some(json!({"status":"completed"})),
            &json!([
                {"event":"started","execution_mode":"process","turn_id":"old"},
                {"event":"input_pending","execution_mode":"tmux","turn_id":"held","owner_input":owner}
            ]),
        );
        assert_eq!(result["owner_input"], owner);
        assert_eq!(result["turn_id"], "held");
        assert_eq!(
            AgentExecutionMode::from_result(Some(&result)),
            AgentExecutionMode::Tmux
        );
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
