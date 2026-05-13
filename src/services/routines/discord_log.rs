use anyhow::{Result, anyhow};
use chrono::Datelike;
use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use std::sync::{Arc, OnceLock};

use crate::services::discord::health::{HealthRegistry, resolve_bot_http};
use crate::services::discord::outbound::{
    DeliveryResult, DiscordOutboundMessage, DiscordOutboundPolicy, HttpOutboundClient,
    OutboundDeduper, deliver_outbound,
};

use super::runtime::RoutineRunOutcome;
use super::store::{ClaimedRoutineRun, RecoveredRoutineRun, RoutineRecord, RoutineStore};

const RUN_LOG_SECTION_ORDER: [&str; 4] = ["started", "js_inputs", "js_action", "outcome"];

fn routine_run_log_deduper() -> &'static OutboundDeduper {
    static DEDUPER: OnceLock<OutboundDeduper> = OnceLock::new();
    DEDUPER.get_or_init(OutboundDeduper::new)
}

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
            None,
            Some(&routine.id),
            Some(&routine.name),
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
            Ok(Some(routine)) => {
                self.log_routine_event_with_store(store, &routine, event)
                    .await
            }
            Ok(None) => RoutineDiscordLogStatus::failed(format!(
                "routine {routine_id} not found while logging {}",
                event.reason_code()
            )),
            Err(error) => RoutineDiscordLogStatus::failed(format!(
                "failed to load routine {routine_id} for discord log: {error}"
            )),
        }
    }

    pub async fn log_routine_event_with_store(
        &self,
        store: &RoutineStore,
        routine: &RoutineRecord,
        event: RoutineLifecycleEvent,
    ) -> RoutineDiscordLogStatus {
        self.log_to_routine_target(
            Some(store),
            Some(&routine.id),
            Some(&routine.name),
            routine.agent_id.as_deref(),
            routine.discord_thread_id.as_deref(),
            event.reason_code(),
            &format!("routine:{}:{}", routine.id, event.reason_code()),
            &routine_lifecycle_message(routine, event),
        )
        .await
    }

    pub async fn log_run_started(
        &self,
        store: &RoutineStore,
        claimed: &ClaimedRoutineRun,
    ) -> RoutineDiscordLogStatus {
        let status = self
            .log_run_section(
                store,
                Some(&claimed.routine_id),
                Some(&claimed.name),
                claimed.agent_id.as_deref(),
                claimed.discord_thread_id.as_deref(),
                &claimed.run_id,
                "started",
                &run_started_section(claimed),
            )
            .await;
        self.persist_run_log_status(store, &claimed.run_id, &status)
            .await;
        status
    }

    pub async fn log_run_js_inputs(
        &self,
        store: &RoutineStore,
        claimed: &ClaimedRoutineRun,
        observation_count: usize,
        automation_inventory_count: usize,
        checkpoint: Option<&Value>,
    ) -> RoutineDiscordLogStatus {
        let status = self
            .log_run_section(
                store,
                Some(&claimed.routine_id),
                Some(&claimed.name),
                claimed.agent_id.as_deref(),
                claimed.discord_thread_id.as_deref(),
                &claimed.run_id,
                "js_inputs",
                &run_js_inputs_section(
                    claimed,
                    observation_count,
                    automation_inventory_count,
                    checkpoint,
                ),
            )
            .await;
        self.persist_run_log_status(store, &claimed.run_id, &status)
            .await;
        status
    }

    pub async fn log_run_js_action(
        &self,
        store: &RoutineStore,
        claimed: &ClaimedRoutineRun,
        action: &str,
        summary: Option<&str>,
        prompt: Option<&str>,
        checkpoint_update: bool,
    ) -> RoutineDiscordLogStatus {
        let status = self
            .log_run_section(
                store,
                Some(&claimed.routine_id),
                Some(&claimed.name),
                claimed.agent_id.as_deref(),
                claimed.discord_thread_id.as_deref(),
                &claimed.run_id,
                "js_action",
                &run_js_action_section(claimed, action, summary, prompt, checkpoint_update),
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

        let status = self
            .log_run_section(
                store,
                Some(&routine.id),
                Some(&routine.name),
                routine.agent_id.as_deref(),
                routine.discord_thread_id.as_deref(),
                &outcome.run_id,
                "outcome",
                &run_outcome_section(&routine, outcome),
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
                Some(store),
                Some(&recovered.routine_id),
                Some(&recovered.name),
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
        store: Option<&RoutineStore>,
        routine_id: Option<&str>,
        routine_name: Option<&str>,
        agent_id: Option<&str>,
        discord_thread_id: Option<&str>,
        reason_code: &str,
        session_key: &str,
        content: &str,
    ) -> RoutineDiscordLogStatus {
        let target = match self
            .resolve_routine_log_target(
                store,
                routine_id,
                routine_name,
                agent_id,
                discord_thread_id,
            )
            .await
        {
            Ok(Some(target)) => target,
            Ok(None) => return RoutineDiscordLogStatus::skipped(),
            Err(error) => return RoutineDiscordLogStatus::failed(error),
        };

        self.log_to_target(
            &target.target,
            &target.bot,
            reason_code,
            session_key,
            content,
        )
        .await
    }

    async fn log_run_section(
        &self,
        store: &RoutineStore,
        routine_id: Option<&str>,
        routine_name: Option<&str>,
        agent_id: Option<&str>,
        discord_thread_id: Option<&str>,
        run_id: &str,
        section_key: &str,
        section_text: &str,
    ) -> RoutineDiscordLogStatus {
        let state = match store
            .record_run_discord_log_section(run_id, section_key, section_text)
            .await
        {
            Ok(state) => state,
            Err(error) => return RoutineDiscordLogStatus::failed(error),
        };
        let content = routine_run_progress_message(&render_run_log_sections(&state.sections));
        let target = match self
            .resolve_routine_log_target(
                Some(store),
                routine_id,
                routine_name,
                agent_id,
                discord_thread_id,
            )
            .await
        {
            Ok(Some(target)) => target,
            Ok(None) => return RoutineDiscordLogStatus::skipped(),
            Err(error) => return RoutineDiscordLogStatus::failed(error),
        };

        match self
            .send_or_edit_run_log(&target, state.message_id.as_deref(), &content)
            .await
        {
            Ok(message_id) => {
                if state.message_id.as_deref() != Some(message_id.as_str()) {
                    if let Err(error) = store
                        .record_run_discord_message_id(run_id, &message_id)
                        .await
                    {
                        return RoutineDiscordLogStatus::failed(error);
                    }
                }
                RoutineDiscordLogStatus::ok()
            }
            Err(error) => RoutineDiscordLogStatus::failed(error),
        }
    }

    async fn resolve_routine_log_target(
        &self,
        store: Option<&RoutineStore>,
        routine_id: Option<&str>,
        routine_name: Option<&str>,
        agent_id: Option<&str>,
        discord_thread_id: Option<&str>,
    ) -> Result<Option<RoutineLogTarget>, String> {
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
                        return Err(error);
                    }
                },
                None => "notify".to_string(),
            };
            return Ok(Some(RoutineLogTarget { target, bot }));
        }

        let Some(agent_id) = normalized_agent_id(agent_id) else {
            return Ok(None);
        };

        if let (Some(store), Some(routine_id), Some(routine_name), Some(registry)) = (
            store,
            routine_id,
            routine_name,
            self.health_registry.as_deref(),
        ) {
            match self
                .resolve_or_create_log_thread(
                    store,
                    registry,
                    routine_id,
                    routine_name,
                    agent_id,
                    discord_thread_id,
                )
                .await
            {
                Ok((target, bot)) => {
                    return Ok(Some(RoutineLogTarget { target, bot }));
                }
                Err(error) => {
                    tracing::warn!(
                        routine_id,
                        agent_id,
                        error = %error,
                        "routine discord thread setup failed; falling back to agent primary channel"
                    );
                }
            }
        }

        let target = match resolve_agent_channel_target(&self.pool, agent_id).await {
            Ok(target) => target,
            Err(error) => {
                return Err(error);
            }
        };

        let bot = match resolve_agent_log_bot(&self.pool, self.health_registry.as_deref(), agent_id)
            .await
        {
            Ok(bot) => bot,
            Err(error) => {
                tracing::warn!("routine log bot resolution failed for {agent_id}: {error}");
                return Err(error);
            }
        };

        Ok(Some(RoutineLogTarget { target, bot }))
    }

    async fn send_or_edit_run_log(
        &self,
        target: &RoutineLogTarget,
        message_id: Option<&str>,
        content: &str,
    ) -> Result<String, String> {
        let channel_id = target
            .target
            .strip_prefix("channel:")
            .unwrap_or(target.target.as_str());
        let token = crate::credential::read_bot_token(&target.bot)
            .or_else(|| crate::credential::read_bot_token("notify"))
            .ok_or_else(|| {
                format!(
                    "routine run Discord summary bot token not configured for {}",
                    target.bot
                )
            })?;
        let client = HttpOutboundClient::new(
            reqwest::Client::new(),
            token,
            crate::server::routes::dispatches::discord_delivery::discord_api_base_url(),
        );
        let policy = DiscordOutboundPolicy::default();

        if let Some(message_id) = message_id.filter(|value| parse_message_id(value).is_some()) {
            let message = DiscordOutboundMessage::new(channel_id.to_string(), content.to_string())
                .with_edit_message_id(message_id.to_string());
            match deliver_outbound(&client, routine_run_log_deduper(), message, policy.clone())
                .await
            {
                DeliveryResult::Success { message_id }
                | DeliveryResult::Fallback { message_id, .. } => return Ok(message_id),
                DeliveryResult::Duplicate {
                    message_id: Some(message_id),
                } => return Ok(message_id),
                DeliveryResult::Duplicate { message_id: None } => {
                    return Err(
                        "routine run Discord summary edit duplicate without message id".to_string(),
                    );
                }
                DeliveryResult::Skipped { reason } => {
                    return Err(format!(
                        "routine run Discord summary edit skipped: {reason:?}"
                    ));
                }
                DeliveryResult::PermanentFailure { detail } => {
                    tracing::warn!(
                        target = %target.target,
                        message_id,
                        error = %detail,
                        "routine run Discord summary edit failed; sending replacement message"
                    );
                }
            }
        }

        let message = DiscordOutboundMessage::new(channel_id.to_string(), content.to_string());
        match deliver_outbound(&client, routine_run_log_deduper(), message, policy).await {
            DeliveryResult::Success { message_id }
            | DeliveryResult::Fallback { message_id, .. } => Ok(message_id),
            DeliveryResult::Duplicate {
                message_id: Some(message_id),
            } => Ok(message_id),
            DeliveryResult::Duplicate { message_id: None } => {
                Err("routine run Discord summary send duplicate without message id".to_string())
            }
            DeliveryResult::Skipped { reason } => Err(format!(
                "routine run Discord summary send skipped: {reason:?}"
            )),
            DeliveryResult::PermanentFailure { detail } => Err(detail),
        }
    }

    async fn resolve_or_create_log_thread(
        &self,
        store: &RoutineStore,
        registry: &HealthRegistry,
        routine_id: &str,
        routine_name: &str,
        agent_id: &str,
        existing_discord_thread_id: Option<&str>,
    ) -> Result<(String, String)> {
        if let Some(thread_id) = existing_discord_thread_id.and_then(parse_channel_id) {
            let bot = validate_routine_thread(registry, &self.pool, agent_id, thread_id).await?;
            return Ok((format!("channel:{}", thread_id.get()), bot));
        }

        if let Some(saved_thread_id) = store
            .get_routine(routine_id)
            .await
            .map_err(|error| anyhow!("load routine {routine_id} before routine log: {error}"))?
            .and_then(|routine| routine.discord_thread_id)
        {
            if let Some(thread_id) = parse_channel_id(&saved_thread_id) {
                match validate_routine_thread(registry, &self.pool, agent_id, thread_id).await {
                    Ok(bot) => return Ok((format!("channel:{}", thread_id.get()), bot)),
                    Err(error) => {
                        tracing::warn!(
                            routine_id,
                            thread_id = saved_thread_id,
                            error = %error,
                            "saved routine discord thread is not reusable; creating replacement"
                        );
                    }
                }
            }
        }

        let bindings = crate::db::agents::load_agent_channel_bindings_pg(&self.pool, agent_id)
            .await
            .map_err(|error| anyhow!("load agent bindings for routine log {agent_id}: {error}"))?
            .ok_or_else(|| anyhow!("agent {agent_id} not found for routine log"))?;
        let provider = bindings
            .resolved_primary_provider_kind()
            .ok_or_else(|| anyhow!("agent {agent_id} has no primary provider for routine log"))?;
        let primary_channel = bindings
            .primary_channel()
            .ok_or_else(|| anyhow!("agent {agent_id} has no primary channel for routine log"))?;
        let parent_channel_id = crate::server::routes::dispatches::resolve_channel_alias_pub(
            &primary_channel,
        )
        .or_else(|| primary_channel.parse::<u64>().ok())
        .ok_or_else(|| {
            anyhow!(
                "agent {agent_id} primary channel is invalid for routine log: {primary_channel}"
            )
        })?;
        let title = routine_thread_title(routine_name, agent_id);
        let (thread_id, bot) = create_routine_thread(
            registry,
            provider.as_str(),
            agent_id,
            poise::serenity_prelude::ChannelId::new(parent_channel_id),
            &title,
        )
        .await?;
        let thread_id_string = thread_id.get().to_string();
        store
            .update_discord_thread_id(routine_id, &thread_id_string)
            .await
            .map_err(|error| anyhow!("persist routine discord_thread_id failed: {error}"))?;
        Ok((format!("channel:{thread_id_string}"), bot))
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

struct RoutineThreadHttp {
    http: Arc<poise::serenity_prelude::Http>,
    bot: String,
}

struct RoutineLogTarget {
    target: String,
    bot: String,
}

fn parse_channel_id(value: &str) -> Option<poise::serenity_prelude::ChannelId> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(poise::serenity_prelude::ChannelId::new)
}

