//! Bounded durable delivery for task-response chunks (#4446).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};
use sqlx::PgPool;

use super::store::{
    PreparedResponseChunk, ResponseChunkJournal, ResponseChunkPrepareError, confirm_response_chunk,
    mark_response_chunk_ambiguous, mark_response_chunk_posting, prepare_response_chunk,
};
use super::{ResponseDeliveryClaim, content_hash, response_chunk_nonce_for_generation};
use crate::services::discord::{SharedData, rate_limit_wait};

/// Discord documents nonce reconciliation as only a recent-message contract.
/// Stay strictly inside a conservative two-minute subset; the equality
/// boundary is already outside our retry authority.
const NONCE_RETRY_TTL_SECONDS: i64 = 120;
const NONCE_CLOCK_SKEW_SECONDS: i64 = 5;
const HISTORY_PAGE_SIZE: usize = 100;
const HISTORY_MAX_PAGES: usize = 10;
const MAX_CARD_REPAIRS_PER_SEND: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct ResponseChunkHistoryMessage {
    pub(in crate::services::discord) channel_id: u64,
    pub(in crate::services::discord) message_id: u64,
    pub(in crate::services::discord) author_id: u64,
    pub(in crate::services::discord) nonce: Option<String>,
    pub(in crate::services::discord) content_hash: String,
    pub(in crate::services::discord) referenced_message_id: Option<u64>,
    pub(in crate::services::discord) created_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub(in crate::services::discord) enum ResponseChunkPostError {
    #[error("required task-card reference is no longer valid: {0}")]
    UnknownReference(String),
    #[error("transient Discord response-chunk failure: {0}")]
    Transient(String),
    #[error("permanent Discord response-chunk rejection: {0}")]
    Permanent(String),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(in crate::services::discord) enum ResponseChunkHistoryError {
    #[error("transient Discord response-history failure: {0}")]
    Transient(String),
    #[error("permanent Discord response-history rejection: {0}")]
    Permanent(String),
}

#[derive(Debug, thiserror::Error)]
pub(in crate::services::discord) enum ResponseChunkDeliveryError {
    #[error("required task-card reference is no longer valid: {detail}")]
    UnknownReference { detail: String },
    #[error("transient task-response chunk delivery failure: {0}")]
    Transient(String),
    #[error("permanent task-response chunk delivery failure: {0}")]
    Permanent(String),
    #[error("ambiguous task-response chunk was fail-closed: {reason}")]
    Ambiguous { reason: String },
}

#[allow(async_fn_in_trait)]
pub(in crate::services::discord) trait ResponseChunkTransport:
    Send + Sync
{
    async fn bot_user_id(&self) -> Result<u64, String>;

    async fn post_chunk(
        &self,
        channel_id: u64,
        content: &str,
        reference_message_id: Option<u64>,
        nonce: &str,
    ) -> Result<u64, ResponseChunkPostError>;

    async fn history_page(
        &self,
        channel_id: u64,
        before_message_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<ResponseChunkHistoryMessage>, ResponseChunkHistoryError>;

    /// Production Discord history does not expose deletion audit evidence, so
    /// absence cannot disprove a POST-then-delete interleaving. Deterministic
    /// test transports may opt in only when they model deletion completeness.
    fn history_proves_deletions(&self) -> bool {
        false
    }
}

pub(in crate::services::discord) struct DiscordResponseChunkTransport<'a> {
    http: &'a serenity::Http,
    shared: &'a Arc<SharedData>,
}

impl<'a> DiscordResponseChunkTransport<'a> {
    pub(in crate::services::discord) fn new(
        http: &'a serenity::Http,
        shared: &'a Arc<SharedData>,
    ) -> Self {
        Self { http, shared }
    }
}

impl ResponseChunkTransport for DiscordResponseChunkTransport<'_> {
    async fn bot_user_id(&self) -> Result<u64, String> {
        self.http
            .get_current_user()
            .await
            .map(|user| user.id.get())
            .map_err(|error| format!("resolve response bot identity: {error}"))
    }

    async fn post_chunk(
        &self,
        channel_id: u64,
        content: &str,
        reference_message_id: Option<u64>,
        nonce: &str,
    ) -> Result<u64, ResponseChunkPostError> {
        let channel = ChannelId::new(channel_id);
        rate_limit_wait(self.shared, channel).await;
        match reference_message_id {
            Some(reference) => {
                crate::services::discord::http::send_channel_message_with_required_reference_and_nonce(
                    self.http,
                    channel,
                    content,
                    channel,
                    MessageId::new(reference),
                    nonce,
                )
                .await
                .map(|message| message.id.get())
                .map_err(|error| match error {
                    crate::services::discord::http::RequiredReferenceSendError::UnknownReference(error) => {
                        ResponseChunkPostError::UnknownReference(error.to_string())
                    }
                    crate::services::discord::http::RequiredReferenceSendError::Other(error) => {
                        classify_serenity_post_error(error)
                    }
                })
            }
            None => crate::services::discord::http::send_channel_message_with_nonce(
                self.http, channel, content, nonce,
            )
            .await
            .map(|message| message.id.get())
            .map_err(classify_serenity_post_error),
        }
    }

    async fn history_page(
        &self,
        channel_id: u64,
        before_message_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<ResponseChunkHistoryMessage>, ResponseChunkHistoryError> {
        let mut builder = serenity::GetMessages::new().limit(limit.min(100) as u8);
        if let Some(before) = before_message_id {
            builder = builder.before(MessageId::new(before));
        }
        ChannelId::new(channel_id)
            .messages(self.http, builder)
            .await
            .map_err(classify_serenity_history_error)?
            .into_iter()
            .map(|message| {
                let created_at =
                    DateTime::from_timestamp_millis(message.timestamp.timestamp_millis())
                        .ok_or_else(|| {
                            ResponseChunkHistoryError::Permanent(
                                "Discord history contained an invalid timestamp".to_string(),
                            )
                        })?;
                Ok(ResponseChunkHistoryMessage {
                    channel_id: message.channel_id.get(),
                    message_id: message.id.get(),
                    author_id: message.author.id.get(),
                    nonce: message.nonce.map(|nonce| match nonce {
                        serenity::model::channel::Nonce::String(value) => value,
                        serenity::model::channel::Nonce::Number(value) => value.to_string(),
                    }),
                    content_hash: content_hash(&message.content),
                    referenced_message_id: message
                        .message_reference
                        .and_then(|reference| reference.message_id)
                        .map(MessageId::get),
                    created_at,
                })
            })
            .collect()
    }
}

fn classify_serenity_post_error(error: serenity::Error) -> ResponseChunkPostError {
    let transient = match &error {
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) => {
            discord_status_is_transient(response.status_code.as_u16())
        }
        serenity::Error::Http(_) => true,
        _ => true,
    };
    if transient {
        ResponseChunkPostError::Transient(error.to_string())
    } else {
        ResponseChunkPostError::Permanent(error.to_string())
    }
}

fn classify_serenity_history_error(error: serenity::Error) -> ResponseChunkHistoryError {
    let transient = match &error {
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) => {
            discord_status_is_transient(response.status_code.as_u16())
        }
        serenity::Error::Http(_) => true,
        _ => true,
    };
    let detail = format!("read Discord response history: {error}");
    if transient {
        ResponseChunkHistoryError::Transient(detail)
    } else {
        ResponseChunkHistoryError::Permanent(detail)
    }
}

