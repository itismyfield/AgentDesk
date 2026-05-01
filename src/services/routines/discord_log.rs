use chrono::Datelike;
use serde::Serialize;
use sqlx::PgPool;
use std::sync::Arc;

use crate::services::discord::health::{HealthRegistry, resolve_bot_http};

use super::runtime::RoutineRunOutcome;
use super::store::{ClaimedRoutineRun, RecoveredRoutineRun, RoutineRecord, RoutineStore};

#[derive(Clone)]
pub struct RoutineDiscordLogger {
    pool: Arc<PgPool>,
    health_registry: Option<Arc<HealthRegistry>>,
    health_target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoutineDiscordLogStatus {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl RoutineDiscordLogger {
    pub fn new_with_health_registry(
        pool: Arc<PgPool>,
        health_registry: Option<Arc<HealthRegistry>>,
        health_target: Option<String>,
    ) -> Self {
        Self {
            pool,
            health_registry,
            health_target,
        }
    }

    pub async fn log_routine_event(
        &self,
        routine: &RoutineRecord,
        event: RoutineLifecycleEvent,
    ) -> RoutineDiscordLogStatus {
        self.log_to_routine_target(
            routine.agent_id.as_deref(),
            routine.discord_thread_id.as_deref(),
            event.reason_code(),
            &format!("routine:{}:{}", routine.id, event.reason_code()),
            &routine_lifecycle_message(routine, event),
        )
        .await
    }

    pub async fn log_routine_event_by_id(
        &self,
        store: &RoutineStore,
        routine_id: &str,
        event: RoutineLifecycleEvent,
    ) -> RoutineDiscordLogStatus {
        match store.get_routine(routine_id).await {
            Ok(Some(routine)) => self.log_routine_event(&routine, event).await,
            Ok(None) => RoutineDiscordLogStatus::failed(format!(
                "routine {routine_id} not found while logging {}",
                event.reason_code()
            )),
            Err(error) => RoutineDiscordLogStatus::failed(format!(
                "failed to load routine {routine_id} for discord log: {error}"
            )),
        }
    }

    pub async fn log_run_started(
        &self,
        store: &RoutineStore,
        claimed: &ClaimedRoutineRun,
    ) -> RoutineDiscordLogStatus {
        let status = self
            .log_to_routine_target(
                claimed.agent_id.as_deref(),
                claimed.discord_thread_id.as_deref(),
                "routine_run_started",
                &format!(
                    "routine:{}:run:{}:started",
                    claimed.routine_id, claimed.run_id
                ),
                &run_started_message(claimed),
            )
            .await;
        self.persist_run_log_status(store, &claimed.run_id, &status)
            .await;
        status
    }

    pub async fn log_run_outcome(
        &self,
        store: &RoutineStore,
        outcome: &RoutineRunOutcome,
    ) -> RoutineDiscordLogStatus {
        let routine = match store.get_routine(&outcome.routine_id).await {
            Ok(value) => value,
            Err(error) => {
                let status = RoutineDiscordLogStatus::failed(format!(
                    "failed to load routine {} for discord log: {error}",
                    outcome.routine_id
                ));
                self.persist_run_log_status(store, &outcome.run_id, &status)
                    .await;
                return status;
            }
        };
        let Some(routine) = routine else {
            let status = RoutineDiscordLogStatus::failed(format!(
                "routine {} not found while logging run {}",
                outcome.routine_id, outcome.run_id
            ));
            self.persist_run_log_status(store, &outcome.run_id, &status)
                .await;
            return status;
        };

        let reason_code = format!("routine_run_{}", outcome.status);
        let status = self
            .log_to_routine_target(
                routine.agent_id.as_deref(),
                routine.discord_thread_id.as_deref(),
                &reason_code,
                &format!(
                    "routine:{}:run:{}:{}",
                    outcome.routine_id, outcome.run_id, outcome.status
                ),
                &run_outcome_message(&routine, outcome),
            )
            .await;
        self.persist_run_log_status(store, &outcome.run_id, &status)
            .await;
        status
    }

    pub async fn log_recovery(
        &self,
        store: &RoutineStore,
        recovered: &RecoveredRoutineRun,
    ) -> RoutineDiscordLogStatus {
        let message = recovery_message(recovered);
        let status = if recovered
            .discord_thread_id
            .as_deref()
            .and_then(channel_target_from_id)
            .is_some()
        {
            self.log_to_routine_target(
                recovered.agent_id.as_deref(),
                recovered.discord_thread_id.as_deref(),
                "routine_recovery_resumed",
                &format!(
                    "routine:{}:run:{}:recovery",
                    recovered.routine_id, recovered.run_id
                ),
                &message,
            )
            .await
        } else if let Some(target) = self.health_target.as_deref() {
            self.log_to_target(
                target,
                "notify",
                "routine_recovery_resumed",
                &format!(
                    "routine:{}:run:{}:recovery",
                    recovered.routine_id, recovered.run_id
                ),
                &message,
            )
            .await
        } else {
            RoutineDiscordLogStatus::skipped()
        };
        self.persist_run_log_status(store, &recovered.run_id, &status)
            .await;
        status
    }

    async fn log_to_routine_target(
        &self,
        agent_id: Option<&str>,
        discord_thread_id: Option<&str>,
        reason_code: &str,
        session_key: &str,
        content: &str,
    ) -> RoutineDiscordLogStatus {
        if let Some(target) = discord_thread_id.and_then(channel_target_from_id) {
            let bot = match normalized_agent_id(agent_id) {
                Some(agent_id) => match resolve_agent_log_bot(
                    &self.pool,
                    self.health_registry.as_deref(),
                    agent_id,
                )
                .await
                {
                    Ok(bot) => bot,
                    Err(error) => {
                        tracing::warn!(
                            "routine thread log bot resolution failed for {agent_id}: {error}"
                        );
                        return RoutineDiscordLogStatus::failed(error);
                    }
                },
                None => "notify".to_string(),
            };
            return self
                .log_to_target(&target, &bot, reason_code, session_key, content)
                .await;
        }

        let Some(agent_id) = normalized_agent_id(agent_id) else {
            return RoutineDiscordLogStatus::skipped();
        };

        let target = match resolve_agent_channel_target(&self.pool, agent_id).await {
            Ok(target) => target,
            Err(error) => {
                return RoutineDiscordLogStatus::failed(error);
            }
        };

        let bot = match resolve_agent_log_bot(&self.pool, self.health_registry.as_deref(), agent_id)
            .await
        {
            Ok(bot) => bot,
            Err(error) => {
                tracing::warn!("routine log bot resolution failed for {agent_id}: {error}");
                return RoutineDiscordLogStatus::failed(error);
            }
        };

        self.log_to_target(&target, &bot, reason_code, session_key, content)
            .await
    }

    async fn log_to_target(
        &self,
        target: &str,
        bot: &str,
        reason_code: &str,
        session_key: &str,
        content: &str,
    ) -> RoutineDiscordLogStatus {
        match crate::services::message_outbox::enqueue_outbox_pg(
            &self.pool,
            crate::services::message_outbox::OutboxMessage {
                target,
                content,
                bot,
                source: "routine-runtime",
                reason_code: Some(reason_code),
                session_key: Some(session_key),
            },
        )
        .await
        {
            Ok(_) => RoutineDiscordLogStatus::ok(),
            Err(error) => RoutineDiscordLogStatus::failed(format!(
                "failed to enqueue routine discord log: {error}"
            )),
        }
    }

    async fn persist_run_log_status(
        &self,
        store: &RoutineStore,
        run_id: &str,
        status: &RoutineDiscordLogStatus,
    ) {
        if let Err(error) = store
            .record_discord_log_result(run_id, &status.status, status.warning.as_deref())
            .await
        {
            tracing::warn!(
                run_id,
                error = %error,
                "failed to persist routine discord log status"
            );
        }
    }
}

impl RoutineDiscordLogStatus {
    fn ok() -> Self {
        Self {
            status: "ok".to_string(),
            warning_code: None,
            warning: None,
        }
    }

    fn skipped() -> Self {
        Self {
            status: "skipped".to_string(),
            warning_code: None,
            warning: None,
        }
    }

    fn failed(error: impl ToString) -> Self {
        let warning = error.to_string();
        let warning_code = classify_warning(&warning);
        tracing::warn!(warning, warning_code, "routine discord log failed");
        Self {
            status: "failed".to_string(),
            warning_code: Some(warning_code.to_string()),
            warning: Some(warning),
        }
    }
}

fn classify_warning(warning: &str) -> &'static str {
    let lower = warning.to_ascii_lowercase();
    if lower.contains("missing permissions")
        || lower.contains("missing access")
        || lower.contains("permission")
        || lower.contains("forbidden")
        || lower.contains("403")
    {
        "discord_permission_denied"
    } else if lower.contains("archived") {
        "discord_thread_archived"
    } else if (lower.contains("thread") && lower.contains("creat"))
        || lower.contains("create thread")
    {
        "discord_thread_creation_failed"
    } else if lower.contains("primary channel is invalid") {
        "discord_channel_invalid"
    } else if lower.contains("no primary channel") {
        "discord_channel_missing"
    } else if lower.contains("agent ") && lower.contains("not found") {
        "agent_not_found"
    } else if lower.contains("enqueue") || lower.contains("message_outbox") {
        "message_outbox_enqueue_failed"
    } else {
        "discord_log_failed"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineLifecycleEvent {
    Attached,
    Paused,
    Resumed,
    Detached,
}

impl RoutineLifecycleEvent {
    fn reason_code(self) -> &'static str {
        match self {
            Self::Attached => "routine_attached",
            Self::Paused => "routine_paused",
            Self::Resumed => "routine_resumed",
            Self::Detached => "routine_detached",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::Paused => "paused",
            Self::Resumed => "resumed",
            Self::Detached => "detached",
        }
    }
}

async fn resolve_agent_channel_target(pool: &PgPool, agent_id: &str) -> Result<String, String> {
    let bindings = crate::db::agents::load_agent_channel_bindings_pg(pool, agent_id)
        .await
        .map_err(|error| format!("load agent bindings for routine log {agent_id}: {error}"))?
        .ok_or_else(|| format!("agent {agent_id} not found for routine log"))?;
    let primary_channel = bindings
        .primary_channel()
        .ok_or_else(|| format!("agent {agent_id} has no primary channel for routine log"))?;
    let channel_id = crate::server::routes::dispatches::resolve_channel_alias_pub(&primary_channel)
        .or_else(|| primary_channel.parse::<u64>().ok())
        .ok_or_else(|| {
            format!(
                "agent {agent_id} primary channel is invalid for routine log: {primary_channel}"
            )
        })?;
    Ok(format!("channel:{channel_id}"))
}

async fn resolve_agent_provider_bot(pool: &PgPool, agent_id: &str) -> Result<String, String> {
    let bindings = crate::db::agents::load_agent_channel_bindings_pg(pool, agent_id)
        .await
        .map_err(|error| format!("load agent bindings for routine log {agent_id}: {error}"))?
        .ok_or_else(|| format!("agent {agent_id} not found for routine log"))?;
    bindings
        .resolved_primary_provider_kind()
        .map(|provider| provider.as_str().to_string())
        .ok_or_else(|| format!("agent {agent_id} has no primary provider for routine log"))
}

async fn resolve_agent_log_bot(
    pool: &PgPool,
    health_registry: Option<&HealthRegistry>,
    agent_id: &str,
) -> Result<String, String> {
    let provider_bot = match resolve_agent_provider_bot(pool, agent_id).await {
        Ok(bot) => Some(bot),
        Err(error) => {
            tracing::warn!(
                "routine log provider bot lookup failed for {agent_id}: {error}; trying fallback bots"
            );
            None
        }
    };
    let candidates = routine_log_bot_candidates(provider_bot.as_deref(), Some(agent_id));
    resolve_available_log_bot(health_registry, &candidates).await
}

async fn resolve_available_log_bot(
    health_registry: Option<&HealthRegistry>,
    candidates: &[String],
) -> Result<String, String> {
    let Some(health_registry) = health_registry else {
        return candidates
            .first()
            .cloned()
            .ok_or_else(|| "no routine discord log bot candidates".to_string());
    };

    let mut errors = Vec::new();
    for bot in candidates {
        match resolve_bot_http(health_registry, bot).await {
            Ok(_) => return Ok(bot.clone()),
            Err((status, error)) => errors.push(format!("{bot}: {status} {error}")),
        }
    }

    Err(format!(
        "no routine discord log bot available ({})",
        errors.join("; ")
    ))
}

fn routine_log_bot_candidates(provider_bot: Option<&str>, agent_id: Option<&str>) -> Vec<String> {
    let mut candidates = Vec::new();
    for bot in [provider_bot, agent_id, Some("notify")]
        .into_iter()
        .flatten()
    {
        let bot = bot.trim();
        if bot.is_empty() || candidates.iter().any(|candidate| candidate == bot) {
            continue;
        }
        candidates.push(bot.to_string());
    }
    candidates
}

fn normalized_agent_id(agent_id: Option<&str>) -> Option<&str> {
    agent_id.map(str::trim).filter(|value| !value.is_empty())
}

fn channel_target_from_id(value: &str) -> Option<String> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(|channel_id| format!("channel:{channel_id}"))
}

fn routine_lifecycle_message(routine: &RoutineRecord, event: RoutineLifecycleEvent) -> String {
    format!(
        "루틴 {}: {} / id {} / script {} / status {}",
        event.label(),
        compact(&routine.name, 80),
        short_id(&routine.id),
        compact(&routine.script_ref, 80),
        routine.status
    )
}

fn recovery_message(recovered: &RecoveredRoutineRun) -> String {
    format!(
        "[재개] 서버 재시작 - routine을 다시 스케줄합니다: {} / run {} / script {}",
        compact(&recovered.name, 80),
        short_id(&recovered.run_id),
        compact(&recovered.script_ref, 80)
    )
}

fn run_started_message(claimed: &ClaimedRoutineRun) -> String {
    format!(
        "[{}] 루틴 실행 시작: {} / run {} / script {}",
        routine_log_timestamp(),
        compact(&claimed.name, 80),
        short_id(&claimed.run_id),
        script_ref_for_message(&claimed.script_ref, 80)
    )
}

fn run_outcome_message(routine: &RoutineRecord, outcome: &RoutineRunOutcome) -> String {
    let mut message = format!(
        "[{}] 루틴 실행 결과: {} / run {} / action \"{}\" / status \"{}\"",
        routine_log_timestamp(),
        compact(&routine.name, 80),
        short_id(&outcome.run_id),
        outcome.action,
        outcome.status
    );
    if let Some(error) = outcome
        .error
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        message.push_str(" / error ");
        message.push_str(&compact(error, 160));
    }
    if let Some(summary) = outcome_summary_for_message(outcome) {
        message.push_str(" / 요약 ");
        message.push_str(&compact(&summary, 220));
    }
    message
}