fn parse_message_id(value: &str) -> Option<poise::serenity_prelude::MessageId> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(poise::serenity_prelude::MessageId::new)
}

async fn validate_routine_thread(
    registry: &HealthRegistry,
    pool: &PgPool,
    agent_id: &str,
    thread_id: poise::serenity_prelude::ChannelId,
) -> Result<String> {
    let bindings = crate::db::agents::load_agent_channel_bindings_pg(pool, agent_id)
        .await
        .map_err(|error| anyhow!("load agent bindings for routine log {agent_id}: {error}"))?
        .ok_or_else(|| anyhow!("agent {agent_id} not found for routine log"))?;
    let provider = bindings
        .resolved_primary_provider_kind()
        .ok_or_else(|| anyhow!("agent {agent_id} has no primary provider for routine log"))?;
    let resolved = resolve_routine_thread_http(registry, provider.as_str(), agent_id).await?;
    let channel = thread_id
        .to_channel(&*resolved.http)
        .await
        .map_err(|error| anyhow!("fetch saved routine thread {}: {error}", thread_id.get()))?;
    match channel {
        poise::serenity_prelude::Channel::Guild(channel)
            if matches!(
                channel.kind,
                poise::serenity_prelude::ChannelType::PublicThread
                    | poise::serenity_prelude::ChannelType::PrivateThread
            ) =>
        {
            Ok(resolved.bot)
        }
        _ => Err(anyhow!(
            "saved routine discord_thread_id {} is not a thread",
            thread_id.get()
        )),
    }
}