fn discord_status_is_transient(status: u16) -> bool {
    status >= 500 || status == 408 || status == 429
}

pub(in crate::services::discord) async fn send_task_response_chunks<T: ResponseChunkTransport>(
    pool: Option<&PgPool>,
    transport: &T,
    claim: &ResponseDeliveryClaim,
    text: &str,
) -> Result<Vec<MessageId>, ResponseChunkDeliveryError> {
    let bot_user_id = transport
        .bot_user_id()
        .await
        .map_err(ResponseChunkDeliveryError::Transient)?;
    let chunks = crate::services::discord::formatting::split_message(text);
    let chunk_count = chunks.len();
    let mut delivered = Vec::with_capacity(chunk_count);
    for (chunk_index, chunk) in chunks.iter().enumerate() {
        let reference = (chunk_index == 0).then_some(claim.card_message_id());
        let nonce = response_chunk_nonce_for_generation(
            claim.response_turn_key(),
            claim.response_generation(),
            chunk_index,
        );
        let hash = content_hash(chunk);
        let journal = prepare_response_chunk(
            pool,
            claim,
            chunk_index,
            chunk_count,
            &hash,
            &nonce,
            bot_user_id,
            reference,
        )
        .await
        .map_err(|error| match error {
            ResponseChunkPrepareError::Conflict(error) => {
                ResponseChunkDeliveryError::Permanent(error)
            }
            ResponseChunkPrepareError::Store(error) => ResponseChunkDeliveryError::Transient(error),
        })?;
        let posting = match journal {
            ResponseChunkJournal::Confirmed(confirmed) => {
                delivered.push(MessageId::new(confirmed.discord_message_id));
                continue;
            }
            ResponseChunkJournal::Prepared(prepared) => {
                // This durable intent has not crossed the network boundary, so
                // its age is irrelevant. Persist `posting` first; only that
                // phase ever needs bounded nonce/history reconciliation.
                mark_response_chunk_posting(pool, claim, &prepared)
                    .await
                    .map_err(ResponseChunkDeliveryError::Transient)?
            }
            ResponseChunkJournal::Posting(posting) => {
                let now = Utc::now();
                if posting
                    .next_reconcile_at
                    .is_some_and(|retry_at| retry_at > now)
                {
                    return Err(ResponseChunkDeliveryError::Ambiguous {
                        reason: format!(
                            "chunk {} remains quarantined until {}",
                            chunk_index,
                            posting.next_reconcile_at.expect("checked")
                        ),
                    });
                }
                let post_started_at = posting.post_started_at.ok_or_else(|| {
                    ResponseChunkDeliveryError::Permanent(format!(
                        "posting chunk {chunk_index} omitted its network-boundary timestamp"
                    ))
                })?;
                if !nonce_retry_allowed(post_started_at, now) {
                    match reconcile_prepared_chunk(transport, claim, &posting).await {
                        HistoryReconcile::Found(message_id) => {
                            confirm_response_chunk(pool, claim, &posting, message_id)
                                .await
                                .map_err(ResponseChunkDeliveryError::Transient)?;
                            delivered.push(MessageId::new(message_id));
                            continue;
                        }
                        HistoryReconcile::ProvenAbsent => {}
                        HistoryReconcile::Ambiguous(reason) => {
                            fail_closed(pool, claim, &posting, &reason).await?;
                            return Err(ResponseChunkDeliveryError::Ambiguous { reason });
                        }
                        HistoryReconcile::Transient(reason) => {
                            return Err(ResponseChunkDeliveryError::Transient(reason));
                        }
                        HistoryReconcile::Permanent(reason) => {
                            // A permanent history rejection still cannot prove
                            // whether the prior POST crossed Discord's network
                            // boundary. Quarantine and retain retry/operator
                            // authority instead of converting uncertainty into
                            // a terminal delivery loss.
                            fail_closed(pool, claim, &posting, &reason).await?;
                            return Err(ResponseChunkDeliveryError::Ambiguous { reason });
                        }
                    }
                }
                posting
            }
        };
        let message_id = transport
            .post_chunk(
                claim.scope.channel_id,
                chunk,
                posting.referenced_message_id,
                &posting.discord_nonce,
            )
            .await
            .map_err(|error| match error {
                ResponseChunkPostError::UnknownReference(detail) => {
                    ResponseChunkDeliveryError::UnknownReference { detail }
                }
                ResponseChunkPostError::Transient(error) => {
                    ResponseChunkDeliveryError::Transient(error)
                }
                ResponseChunkPostError::Permanent(error) => {
                    ResponseChunkDeliveryError::Permanent(error)
                }
            })?;
        confirm_response_chunk(pool, claim, &posting, message_id)
            .await
            .map_err(|error| {
                ResponseChunkDeliveryError::Transient(format!(
                    "Discord accepted chunk {chunk_index}, but its journal CAS failed: {error}"
                ))
            })?;
        delivered.push(MessageId::new(message_id));
    }
    Ok(delivered)
}

