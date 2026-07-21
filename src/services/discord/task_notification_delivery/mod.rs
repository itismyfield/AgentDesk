//! Durable task-notification card authority (#4055).
//!
//! Prompt observation and terminal response delivery both converge here. A
//! semantic event owns one durable row and one stable Discord nonce; only this
//! module may create, edit, or replace its completion card.

mod card_post;
mod gateway;
mod response_chunks;
mod store;
mod terminal_identity;

#[cfg(test)]
mod tests;

use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::session_backend::{StreamLineState, classify_task_notification_kind};

use self::card_post::deliver_card_post_claim;
pub(super) use gateway::{
    CardBot, CardDeliveryClients, CardPostReconcile, DiscordTaskCardTransport, TaskCardTransport,
    TaskCardTransportError,
};
#[cfg(test)]
pub(in crate::services::discord) use response_chunks::send_task_response_chunks;
pub(in crate::services::discord) use response_chunks::{
    DiscordResponseChunkTransport, ResponseChunkDeliveryError,
    send_task_response_chunks_with_card_repair,
};

use self::store::{CardClaim, ClaimedCard, StoreIntent};
pub(super) use self::store::{
    ExistingResponseDelivery, ResponseDeliveryClaim, ResponseDeliveryClaimOutcome,
    ResponseDeliveryOwner,
};

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

#[derive(Clone, Debug)]
pub(super) struct TaskCardScope {
    channel_id: u64,
    provider: String,
    session_key: String,
    event_key: String,
    terminal_delivery_fingerprint: Option<String>,
}

impl PartialEq for TaskCardScope {
    fn eq(&self, other: &Self) -> bool {
        self.channel_id == other.channel_id
            && self.provider == other.provider
            && self.session_key == other.session_key
            && self.event_key == other.event_key
    }
}

impl Eq for TaskCardScope {}

impl std::hash::Hash for TaskCardScope {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.channel_id.hash(state);
        self.provider.hash(state);
        self.session_key.hash(state);
        self.event_key.hash(state);
    }
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
            terminal_delivery_fingerprint: None,
        }
    }

    fn with_terminal_delivery_fingerprint(mut self, fingerprint: String) -> Self {
        self.terminal_delivery_fingerprint = Some(fingerprint);
        self
    }
}