async fn create_routine_thread(
    registry: &HealthRegistry,
    provider_name: &str,
    agent_id: &str,
    parent_channel_id: poise::serenity_prelude::ChannelId,
    title: &str,
) -> Result<(poise::serenity_prelude::ChannelId, String)> {
    let resolved = resolve_routine_thread_http(registry, provider_name, agent_id).await?;
    let thread = parent_channel_id
        .create_thread(
            &*resolved.http,
            poise::serenity_prelude::builder::CreateThread::new(title)
                .kind(poise::serenity_prelude::ChannelType::PublicThread)
                .auto_archive_duration(poise::serenity_prelude::AutoArchiveDuration::OneDay),
        )
        .await
        .map_err(|error| {
            anyhow!(
                "discord create thread in {}: {error}",
                parent_channel_id.get()
            )
        })?;
    Ok((thread.id, resolved.bot))
}

async fn resolve_routine_thread_http(
    registry: &HealthRegistry,
    provider_name: &str,
    agent_id: &str,
) -> Result<RoutineThreadHttp> {
    let mut errors = Vec::new();
    let mut tried = Vec::new();
    for bot in [provider_name, agent_id, "notify"] {
        if bot.trim().is_empty() || tried.contains(&bot) {
            continue;
        }
        tried.push(bot);
        match resolve_bot_http(registry, bot).await {
            Ok(http) => {
                return Ok(RoutineThreadHttp {
                    http,
                    bot: bot.to_string(),
                });
            }
            Err((_, error)) => errors.push(format!("{bot}: {error}")),
        }
    }
    Err(anyhow!(
        "no routine discord bot available ({})",
        errors.join("; ")
    ))
}