/// Repair a missing required-reference card a bounded number of times in one
/// delivery pass. Exhaustion remains transient: the durable response claim and
/// terminal frontier retain authority for a later pass instead of converting a
/// second deletion race into permanent response loss.
pub(in crate::services::discord) async fn send_task_response_chunks_with_card_repair<
    C: super::TaskCardTransport,
    R: ResponseChunkTransport,
>(
    pool: Option<&PgPool>,
    clients: &super::CardDeliveryClients,
    card_transport: &C,
    response_transport: &R,
    event: &super::TaskCardEvent,
    mut claim: ResponseDeliveryClaim,
    text: &str,
) -> Result<(Vec<MessageId>, ResponseDeliveryClaim), ResponseChunkDeliveryError> {
    let mut repairs = 0;
    loop {
        match send_task_response_chunks(pool, response_transport, &claim, text).await {
            Ok(messages) => return Ok((messages, claim)),
            Err(ResponseChunkDeliveryError::UnknownReference { detail })
                if repairs < MAX_CARD_REPAIRS_PER_SEND =>
            {
                let replacement = super::replace_confirmed_missing_card(
                    pool,
                    clients,
                    card_transport,
                    event,
                    claim.card_message_id(),
                )
                .await
                .map_err(|error| match error {
                    super::CardEnsureError::Permanent(error) => {
                        ResponseChunkDeliveryError::Permanent(error)
                    }
                    error => ResponseChunkDeliveryError::Transient(error.to_string()),
                })?;
                claim = super::rebind_task_response_card(pool, &claim, replacement.message_id)
                    .await
                    .map_err(|error| {
                        ResponseChunkDeliveryError::Transient(format!(
                            "rebind response to replacement task card: {error}"
                        ))
                    })?;
                repairs += 1;
                tracing::warn!(
                    response_turn_key = %claim.response_turn_key(),
                    response_generation = claim.response_generation(),
                    replacement_card_message_id = claim.card_message_id(),
                    repair_attempt = repairs,
                    error = %detail,
                    "required task-response card disappeared; rebound response generation"
                );
            }
            Err(ResponseChunkDeliveryError::UnknownReference { detail }) => {
                return Err(ResponseChunkDeliveryError::Transient(format!(
                    "required task-response card disappeared after {repairs} bounded repairs; preserving retry authority: {detail}"
                )));
            }
            Err(error) => return Err(error),
        }
    }
}