fn outcome_summary_for_message(outcome: &RoutineRunOutcome) -> Option<String> {
    let result = outcome.result_json.as_ref()?;
    for key in [
        "outcome_summary",
        "summary",
        "assistant_message_preview",
        "reason",
        "error",
    ] {
        if let Some(value) = result
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_string());
        }
    }
    None
}

fn script_ref_for_message(value: &str, max_chars: usize) -> String {
    compact(&value.split('/').collect::<Vec<_>>().join(" / "), max_chars)
}

fn routine_log_timestamp() -> String {
    let now = chrono::Utc::now().with_timezone(&chrono_tz::Asia::Seoul);
    format!(
        "{} {} {}",
        now.format("%Y-%m-%d"),
        korean_weekday(now.weekday()),
        now.format("%H:%M:%S")
    )
}

fn korean_weekday(weekday: chrono::Weekday) -> &'static str {
    match weekday {
        chrono::Weekday::Mon => "월",
        chrono::Weekday::Tue => "화",
        chrono::Weekday::Wed => "수",
        chrono::Weekday::Thu => "목",
        chrono::Weekday::Fri => "금",
        chrono::Weekday::Sat => "토",
        chrono::Weekday::Sun => "일",
    }
}

fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

fn compact(value: &str, max_chars: usize) -> String {
    let trimmed = value.trim();
    let mut result: String = trimmed.chars().take(max_chars).collect();
    if trimmed.chars().count() > max_chars {
        result.push_str("...");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    #[test]
    fn compact_caps_long_values() {
        assert_eq!(compact("abcdef", 3), "abc...");
        assert_eq!(compact(" abc ", 10), "abc");
    }

    #[test]
    fn run_outcome_message_includes_error_preview() {
        let routine = RoutineRecord {
            id: "routine-123456789".to_string(),
            agent_id: Some("maker".to_string()),
            script_ref: "daily-summary.js".to_string(),
            name: "Daily Summary".to_string(),
            status: "enabled".to_string(),
            execution_strategy: "fresh".to_string(),
            schedule: None,
            next_due_at: None,
            last_run_at: None,
            last_result: None,
            checkpoint: None,
            discord_thread_id: None,
            timeout_secs: None,
            in_flight_run_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let outcome = RoutineRunOutcome {
            run_id: "run-123456789".to_string(),
            routine_id: routine.id.clone(),
            script_ref: routine.script_ref.clone(),
            action: "complete".to_string(),
            status: "failed".to_string(),
            result_json: Some(json!({"error": "boom"})),
            error: Some("boom".to_string()),
            fresh_context_guaranteed: false,
        };

        let message = run_outcome_message(&routine, &outcome);

        assert!(message.starts_with("["));
        assert!(message.contains("] 루틴 실행 결과: Daily Summary"));
        assert!(message.contains("Daily Summary"));
        assert!(message.contains("run-1234"));
        assert!(message.contains("action \"complete\""));
        assert!(message.contains("status \"failed\""));
        assert!(message.contains("error boom"));
    }

    #[test]
    fn run_outcome_message_includes_result_summary_preview() {
        let routine = RoutineRecord {
            id: "routine-123456789".to_string(),
            agent_id: Some("monitoring".to_string()),
            script_ref: "monitoring/automation-candidate-recommender.js".to_string(),
            name: "automation-candidate-recommender".to_string(),
            status: "enabled".to_string(),
            execution_strategy: "fresh".to_string(),
            schedule: None,
            next_due_at: None,
            last_run_at: None,
            last_result: None,
            checkpoint: None,
            discord_thread_id: Some("1499416722204004372".to_string()),
            timeout_secs: None,
            in_flight_run_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let outcome = RoutineRunOutcome {
            run_id: "run-123456789".to_string(),
            routine_id: routine.id.clone(),
            script_ref: routine.script_ref.clone(),
            action: "complete".to_string(),
            status: "succeeded".to_string(),
            result_json: Some(json!({
                "outcome_summary": "성공 요약: 새 자동화 추천 후보 없음"
            })),
            error: None,
            fresh_context_guaranteed: false,
        };

        let message = run_outcome_message(&routine, &outcome);

        assert!(message.contains("요약 성공 요약: 새 자동화 추천 후보 없음"));
    }

    #[test]
    fn run_started_message_includes_timestamp_and_readable_script_ref() {
        let claimed = ClaimedRoutineRun {
            run_id: "21e14c13-0000-0000-0000-000000000000".to_string(),
            routine_id: "routine-123456789".to_string(),
            agent_id: Some("monitoring".to_string()),
            script_ref: "monitoring/working-watchdog.js".to_string(),
            name: "monitoring-working-watchdog".to_string(),
            execution_strategy: "fresh".to_string(),
            checkpoint: None,
            discord_thread_id: None,
            timeout_secs: None,
            lease_expires_at: Utc::now(),
        };

        let message = run_started_message(&claimed);

        assert!(message.starts_with("["));
        assert!(message.contains("] 루틴 실행 시작: monitoring-working-watchdog"));
        assert!(message.contains("run 21e14c13"));
        assert!(message.contains("script monitoring / working-watchdog.js"));
    }

    #[test]
    fn missing_agent_is_skipped_without_warning() {
        let status = RoutineDiscordLogStatus::skipped();
        assert_eq!(status.status, "skipped");
        assert_eq!(status.warning_code, None);
        assert_eq!(status.warning, None);
    }

    #[test]
    fn routine_log_bot_candidates_dedupes_and_keeps_notify_fallback() {
        assert_eq!(
            routine_log_bot_candidates(Some("codex"), Some("maker")),
            vec!["codex", "maker", "notify"]
        );
        assert_eq!(
            routine_log_bot_candidates(Some(" maker "), Some("maker")),
            vec!["maker", "notify"]
        );
        assert_eq!(routine_log_bot_candidates(None, Some("")), vec!["notify"]);
    }

    #[test]
    fn discord_log_warning_codes_are_operator_specific() {
        assert_eq!(
            classify_warning("Discord API returned 403 Missing Permissions"),
            "discord_permission_denied"
        );
        assert_eq!(
            classify_warning("cannot reuse archived thread 123"),
            "discord_thread_archived"
        );
        assert_eq!(
            classify_warning("thread creation failed for routine log"),
            "discord_thread_creation_failed"
        );
        assert_eq!(
            classify_warning("failed to enqueue routine discord log: db down"),
            "message_outbox_enqueue_failed"
        );
    }
}