pub(super) fn provider_bot_key(provider: &str) -> String {
    format!("provider:{}", provider.trim().to_ascii_lowercase())
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
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
    /// A restored watcher can observe terminal task output without the original
    /// provider envelope. Give that turn a deterministic, sanitized card rather
    /// than waiting forever for context that can no longer arrive.
    pub(super) fn from_recovered_terminal(
        channel_id: u64,
        provider: &str,
        session_key: &str,
        kind: TaskNotificationKind,
        response_turn_key: &str,
    ) -> Self {
        let note = super::tui_task_card::TaskNotification {
            status: Some("completed".to_string()),
            summary: Some("Recovered task completion".to_string()),
            ..Default::default()
        };
        Self {
            scope: TaskCardScope::new(
                channel_id,
                provider,
                session_key,
                format!("turn:{response_turn_key}"),
            ),
            task_id: None,
            tool_use_id: None,
            kind: kind.as_str().to_string(),
            payload: TaskCardPayload::Task(note),
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

    pub(super) fn event_key(&self) -> &str {
        &self.scope.event_key
    }

    pub(in crate::services::discord) fn with_persisted_event_key(
        mut self,
        event_key: impl Into<String>,
    ) -> Self {
        self.scope.event_key = event_key.into();
        self
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

pub(in crate::services::discord) fn fallback_response_turn_key(
    channel_id: u64,
    provider: &str,
    session_key: &str,
    end_offset: u64,
    response: &str,
) -> String {
    full_fingerprint(&[
        "task-response-recovered-v1",
        &channel_id.to_string(),
        &provider.trim().to_ascii_lowercase(),
        session_key,
        &end_offset.to_string(),
        &full_fingerprint(&[response]),
    ])
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn durable_response_turn_key(
    channel_id: u64,
    provider: &str,
    session_key: &str,
    user_msg_id: u64,
    started_at: &str,
    turn_start_offset: Option<u64>,
    end_offset: u64,
    response: &str,
) -> String {
    if user_msg_id != 0 || (!started_at.trim().is_empty() && turn_start_offset.is_some()) {
        response_turn_key(user_msg_id, started_at, turn_start_offset)
    } else {
        fallback_response_turn_key(channel_id, provider, session_key, end_offset, response)
    }
}

/// Discord only reconciles duplicate creates when every retry reuses the same
/// nonce. Bind each physical reply chunk to the durable response turn and its
/// stable chunk index so sink/watcher takeover cannot create a second copy.
pub(in crate::services::discord) fn response_chunk_nonce(
    response_turn_key: &str,
    chunk_index: usize,
) -> String {
    response_chunk_nonce_for_generation(response_turn_key, 1, chunk_index)
}

pub(in crate::services::discord) fn response_chunk_nonce_for_generation(
    response_turn_key: &str,
    response_generation: i32,
    chunk_index: usize,
) -> String {
    let digest = full_fingerprint(&[
        "task-response-chunk-nonce-v2",
        response_turn_key,
        &response_generation.to_string(),
        &chunk_index.to_string(),
    ]);
    format!("adktr{}", &digest[..20])
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn claim_task_response_delivery(
    pool: Option<&PgPool>,
    channel_id: u64,
    provider: &str,
    session_key: &str,
    event_key: &str,
    response_turn_key: &str,
    card_message_id: u64,
    owner: ResponseDeliveryOwner,
) -> Result<ResponseDeliveryClaimOutcome, String> {
    claim_task_response_delivery_with_recovery_key(
        pool,
        channel_id,
        provider,
        session_key,
        event_key,
        response_turn_key,
        // A self-alias makes the recovery-key uniqueness constraint symmetric:
        // if a watcher persists its fallback/content key first, a later sink
        // frame-key claim with that fallback alias conflicts onto the same row
        // instead of creating a second delivery authority.
        Some(response_turn_key),
        card_message_id,
        owner,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn claim_task_response_delivery_with_recovery_key(
    pool: Option<&PgPool>,
    channel_id: u64,
    provider: &str,
    session_key: &str,
    event_key: &str,
    response_turn_key: &str,
    recovery_turn_key: Option<&str>,
    card_message_id: u64,
    owner: ResponseDeliveryOwner,
) -> Result<ResponseDeliveryClaimOutcome, String> {
    claim_task_response_delivery_with_recovery_key_and_started_at(
        pool,
        channel_id,
        provider,
        session_key,
        event_key,
        response_turn_key,
        recovery_turn_key,
        None,
        None,
        None,
        card_message_id,
        owner,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn claim_task_response_delivery_with_recovery_key_and_started_at(
    pool: Option<&PgPool>,
    channel_id: u64,
    provider: &str,
    session_key: &str,
    event_key: &str,
    response_turn_key: &str,
    recovery_turn_key: Option<&str>,
    turn_started_at: Option<&str>,
    turn_start_offset: Option<u64>,
    turn_end_offset: Option<u64>,
    card_message_id: u64,
    owner: ResponseDeliveryOwner,
) -> Result<ResponseDeliveryClaimOutcome, String> {
    let scope = TaskCardScope::new(channel_id, provider, session_key, event_key);
    store::claim_response_delivery(
        pool,
        &scope,
        response_turn_key,
        recovery_turn_key,
        turn_started_at,
        turn_start_offset,
        turn_end_offset,
        card_message_id,
        owner,
    )
    .await
}

pub(in crate::services::discord) async fn claim_existing_task_response_delivery(
    pool: Option<&PgPool>,
    channel_id: u64,
    provider: &str,
    session_key: &str,
    response_turn_key: &str,
    owner: ResponseDeliveryOwner,
) -> Result<Option<ExistingResponseDelivery>, String> {
    let lookup_scope = TaskCardScope::new(channel_id, provider, session_key, "lookup-only");
    store::claim_existing_response_delivery(pool, &lookup_scope, response_turn_key, owner).await
}

pub(in crate::services::discord) async fn mark_task_response_delivered(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
) -> Result<(), String> {
    store::mark_response_delivered(pool, claim).await
}

pub(in crate::services::discord) async fn mark_task_response_sent(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
) -> Result<(), String> {
    store::mark_response_sent(pool, claim).await
}

pub(in crate::services::discord) async fn rebind_task_response_card(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
    replacement_card_message_id: u64,
) -> Result<ResponseDeliveryClaim, String> {
    store::rebind_response_card(pool, claim, replacement_card_message_id).await
}

const RESPONSE_COMMIT_ATTEMPTS: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum TaskResponseCommitOutcome {
    Delivered,
    SentButUncommitted { error: String },
}

pub(in crate::services::discord) async fn record_task_response_sent_bounded(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
) -> Result<(), String> {
    let mut last_error = String::new();
    for attempt in 0..RESPONSE_COMMIT_ATTEMPTS {
        match mark_task_response_sent(pool, claim).await {
            Ok(()) => return Ok(()),
            Err(error) => last_error = error,
        }
        if attempt + 1 < RESPONSE_COMMIT_ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
    Err(format!(
        "task response POST succeeded but sent-state CAS failed after {RESPONSE_COMMIT_ATTEMPTS} attempts: {last_error}"
    ))
}

pub(in crate::services::discord) async fn commit_task_response_delivered_bounded(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
) -> TaskResponseCommitOutcome {
    let mut last_error = String::new();
    for attempt in 0..RESPONSE_COMMIT_ATTEMPTS {
        match mark_task_response_delivered(pool, claim).await {
            Ok(()) => return TaskResponseCommitOutcome::Delivered,
            Err(error) => last_error = error,
        }
        if attempt + 1 < RESPONSE_COMMIT_ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
    TaskResponseCommitOutcome::SentButUncommitted {
        error: format!(
            "final delivered-state CAS failed after {RESPONSE_COMMIT_ATTEMPTS} attempts: {last_error}"
        ),
    }
}

#[cfg(test)]
fn force_task_response_delivered_failures(claim: &ResponseDeliveryClaim, attempts: usize) {
    store::force_response_deliver_failures(claim, attempts);
}

pub(in crate::services::discord) async fn renew_task_response_delivery(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
) -> Result<(), String> {
    store::renew_response_delivery(pool, claim).await
}

pub(in crate::services::discord) struct TaskResponseDeliveryHeartbeat {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl TaskResponseDeliveryHeartbeat {
    pub(in crate::services::discord) fn stop(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for TaskResponseDeliveryHeartbeat {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub(in crate::services::discord) fn task_response_delivery_heartbeat(
    pool: Option<&PgPool>,
    claim: Option<&ResponseDeliveryClaim>,
) -> TaskResponseDeliveryHeartbeat {
    let task = pool.cloned().zip(claim.cloned()).map(|(pool, claim)| {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.tick().await;
            loop {
                interval.tick().await;
                if let Err(error) = store::renew_response_delivery(Some(&pool), &claim).await {
                    tracing::warn!(
                        error = %error,
                        "task response delivery heartbeat lost exact claim ownership"
                    );
                    break;
                }
            }
        })
    });
    TaskResponseDeliveryHeartbeat { task }
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
    #[error("task card delivery is ambiguous and fail-closed: {0}")]
    Ambiguous(String),
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

/// Replace a task card only after Discord authoritatively rejects it as a
/// required response reference. The old message id is an exact CAS input: a
/// concurrent worker that already installed a replacement returns that card
/// instead of issuing another POST.
pub(in crate::services::discord) async fn replace_confirmed_missing_card<T: TaskCardTransport>(
    pool: Option<&PgPool>,
    clients: &CardDeliveryClients,
    transport: &T,
    event: &TaskCardEvent,
    missing_message_id: u64,
) -> Result<CardEnsureOutcome, CardEnsureError> {
    for attempt in 0..20 {
        let claim = store::claim_missing_card_replacement(pool, &event.scope, missing_message_id)
            .await
            .map_err(CardEnsureError::Store)?;
        match claim {
            store::MissingCardReplacementClaim::Existing {
                message_id,
                bot_key,
            } => {
                return Ok(CardEnsureOutcome {
                    message_id,
                    bot_key,
                    disposition: CardDisposition::Replaced,
                });
            }
            store::MissingCardReplacementClaim::Busy { bot_key } if attempt < 19 => {
                let _ = bot_key;
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            store::MissingCardReplacementClaim::Busy { bot_key } => {
                return Err(CardEnsureError::Busy(format!(
                    "another worker owns the missing-card replacement lease (bot={bot_key})"
                )));
            }
            store::MissingCardReplacementClaim::Owned(claimed) => {
                let Some(bot) = clients.by_key(&claimed.bot_key) else {
                    let error = format!(
                        "the replacement card's pinned bot {} is unavailable",
                        claimed.bot_key
                    );
                    store::mark_post_failure(pool, &claimed, &error)
                        .await
                        .map_err(CardEnsureError::Store)?;
                    return Err(CardEnsureError::Permanent(error));
                };
                let content = claimed.rendered_content.clone();
                let hash = content_hash(&content);
                let message_id =
                    deliver_card_post_claim(pool, transport, bot, &claimed, &content, &hash)
                        .await?;
                return Ok(CardEnsureOutcome {
                    message_id,
                    bot_key: claimed.bot_key,
                    disposition: CardDisposition::Replaced,
                });
            }
        }
    }
    unreachable!("bounded missing-card replacement loop returns on its last attempt")
}

async fn deliver_claim<T: TaskCardTransport>(
    pool: Option<&PgPool>,
    clients: &CardDeliveryClients,
    transport: &T,
    event: &TaskCardEvent,
    _intent: EnsureIntent,
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
        return Err(CardEnsureError::Permanent(error));
    };
    let content = match &claimed.action {
        store::ClaimAction::Post if !claimed.rendered_content.is_empty() => {
            claimed.rendered_content.clone()
        }
        _ => event.payload.render(claimed.update_count),
    };
    let hash = content_hash(&content);

    match claimed.action {
        store::ClaimAction::Post => {
            let message_id =
                deliver_card_post_claim(pool, transport, bot, &claimed, &content, &hash).await?;
            Ok(CardEnsureOutcome {
                message_id,
                bot_key: claimed.bot_key,
                disposition: if claimed.revision > 1 && !claimed.new_terminal_completion {
                    CardDisposition::Replaced
                } else {
                    CardDisposition::Created
                },
            })
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
                    let replacement_id = deliver_card_post_claim(
                        pool,
                        transport,
                        bot,
                        &replacement,
                        &content,
                        &hash,
                    )
                    .await?;
                    Ok(CardEnsureOutcome {
                        message_id: replacement_id,
                        bot_key: replacement.bot_key,
                        disposition: CardDisposition::Replaced,
                    })
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
    payload_fingerprint: &str,
) -> String {
    if let Some(task_id) = task_id.filter(|value| !value.trim().is_empty()) {
        return format!("task:{}", fingerprint(&[task_id]));
    }
    if let Some(tool_use_id) = tool_use_id.filter(|value| !value.trim().is_empty()) {
        return format!("tool:{}", fingerprint(&[tool_use_id]));
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