fn nonce_retry_allowed(attempt_started_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    let age = now.signed_duration_since(attempt_started_at);
    age >= chrono::Duration::seconds(-NONCE_CLOCK_SKEW_SECONDS)
        && age < chrono::Duration::seconds(NONCE_RETRY_TTL_SECONDS)
}

enum HistoryReconcile {
    Found(u64),
    ProvenAbsent,
    Ambiguous(String),
    Transient(String),
    Permanent(String),
}

async fn reconcile_prepared_chunk<T: ResponseChunkTransport>(
    transport: &T,
    claim: &ResponseDeliveryClaim,
    prepared: &PreparedResponseChunk,
) -> HistoryReconcile {
    let Some(post_started_at) = prepared.post_started_at else {
        return HistoryReconcile::Permanent(
            "history reconciliation requires a persisted network-boundary timestamp".into(),
        );
    };
    let current_bot = match transport.bot_user_id().await {
        Ok(bot) if bot == prepared.bot_user_id => bot,
        Ok(bot) => {
            return HistoryReconcile::Ambiguous(format!(
                "response bot identity changed from {} to {bot}",
                prepared.bot_user_id
            ));
        }
        Err(error) => return HistoryReconcile::Transient(error),
    };
    let mut before = None;
    let mut previous_oldest: Option<u64> = None;
    for page_index in 0..HISTORY_MAX_PAGES {
        let page = match transport
            .history_page(claim.scope.channel_id, before, HISTORY_PAGE_SIZE)
            .await
        {
            Ok(page) => page,
            Err(ResponseChunkHistoryError::Transient(error)) => {
                return HistoryReconcile::Transient(error);
            }
            Err(ResponseChunkHistoryError::Permanent(error)) => {
                return HistoryReconcile::Permanent(error);
            }
        };
        if page.is_empty() {
            return if transport.history_proves_deletions() {
                HistoryReconcile::ProvenAbsent
            } else {
                HistoryReconcile::Ambiguous(
                    "empty history cannot distinguish no POST from POST-then-delete".into(),
                )
            };
        }
        for pair in page.windows(2) {
            if pair[0].created_at < pair[1].created_at || pair[0].message_id <= pair[1].message_id {
                return HistoryReconcile::Ambiguous(
                    "Discord history page was not contiguous newest-first".into(),
                );
            }
        }
        if let Some(previous_id) = previous_oldest
            && page[0].message_id >= previous_id
        {
            return HistoryReconcile::Ambiguous(
                "Discord history pagination overlapped or went forward".into(),
            );
        }
        for message in &page {
            if message.channel_id != claim.scope.channel_id {
                return HistoryReconcile::Ambiguous(
                    "Discord history crossed the persisted channel".into(),
                );
            }
            if message.author_id != current_bot {
                continue;
            }
            let same_payload = message.content_hash == prepared.content_hash
                && message.referenced_message_id == prepared.referenced_message_id;
            if message.nonce.as_deref() == Some(prepared.discord_nonce.as_str()) {
                if same_payload {
                    return HistoryReconcile::Found(message.message_id);
                }
                return HistoryReconcile::Ambiguous(
                    "persisted nonce was observed with different content/reference".into(),
                );
            }
            if message.created_at >= post_started_at && same_payload && message.nonce.is_none() {
                return HistoryReconcile::Ambiguous(
                    "matching bot/content/reference history entry omitted nonce".into(),
                );
            }
        }
        let oldest = page.last().expect("nonempty page checked");
        if oldest.created_at <= post_started_at {
            return if transport.history_proves_deletions() {
                HistoryReconcile::ProvenAbsent
            } else {
                HistoryReconcile::Ambiguous(
                    "history reached attempt time but deletion ambiguity remains".into(),
                )
            };
        }
        previous_oldest = Some(oldest.message_id);
        before = Some(oldest.message_id);
        if page.len() < HISTORY_PAGE_SIZE {
            return if transport.history_proves_deletions() {
                HistoryReconcile::ProvenAbsent
            } else {
                HistoryReconcile::Ambiguous(
                    "bounded history ended without deletion evidence".into(),
                )
            };
        }
        if page_index + 1 == HISTORY_MAX_PAGES {
            return HistoryReconcile::Ambiguous(
                "Discord history reconciliation hit its page cap".into(),
            );
        }
    }
    HistoryReconcile::Ambiguous("Discord history reconciliation exhausted unexpectedly".into())
}