fn routine_thread_title(routine_name: &str, agent_id: &str) -> String {
    let base = format!(
        "routine {} - {}",
        compact_for_title(routine_name),
        compact_for_title(agent_id)
    );
    base.chars().take(90).collect()
}

fn compact_for_title(value: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= 64 {
        collapsed
    } else {
        let mut truncated = collapsed.chars().take(61).collect::<String>();
        truncated.push_str("...");
        truncated
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
    routine_log_block_message(
        &format!("루틴 {}", event.label()),
        vec![(
            "기본",
            vec![
                field_line("routine", &compact(&routine.name, 80)),
                field_line("id", short_id(&routine.id)),
                field_line("script", &script_ref_for_message(&routine.script_ref, 120)),
                field_line("status", &routine.status),
            ],
        )],
    )
}

fn recovery_message(recovered: &RecoveredRoutineRun) -> String {
    routine_log_block_message(
        "루틴 재개",
        vec![(
            "기본",
            vec![
                field_line("reason", "서버 재시작 - routine을 다시 스케줄합니다"),
                field_line("routine", &compact(&recovered.name, 80)),
                field_line("run", short_id(&recovered.run_id)),
                field_line(
                    "script",
                    &script_ref_for_message(&recovered.script_ref, 120),
                ),
            ],
        )],
    )
}

fn run_started_message(claimed: &ClaimedRoutineRun) -> String {
    routine_run_progress_message(&run_started_section(claimed))
}

fn run_started_section(claimed: &ClaimedRoutineRun) -> String {
    routine_log_section(
        "루틴 실행 시작",
        vec![(
            "기본",
            vec![
                field_line("routine", &compact(&claimed.name, 80)),
                field_line("run", short_id(&claimed.run_id)),
                field_line("script", &script_ref_for_message(&claimed.script_ref, 120)),
            ],
        )],
    )
}

fn run_js_inputs_message(
    claimed: &ClaimedRoutineRun,
    observation_count: usize,
    automation_inventory_count: usize,
    checkpoint: Option<&Value>,
) -> String {
    routine_run_progress_message(&run_js_inputs_section(
        claimed,
        observation_count,
        automation_inventory_count,
        checkpoint,
    ))
}

fn run_js_inputs_section(
    claimed: &ClaimedRoutineRun,
    observation_count: usize,
    automation_inventory_count: usize,
    checkpoint: Option<&Value>,
) -> String {
    let checkpoint_summary = checkpoint
        .map(checkpoint_state_summary)
        .unwrap_or_else(|| "없음".to_string());
    routine_log_section(
        "루틴 JS 처리 준비",
        vec![(
            "기본",
            vec![
                field_line("routine", &compact(&claimed.name, 80)),
                field_line("run", short_id(&claimed.run_id)),
                field_line("observations", observation_count.to_string()),
                field_line("inventory", automation_inventory_count.to_string()),
                field_line("checkpoint", checkpoint_summary),
            ],
        )],
    )
}

fn checkpoint_state_summary(cp: &Value) -> String {
    let mut state_counts: std::collections::BTreeMap<&str, usize> =
        std::collections::BTreeMap::new();
    if let Some(candidates) = cp.get("candidates").and_then(Value::as_object) {
        for candidate in candidates.values() {
            let state = candidate
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            *state_counts.entry(state).or_insert(0) += 1;
        }
    }
    let mut parts: Vec<String> = state_counts
        .iter()
        .map(|(state, count)| format!("{state}:{count}"))
        .collect();
    if let Some(latest_rec) = cp
        .get("recommendations")
        .and_then(Value::as_array)
        .and_then(|recs| recs.last())
    {
        let pattern_id = latest_rec
            .get("pattern_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let summary = latest_rec
            .get("outcome_summary")
            .or_else(|| latest_rec.get("decision_summary"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !pattern_id.is_empty() {
            let rec_line = if summary.is_empty() {
                format!("최근 추천: {}", compact(pattern_id, 60))
            } else {
                format!(
                    "최근 추천: {} - {}",
                    compact(pattern_id, 60),
                    compact(summary, 120)
                )
            };
            parts.push(rec_line);
        }
    }
    if parts.is_empty() {
        "비어 있음".to_string()
    } else {
        parts.join(" / ")
    }
}

fn run_js_action_message(
    claimed: &ClaimedRoutineRun,
    action: &str,
    summary: Option<&str>,
    prompt: Option<&str>,
    checkpoint_update: bool,
) -> String {
    routine_run_progress_message(&run_js_action_section(
        claimed,
        action,
        summary,
        prompt,
        checkpoint_update,
    ))
}

fn run_js_action_section(
    claimed: &ClaimedRoutineRun,
    action: &str,
    summary: Option<&str>,
    prompt: Option<&str>,
    checkpoint_update: bool,
) -> String {
    let mut sections = vec![(
        "기본",
        vec![
            field_line("routine", &compact(&claimed.name, 80)),
            field_line("run", short_id(&claimed.run_id)),
            field_line("action", format!("\"{}\"", action)),
            field_line("checkpoint", present_label(checkpoint_update)),
        ],
    )];
    if let Some(summary) = summary.filter(|value| !value.trim().is_empty()) {
        sections.push((
            "요약",
            vec![field_line("js", compact_multiline(summary, 600))],
        ));
    }
    if let Some(prompt) = prompt.filter(|value| !value.trim().is_empty()) {
        sections.push((
            "프롬프트",
            vec![field_line("agent", compact_multiline(prompt, 900))],
        ));
    }
    routine_log_section("루틴 JS 처리 결과", sections)
}

fn run_outcome_message(routine: &RoutineRecord, outcome: &RoutineRunOutcome) -> String {
    routine_run_progress_message(&run_outcome_section(routine, outcome))
}

fn run_outcome_section(routine: &RoutineRecord, outcome: &RoutineRunOutcome) -> String {
    let mut basic = vec![
        field_line("routine", &compact(&routine.name, 80)),
        field_line("run", short_id(&outcome.run_id)),
        field_line("action", format!("\"{}\"", outcome.action)),
        field_line("status", format!("\"{}\"", outcome.status)),
    ];
    if let Some(error) = outcome
        .error
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        basic.push(field_line("error", compact(error, 160)));
    }
    let mut sections = vec![("기본", basic)];
    if let Some(summary) = outcome_summary_for_message(outcome) {
        sections.push(("요약", vec![field_line("result", compact(&summary, 220))]));
    }
    if let Some(response) = agent_response_preview(outcome.result_json.as_ref()) {
        sections.push((
            "에이전트 응답",
            vec![field_line("preview", compact_multiline(&response, 900))],
        ));
    }
    routine_log_section("루틴 실행 결과", sections)
}

fn routine_log_block_message(title: &str, sections: Vec<(&str, Vec<String>)>) -> String {
    let body = routine_log_sections_body(sections);
    format!(
        "[{}] {}\n```text\n{}\n```",
        routine_log_timestamp(),
        title,
        body
    )
}

fn routine_log_section(title: &str, sections: Vec<(&str, Vec<String>)>) -> String {
    format!(
        "[{}] {}\n{}",
        routine_log_timestamp(),
        title,
        routine_log_sections_body(sections)
    )
}

fn routine_log_sections_body(sections: Vec<(&str, Vec<String>)>) -> String {
    let mut lines = Vec::new();
    for (section, section_lines) in sections {
        if section_lines.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!("[{}]", sanitize_code_block(section)));
        lines.extend(
            section_lines
                .into_iter()
                .map(|line| sanitize_code_block(&line)),
        );
    }
    lines.join("\n")
}

fn routine_run_progress_message(body: &str) -> String {
    format!(
        "[{}] 루틴 실행 로그\n```text\n{}\n```",
        routine_log_timestamp(),
        sanitize_code_block(body)
    )
}

fn render_run_log_sections(sections: &Value) -> String {
    let Some(object) = sections.as_object() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for key in RUN_LOG_SECTION_ORDER {
        if let Some(section) = object.get(key).and_then(Value::as_str) {
            parts.push(section.to_string());
        }
    }
    let mut extra_keys: Vec<_> = object
        .keys()
        .filter(|key| !RUN_LOG_SECTION_ORDER.contains(&key.as_str()))
        .cloned()
        .collect();
    extra_keys.sort();
    for key in extra_keys {
        if let Some(section) = object.get(&key).and_then(Value::as_str) {
            parts.push(section.to_string());
        }
    }
    parts.join("\n\n")
}

fn field_line(label: &str, value: impl AsRef<str>) -> String {
    format!("{label}: {}", value.as_ref())
}

fn sanitize_code_block(value: &str) -> String {
    value.replace("```", "'''")
}

fn outcome_summary_for_message(outcome: &RoutineRunOutcome) -> Option<String> {
    let result = outcome.result_json.as_ref()?;
    for key in ["outcome_summary", "summary", "reason", "error"] {
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

fn compact_multiline(value: &str, max_chars: usize) -> String {
    compact(&value.replace('\r', "").replace('\n', " / "), max_chars)
}

fn present_label(value: bool) -> &'static str {
    if value { "있음" } else { "없음" }
}

fn agent_response_preview(result_json: Option<&Value>) -> Option<String> {
    result_json?
        .get("assistant_message_preview")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
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
        assert!(message.contains("] 루틴 실행 로그\n```text"));
        assert!(message.contains("루틴 실행 결과"));
        assert!(message.contains("Daily Summary"));
        assert!(message.contains("run: run-1234"));
        assert!(message.contains("action: \"complete\""));
        assert!(message.contains("status: \"failed\""));
        assert!(message.contains("error: boom"));
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

        assert!(message.contains("[요약]"));
        assert!(message.contains("result: 성공 요약: 새 자동화 추천 후보 없음"));
    }

    #[test]
    fn run_outcome_message_includes_agent_response_preview() {
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
            action: "agent".to_string(),
            status: "succeeded".to_string(),
            result_json: Some(json!({
                "assistant_message_preview": "자동화 후보는 보류합니다.\n근거가 부족합니다."
            })),
            error: None,
            fresh_context_guaranteed: false,
        };

        let message = run_outcome_message(&routine, &outcome);

        assert!(message.contains("[에이전트 응답]"));
        assert!(message.contains("preview: 자동화 후보는 보류합니다. / 근거가 부족합니다."));
    }

    #[test]
    fn run_js_action_message_includes_prompt_preview() {
        let claimed = ClaimedRoutineRun {
            run_id: "21e14c13-0000-0000-0000-000000000000".to_string(),
            routine_id: "routine-123456789".to_string(),
            agent_id: Some("monitoring".to_string()),
            script_ref: "monitoring/automation-candidate-recommender.js".to_string(),
            name: "Automation Candidate Recommender".to_string(),
            execution_strategy: "fresh".to_string(),
            checkpoint: Some(json!({"version": 1})),
            discord_thread_id: None,
            timeout_secs: None,
            lease_expires_at: Utc::now(),
        };

        let message = run_js_action_message(
            &claimed,
            "agent",
            Some("agent prompt generated (123 chars)"),
            Some("# 자동화 후보 추천\n근거: 반복 실패"),
            true,
        );

        assert!(message.contains("루틴 JS 처리 결과"));
        assert!(message.contains("```text"));
        assert!(message.contains("action: \"agent\""));
        assert!(message.contains("checkpoint: 있음"));
        assert!(message.contains("[요약]"));
        assert!(message.contains("js: agent prompt generated"));
        assert!(message.contains("[프롬프트]"));
        assert!(message.contains("agent: # 자동화 후보 추천 / 근거: 반복 실패"));
    }

    #[test]
    fn run_js_inputs_message_summarizes_checkpoint_state() {
        let claimed = ClaimedRoutineRun {
            run_id: "21e14c13-0000-0000-0000-000000000000".to_string(),
            routine_id: "routine-123456789".to_string(),
            agent_id: Some("monitoring".to_string()),
            script_ref: "monitoring/automation-candidate-recommender.js".to_string(),
            name: "Automation Candidate Recommender".to_string(),
            execution_strategy: "fresh".to_string(),
            checkpoint: Some(json!({"version": 1})),
            discord_thread_id: None,
            timeout_secs: None,
            lease_expires_at: Utc::now(),
        };
        let checkpoint = json!({
            "candidates": {
                "api-friction:docs-bypass": {"state": "observing"},
                "ops/retry.js:complete": {"state": "recommended"}
            },
            "recommendations": [{
                "pattern_id": "api-friction:docs-bypass",
                "outcome_summary": "실패 요약: API 문서 우회가 반복됩니다."
            }]
        });

        let message = run_js_inputs_message(&claimed, 4, 2, Some(&checkpoint));

        assert!(message.contains("checkpoint: observing:1 / recommended:1"));
        assert!(message.contains("최근 추천: api-friction:docs-bypass - 실패 요약"));
        assert!(!message.contains("\"candidates\""));
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
        assert!(message.contains("] 루틴 실행 로그\n```text"));
        assert!(message.contains("루틴 실행 시작"));
        assert!(message.contains("routine: monitoring-working-watchdog"));
        assert!(message.contains("run: 21e14c13"));
        assert!(message.contains("script: monitoring / working-watchdog.js"));
    }

    #[test]
    fn render_run_log_sections_keeps_stable_order_and_sanitizes_code_fences() {
        let sections = json!({
            "outcome": "[2026] 루틴 실행 결과\n[기본]\nstatus: \"succeeded\"",
            "started": "[2026] 루틴 실행 시작\n[기본]\nroutine: demo",
            "js_action": "[2026] 루틴 JS 처리 결과\n[프롬프트]\nagent: ```danger```",
            "js_inputs": "[2026] 루틴 JS 처리 준비\n[기본]\nobservations: 2"
        });

        let body = render_run_log_sections(&sections);
        let message = routine_run_progress_message(&body);

        assert!(body.find("루틴 실행 시작").unwrap() < body.find("루틴 JS 처리 준비").unwrap());
        assert!(body.find("루틴 JS 처리 준비").unwrap() < body.find("루틴 JS 처리 결과").unwrap());
        assert!(body.find("루틴 JS 처리 결과").unwrap() < body.find("루틴 실행 결과").unwrap());
        assert!(message.contains("```text"));
        assert!(message.contains("agent: '''danger'''"));
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

    #[test]
    fn checkpoint_state_summary_formats_counts_and_latest_recommendation() {
        let cp = serde_json::json!({
            "candidates": {
                "routine-candidate:foo": {"state": "watching"},
                "routine-candidate:bar": {"state": "watching"},
                "routine-candidate:baz": {"state": "recommended"},
                "routine-candidate:qux": {"state": "suppressed"}
            },
            "recommendations": [
                {"pattern_id": "routine-candidate:old", "outcome_summary": "이전 요약"},
                {"pattern_id": "routine-candidate:baz", "outcome_summary": "반복 패턴 5회, 루틴화 후보"}
            ]
        });
        let summary = checkpoint_state_summary(&cp);
        assert!(
            summary.contains("recommended:1"),
            "must show recommended count: {summary}"
        );
        assert!(
            summary.contains("suppressed:1"),
            "must show suppressed count: {summary}"
        );
        assert!(
            summary.contains("watching:2"),
            "must show watching count: {summary}"
        );
        assert!(
            summary.contains("routine-candidate:baz"),
            "must show latest pattern_id: {summary}"
        );
        assert!(
            summary.contains("반복 패턴 5회"),
            "must show outcome_summary: {summary}"
        );
    }

    #[test]
    fn checkpoint_state_summary_empty_checkpoint() {
        let cp = serde_json::json!({});
        assert_eq!(checkpoint_state_summary(&cp), "비어 있음");
    }
}
