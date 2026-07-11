//! Durable task-notification card authority (#4055).
//!
//! Prompt observation and terminal response delivery both converge here. A
//! semantic event owns one durable row and one stable Discord nonce; only this
//! module may create, edit, or replace its completion card.

mod gateway;
mod store;

#[cfg(test)]
mod tests;

use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::session_backend::{StreamLineState, classify_task_notification_kind};

pub(super) use gateway::{
    CardBot, CardDeliveryClients, DiscordTaskCardTransport, TaskCardTransport,
    TaskCardTransportError,
};

use self::store::{CardClaim, ClaimedCard, StoreIntent};

/// Provider-normalized context retained beside terminal response text.
///
/// Only sanitized semantic fields cross the parser/sink boundary. Raw provider
/// envelopes and `agent_path` never do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TaskNotificationContext {
    task_id: Option<String>,
    tool_use_id: Option<String>,
    status: String,
    summary: String,
    kind: String,
    event_key: String,
}

impl TaskNotificationContext {
    pub(super) fn from_stream_json(
        value: &serde_json::Value,
        state: &StreamLineState,
    ) -> Option<Self> {
        if value.get("type").and_then(serde_json::Value::as_str) != Some("system")
            || value.get("subtype").and_then(serde_json::Value::as_str) != Some("task_notification")
        {
            return None;
        }

        let task_id = clean_optional(value.get("task_id").and_then(serde_json::Value::as_str));
        let task_info = task_id.as_deref().and_then(|id| state.task_starts.get(id));
        let tool_use_id = ["tool_use_id", "tool-use-id", "toolUseId"]
            .into_iter()
            .find_map(|key| value.get(key).and_then(serde_json::Value::as_str))
            .and_then(|raw| clean_optional(Some(raw)))
            .or_else(|| {
                task_info
                    .and_then(|info| info.tool_use_id.clone())
                    .and_then(clean_owned)
            });
        let status = clean_line(
            value
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
        );
        let summary = clean_line(
            value
                .get("summary")
                .or_else(|| value.get("description"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
        );
        let kind = value
            .get("task_notification_kind")
            .and_then(serde_json::Value::as_str)
            .map(clean_line)
            .filter(|kind| !kind.is_empty())
            .unwrap_or_else(|| {
                classify_task_notification_kind(value, state)
                    .as_str()
                    .to_string()
            });
        let payload_fingerprint = normalized_task_payload_fingerprint(
            task_id.as_deref(),
            tool_use_id.as_deref(),
            &status,
            &summary,
        );
        let event_key = semantic_event_key(
            task_id.as_deref(),
            tool_use_id.as_deref(),
            None,
            &payload_fingerprint,
        );

        Some(Self {
            task_id,
            tool_use_id,
            status,
            summary,
            kind,
            event_key,
        })
    }

    pub(super) fn routing_kind(&self) -> TaskNotificationKind {
        match self.kind.as_str() {
            "subagent" => TaskNotificationKind::Subagent,
            "monitor_auto_turn" => TaskNotificationKind::MonitorAutoTurn,
            _ => TaskNotificationKind::Background,
        }
    }

    pub(super) fn event_key(&self) -> &str {
        &self.event_key
    }

    pub(super) fn to_event(
        &self,
        channel_id: u64,
        provider: &str,
        session_key: &str,
    ) -> TaskCardEvent {
        let note = super::tui_task_card::TaskNotification {
            task_id: self.task_id.clone(),
            tool_use_id: self.tool_use_id.clone(),
            status: clean_optional(Some(&self.status)),
            summary: clean_optional(Some(&self.summary)),
            ..Default::default()
        };
        TaskCardEvent {
            scope: TaskCardScope::new(channel_id, provider, session_key, self.event_key.clone()),
            task_id: self.task_id.clone(),
            tool_use_id: self.tool_use_id.clone(),
            kind: self.kind.clone(),
            payload: TaskCardPayload::Task(note),
        }
    }
}

/// Keeps the same priority rule as the pre-existing kind merge while retaining
/// the complete context belonging to the winning kind.
pub(super) fn merge_context(
    current: Option<TaskNotificationContext>,
    next: TaskNotificationContext,
) -> Option<TaskNotificationContext> {
    let priority = |kind: TaskNotificationKind| match kind {
        TaskNotificationKind::Subagent => 0,
        TaskNotificationKind::Background => 1,
        TaskNotificationKind::MonitorAutoTurn => 2,
    };
    match current {
        Some(existing) if priority(existing.routing_kind()) > priority(next.routing_kind()) => {
            Some(existing)
        }
        _ => Some(next),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct TaskCardScope {
    channel_id: u64,
    provider: String,
    session_key: String,
    event_key: String,
}

impl TaskCardScope {
    fn new(
        channel_id: u64,
        provider: impl Into<String>,
        session_key: impl Into<String>,
        event_key: impl Into<String>,
    ) -> Self {
        Self {
            channel_id,
            provider: provider.into().trim().to_ascii_lowercase(),
            session_key: session_key.into(),
            event_key: event_key.into(),
        }
    }
}

pub(super) fn provider_bot_key(provider: &str) -> String {
    format!("provider:{}", provider.trim().to_ascii_lowercase())
}

#[derive(Clone, Debug)]
enum TaskCardPayload {
    Task(super::tui_task_card::TaskNotification),
    Subagent(String),
}

impl TaskCardPayload {
    fn render(&self, update_count: u64) -> String {
        match self {
            Self::Task(note) => {
                super::tui_task_card::format_task_notification_card(note, update_count)
            }
            Self::Subagent(card) if update_count > 1 => {
                format!("{card}\n\n-# {update_count} updates")
            }
            Self::Subagent(card) => card.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct TaskCardEvent {
    scope: TaskCardScope,
    task_id: Option<String>,
    tool_use_id: Option<String>,
    kind: String,
    payload: TaskCardPayload,
}

impl TaskCardEvent {
    pub(super) fn from_task_prompt(
        channel_id: u64,
        provider: &str,
        session_key: &str,
        raw_prompt: &str,
    ) -> Self {
        let note = super::tui_task_card::parse_task_notification(raw_prompt);
        let task_id = note.task_id.clone().and_then(clean_owned);
        let tool_use_id = note.tool_use_id.clone().and_then(clean_owned);
        let kind = note.kind().to_string();
        let normalized_payload = normalized_task_payload_fingerprint(
            task_id.as_deref(),
            tool_use_id.as_deref(),
            note.status.as_deref().unwrap_or(""),
            note.summary.as_deref().unwrap_or(""),
        );
        let event_key = semantic_event_key(
            task_id.as_deref(),
            tool_use_id.as_deref(),
            None,
            &normalized_payload,
        );
        Self {
            scope: TaskCardScope::new(channel_id, provider, session_key, event_key),
            task_id,
            tool_use_id,
            kind,
            payload: TaskCardPayload::Task(note),
        }
    }

    pub(super) fn from_subagent_prompt(
        channel_id: u64,
        provider: &str,
        session_key: &str,
        raw_prompt: &str,
    ) -> Self {
        let semantic = super::response_sanitizer::subagent_notification_card::semantic_event(
            raw_prompt,
        )
        .unwrap_or_else(|| {
            // A malformed machine envelope must still receive one safe durable
            // card. Hash the normalized input for identity, but retain none of
            // its raw fields in state, logs, or rendered content.
            super::response_sanitizer::subagent_notification_card::SubagentNotificationSemantic {
                task_id: None,
                tool_use_id: None,
                agent_path_fingerprint: None,
                payload_fingerprint: fingerprint(&[
                    "malformed-subagent",
                    &super::tui_task_card::strip_terminal_controls(raw_prompt),
                ]),
            }
        });
        let event_key = semantic_event_key(
            semantic.task_id.as_deref(),
            semantic.tool_use_id.as_deref(),
            semantic.agent_path_fingerprint.as_deref(),
            &semantic.payload_fingerprint,
        );
        Self {
            scope: TaskCardScope::new(channel_id, provider, session_key, event_key),
            task_id: semantic.task_id,
            tool_use_id: semantic.tool_use_id,
            kind: "subagent".to_string(),
            payload: TaskCardPayload::Subagent(
                super::response_sanitizer::subagent_notification_card::format_subagent_notification_card(
                    Some(session_key),
                    raw_prompt,
                ),
            ),
        }
    }

    pub(super) fn supports_footer_deferral(&self) -> bool {
        self.task_id.is_some() || self.tool_use_id.is_some()
    }

    pub(super) fn tool_use_id(&self) -> Option<&str> {
        self.tool_use_id.as_deref()
    }

    pub(super) fn kind(&self) -> &str {
        &self.kind
    }

    #[cfg(test)]
    pub(super) fn event_key(&self) -> &str {
        &self.scope.event_key
    }
}

pub(in crate::services::discord) fn response_turn_key(
    user_msg_id: u64,
    started_at: &str,
    turn_start_offset: Option<u64>,
) -> String {
    full_fingerprint(&[
        "task-response-turn-v1",
        &user_msg_id.to_string(),
        started_at,
        &turn_start_offset
            .map(|offset| offset.to_string())
            .unwrap_or_else(|| "legacy-none".to_string()),
    ])
}

pub(in crate::services::discord) async fn bind_task_response_turn(
    pool: Option<&PgPool>,
    channel_id: u64,
    provider: &str,
    session_key: &str,
    event_key: &str,
    response_turn_key: &str,
    card_message_id: u64,
) -> Result<(), String> {
    let scope = TaskCardScope::new(channel_id, provider, session_key, event_key);
    store::bind_response_turn(pool, &scope, response_turn_key, card_message_id).await
}

pub(in crate::services::discord) async fn task_response_fallback_must_wait(
    pool: Option<&PgPool>,
    channel_id: u64,
    provider: &str,
    session_key: &str,
    event_key: Option<&str>,
    response_turn_key: Option<&str>,
) -> Result<bool, String> {
    store::response_fallback_must_wait(
        pool,
        channel_id,
        provider,
        session_key,
        event_key,
        response_turn_key,
    )
    .await
}

pub(in crate::services::discord) async fn mark_task_response_delivered(
    pool: Option<&PgPool>,
    channel_id: u64,
    provider: &str,
    session_key: &str,
    event_key: &str,
    card_message_id: u64,
) -> Result<(), String> {
    let scope = TaskCardScope::new(channel_id, provider, session_key, event_key);
    store::mark_response_delivered(pool, &scope, card_message_id).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EnsureIntent {
    Observation,
    Promotion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CardDisposition {
    Created,
    Existing,
    Edited,
    Replaced,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CardEnsureOutcome {
    pub(super) message_id: u64,
    pub(super) bot_key: String,
    pub(super) disposition: CardDisposition,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum CardEnsureError {
    #[error("task card delivery is busy: {0}")]
    Busy(String),
    #[error("task card delivery is transient: {0}")]
    Transient(String),
    #[error("task card delivery failed permanently: {0}")]
    Permanent(String),
    #[error("task card state error: {0}")]
    Store(String),
}

pub(super) async fn record_footer_only(
    pool: Option<&PgPool>,
    event: &TaskCardEvent,
) -> Result<(), CardEnsureError> {
    let content = event.payload.render(1);
    store::record_footer_only(pool, &event.scope, &content, &content_hash(&content))
        .await
        .map_err(CardEnsureError::Store)
}

pub(super) async fn ensure_card<T: TaskCardTransport>(
    pool: Option<&PgPool>,
    clients: &CardDeliveryClients,
    transport: &T,
    event: &TaskCardEvent,
    intent: EnsureIntent,
) -> Result<CardEnsureOutcome, CardEnsureError> {
    let preferred = clients.preferred().ok_or_else(|| {
        CardEnsureError::Transient("no notify/provider Discord bot is available".to_string())
    })?;
    let seed_content = event.payload.render(1);
    for attempt in 0..20 {
        let claim = store::claim_card(
            pool,
            &event.scope,
            &preferred.key,
            &seed_content,
            &content_hash(&seed_content),
            match intent {
                EnsureIntent::Observation => StoreIntent::Observation,
                EnsureIntent::Promotion => StoreIntent::Promotion,
            },
        )
        .await
        .map_err(CardEnsureError::Store)?;
        match claim {
            CardClaim::Existing {
                message_id,
                bot_key,
            } => {
                return Ok(CardEnsureOutcome {
                    message_id,
                    bot_key,
                    disposition: CardDisposition::Existing,
                });
            }
            CardClaim::Owned(claimed) => {
                return deliver_claim(pool, clients, transport, event, intent, claimed).await;
            }
            CardClaim::Busy { .. } if attempt < 19 => {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            CardClaim::Busy { bot_key } => {
                return Err(CardEnsureError::Busy(format!(
                    "another worker owns the card lease (bot={bot_key})"
                )));
            }
        }
    }
    unreachable!("bounded card lease loop returns on its last attempt")
}

async fn deliver_claim<T: TaskCardTransport>(
    pool: Option<&PgPool>,
    clients: &CardDeliveryClients,
    transport: &T,
    event: &TaskCardEvent,
    intent: EnsureIntent,
    claimed: ClaimedCard,
) -> Result<CardEnsureOutcome, CardEnsureError> {
    let Some(bot) = clients.by_key(&claimed.bot_key) else {
        let error = format!("the card's pinned bot {} is unavailable", claimed.bot_key);
        match claimed.action {
            store::ClaimAction::Post => store::mark_post_failure(pool, &claimed, &error).await,
            store::ClaimAction::Edit { message_id } => {
                store::mark_edit_failure(pool, &claimed, message_id, &error).await
            }
        }
        .map_err(CardEnsureError::Store)?;
        return Err(CardEnsureError::Transient(error));
    };
    let content = match (&claimed.action, intent) {
        (store::ClaimAction::Post, EnsureIntent::Promotion)
            if !claimed.rendered_content.is_empty() =>
        {
            claimed.rendered_content.clone()
        }
        _ => event.payload.render(claimed.update_count),
    };
    let hash = content_hash(&content);

    match claimed.action {
        store::ClaimAction::Post => {
            match transport
                .post_card(
                    bot,
                    event.scope.channel_id,
                    &content,
                    &claimed.discord_nonce,
                )
                .await
            {
                Ok(message_id) => {
                    store::mark_posted(pool, &claimed, message_id, &content, &hash)
                        .await
                        .map_err(CardEnsureError::Store)?;
                    Ok(CardEnsureOutcome {
                        message_id,
                        bot_key: claimed.bot_key,
                        disposition: if claimed.revision > 1 {
                            CardDisposition::Replaced
                        } else {
                            CardDisposition::Created
                        },
                    })
                }
                Err(error) => {
                    store::mark_post_failure(pool, &claimed, &error.to_string())
                        .await
                        .map_err(CardEnsureError::Store)?;
                    Err(map_transport_error(error))
                }
            }
        }
        store::ClaimAction::Edit { message_id } => {
            match transport
                .edit_card(bot, event.scope.channel_id, message_id, &content)
                .await
            {
                Ok(()) => {
                    store::mark_edited(pool, &claimed, message_id, &content, &hash)
                        .await
                        .map_err(CardEnsureError::Store)?;
                    Ok(CardEnsureOutcome {
                        message_id,
                        bot_key: claimed.bot_key,
                        disposition: CardDisposition::Edited,
                    })
                }
                Err(TaskCardTransportError::ConfirmedMissing(_error)) => {
                    let replacement =
                        store::prepare_replacement(pool, &claimed, message_id, &content, &hash)
                            .await
                            .map_err(CardEnsureError::Store)?;
                    match transport
                        .post_card(
                            bot,
                            event.scope.channel_id,
                            &content,
                            &replacement.discord_nonce,
                        )
                        .await
                    {
                        Ok(replacement_id) => {
                            store::mark_posted(pool, &replacement, replacement_id, &content, &hash)
                                .await
                                .map_err(CardEnsureError::Store)?;
                            Ok(CardEnsureOutcome {
                                message_id: replacement_id,
                                bot_key: replacement.bot_key,
                                disposition: CardDisposition::Replaced,
                            })
                        }
                        Err(post_error) => {
                            store::mark_post_failure(pool, &replacement, &post_error.to_string())
                                .await
                                .map_err(CardEnsureError::Store)?;
                            Err(map_transport_error(post_error))
                        }
                    }
                }
                Err(error) => {
                    store::mark_edit_failure(pool, &claimed, message_id, &error.to_string())
                        .await
                        .map_err(CardEnsureError::Store)?;
                    Err(map_transport_error(error))
                }
            }
        }
    }
}

fn map_transport_error(error: TaskCardTransportError) -> CardEnsureError {
    match error {
        TaskCardTransportError::Transient(error) => CardEnsureError::Transient(error),
        TaskCardTransportError::ConfirmedMissing(error)
        | TaskCardTransportError::Permanent(error) => CardEnsureError::Permanent(error),
    }
}

fn semantic_event_key(
    task_id: Option<&str>,
    tool_use_id: Option<&str>,
    agent_path_fingerprint: Option<&str>,
    payload_fingerprint: &str,
) -> String {
    if let Some(task_id) = task_id.filter(|value| !value.trim().is_empty()) {
        return format!("task:{}", fingerprint(&[task_id]));
    }
    if let Some(tool_use_id) = tool_use_id.filter(|value| !value.trim().is_empty()) {
        return format!("tool:{}", fingerprint(&[tool_use_id]));
    }
    if let Some(agent_path) = agent_path_fingerprint.filter(|value| !value.trim().is_empty()) {
        return format!("agent:{agent_path}");
    }
    format!("payload:{payload_fingerprint}")
}

fn normalized_task_payload_fingerprint(
    task_id: Option<&str>,
    tool_use_id: Option<&str>,
    status: &str,
    summary: &str,
) -> String {
    let task_id = task_id.map(clean_line).unwrap_or_default();
    let tool_use_id = tool_use_id.map(clean_line).unwrap_or_default();
    let status = clean_line(status);
    let summary = clean_line(summary);
    fingerprint(&[&task_id, &tool_use_id, &status, &summary])
}

pub(super) fn stable_nonce(scope: &TaskCardScope, revision: i32) -> String {
    let digest = full_fingerprint(&[
        "task-card-nonce-v1",
        &scope.channel_id.to_string(),
        &scope.provider,
        &scope.session_key,
        &scope.event_key,
        &revision.to_string(),
    ]);
    format!("adktn{}", &digest[..20])
}

fn content_hash(content: &str) -> String {
    full_fingerprint(&[content])
}

fn fingerprint(parts: &[&str]) -> String {
    full_fingerprint(parts).chars().take(16).collect()
}

fn full_fingerprint(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn clean_line(value: &str) -> String {
    super::tui_task_card::strip_terminal_controls(value)
        .replace(['\r', '\n'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn clean_optional(value: Option<&str>) -> Option<String> {
    value.map(clean_line).filter(|value| !value.is_empty())
}

fn clean_owned(value: String) -> Option<String> {
    clean_optional(Some(&value))
}