async fn fail_closed(
    pool: Option<&PgPool>,
    claim: &ResponseDeliveryClaim,
    prepared: &PreparedResponseChunk,
    reason: &str,
) -> Result<(), ResponseChunkDeliveryError> {
    mark_response_chunk_ambiguous(pool, claim, prepared, reason)
        .await
        .map_err(ResponseChunkDeliveryError::Transient)?;
    crate::services::observability::record_invariant_check(
        false,
        crate::services::observability::InvariantViolation {
            provider: Some(claim.scope.provider.as_str()),
            channel_id: Some(claim.scope.channel_id),
            dispatch_id: None,
            session_key: Some(claim.scope.session_key.as_str()),
            turn_id: Some(claim.response_turn_key()),
            invariant: "task_response_chunk_delivery_ambiguous",
            code_location: "src/services/discord/task_notification_delivery/response_chunks.rs:fail_closed",
            message: "task response crossed the network boundary but cannot be reconciled without duplicate risk",
            details: serde_json::json!({
                "response_generation": claim.response_generation(),
                "chunk_index": prepared.chunk_index,
                "reason": reason,
            }),
        },
    );
    tracing::error!(
        channel_id = claim.scope.channel_id,
        provider = %claim.scope.provider,
        session_key = %claim.scope.session_key,
        response_turn_key = %claim.response_turn_key,
        response_generation = claim.response_generation,
        chunk_index = prepared.chunk_index,
        error = %reason,
        "task response chunk is ambiguous outside the nonce window; quarantined without POST"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_retry_ttl_is_strict_at_boundary() {
        let start = DateTime::parse_from_rfc3339("2026-07-11T00:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        assert!(nonce_retry_allowed(
            start,
            start + chrono::Duration::seconds(NONCE_RETRY_TTL_SECONDS - 1)
        ));
        assert!(!nonce_retry_allowed(
            start,
            start + chrono::Duration::seconds(NONCE_RETRY_TTL_SECONDS)
        ));
        assert!(nonce_retry_allowed(
            start,
            start - chrono::Duration::seconds(1)
        ));
        assert!(!nonce_retry_allowed(
            start,
            start - chrono::Duration::seconds(NONCE_CLOCK_SKEW_SECONDS + 1)
        ));
    }

    #[test]
    fn discord_history_status_policy_keeps_auth_permanent_and_retryable_statuses_transient() {
        for status in [401, 403, 404] {
            assert!(!discord_status_is_transient(status), "status {status}");
        }
        for status in [408, 429, 500, 502, 503, 504] {
            assert!(discord_status_is_transient(status), "status {status}");
        }
    }
}
