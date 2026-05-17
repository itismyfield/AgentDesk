//! Unified length-safe idempotent Discord outbound delivery API (#1006).
//!
//! This module introduces a common outbound domain API that all Discord
//! message-sending code paths can migrate to. New production callsites should
//! use the v3 envelope modules directly; this module is the compatibility
//! facade for producers that have not migrated yet.
//!
//! Design:
//! - [`DiscordOutboundMessage`] carries content + channel/thread routing +
//!   correlation metadata.
//! - [`DiscordOutboundPolicy`] declares how to handle over-length content,
//!   thread fallback, file fallback, and deduplication.
//! - Length policy is enforced *inside* this module — callers do not
//!   pre-truncate. Over-2000-char content is either truncated or replaced with
//!   a minimal fallback variant.
//! - Idempotency is provided via `correlation_id` + `semantic_event_id` with
//!   an in-memory [`OutboundDeduper`]. Follow-up slices can swap this for a
//!   DB-backed store.
//! - [`DiscordOutboundClient`] is the transport trait that test doubles
//!   implement.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use poise::serenity_prelude::{ChannelId, MessageId};
use sha2::{Digest, Sha256};

use crate::server::routes::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};

use super::delivery::{DeliveryReferenceOverride, DeliveryTransportOverrides};
use super::message as v3_message;
use super::policy as v3_policy;
use super::result as v3_result;

/// Discord's hard per-message character limit.
pub(crate) const DISCORD_HARD_LIMIT_CHARS: usize = 2000;
/// Conservative soft limit — leaves headroom for the `[… truncated]` marker.
pub(crate) const DISCORD_SAFE_LIMIT_CHARS: usize = 1900;

/// Short stable fingerprint for outbound idempotency keys. Parts are separated
/// before hashing so concatenated inputs cannot collide by boundary changes.
pub(crate) fn outbound_fingerprint(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    format!("{digest:x}").chars().take(16).collect()
}

/// Strategy for handling content that exceeds the Discord per-message limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SplitStrategy {
    /// Hard-truncate at `max_len`, append `[… truncated]` marker.
    TruncateWithMarker,
    /// Truncate and, on length error from Discord, retry with the
    /// policy-provided minimal fallback content.
    TruncateWithMinimalFallback,
    /// Reject messages that exceed the limit before sending.
    RejectOverLimit,
}

/// Fallback behaviour when a thread send fails (e.g. archived/locked).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThreadFallback {
    /// Never fall back — report the error to the caller.
    None,
    /// On length error in a reused thread, do NOT create a new thread; return
    /// the error. This preserves the invariant in #750 tests.
    PreserveOnLengthError,
}

/// Fallback behaviour for file/attachment on oversized content. Reserved for
/// follow-up slices; the first slice only implements `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileFallback {
    None,
    AttachAsTextFile,
}

/// Policy controlling how [`DiscordOutboundMessage`] is delivered.
#[derive(Clone, Debug)]
pub(crate) struct DiscordOutboundPolicy {
    pub(crate) max_len: usize,
    pub(crate) split_strategy: SplitStrategy,
    pub(crate) thread_fallback: ThreadFallback,
    pub(crate) file_fallback: FileFallback,
    /// Optional short-form replacement used when [`SplitStrategy::TruncateWithMinimalFallback`]
    /// is selected. If empty, the truncated form is used.
    pub(crate) minimal_fallback: Option<String>,
}

impl Default for DiscordOutboundPolicy {
    fn default() -> Self {
        Self {
            max_len: DISCORD_SAFE_LIMIT_CHARS,
            split_strategy: SplitStrategy::TruncateWithMarker,
            thread_fallback: ThreadFallback::None,
            file_fallback: FileFallback::None,
            minimal_fallback: None,
        }
    }
}

impl DiscordOutboundPolicy {
    /// Preset for dispatch outbox: truncate with marker, minimal fallback on
    /// Discord length errors, never spawn a new thread after a reused-thread
    /// length failure.
    pub(crate) fn dispatch_outbox(minimal: String) -> Self {
        Self {
            max_len: DISCORD_SAFE_LIMIT_CHARS,
            split_strategy: SplitStrategy::TruncateWithMinimalFallback,
            thread_fallback: ThreadFallback::PreserveOnLengthError,
            file_fallback: FileFallback::None,
            minimal_fallback: Some(minimal),
        }
    }

    /// Preset for review notifications: truncate with marker, minimal fallback
    /// when the caller provides one.
    pub(crate) fn review_notification(minimal: Option<String>) -> Self {
        Self {
            max_len: DISCORD_SAFE_LIMIT_CHARS,
            split_strategy: if minimal.is_some() {
                SplitStrategy::TruncateWithMinimalFallback
            } else {
                SplitStrategy::TruncateWithMarker
            },
            thread_fallback: ThreadFallback::None,
            file_fallback: FileFallback::None,
            minimal_fallback: minimal,
        }
    }

    /// Preset for streaming gateway messages: caller-side planning already
    /// chunks content to Discord's hard per-message limit, so this policy must
    /// preserve the planned text verbatim or fail before sending. Silent
    /// truncation would corrupt the stream offset bookkeeping.
    pub(crate) fn preserve_inline_content() -> Self {
        Self {
            max_len: DISCORD_HARD_LIMIT_CHARS,
            split_strategy: SplitStrategy::RejectOverLimit,
            thread_fallback: ThreadFallback::None,
            file_fallback: FileFallback::None,
            minimal_fallback: None,
        }
    }
}

/// Semantic identifiers used to deduplicate outbound deliveries. The
/// [`correlation_id`] is the callsite-supplied grouping key; the
/// [`semantic_event_id`] is the specific event (e.g. `dispatch:<id>:sent`).
/// A deliver call is skipped when the same (correlation_id, semantic_event_id)
/// pair has already been marked delivered.
#[derive(Clone, Debug)]
pub(crate) struct DiscordOutboundMessage {
    pub(crate) content: String,
    pub(crate) channel_id: String,
    pub(crate) thread_id: Option<String>,
    pub(crate) edit_message_id: Option<String>,
    pub(crate) reference: Option<DiscordOutboundReference>,
    pub(crate) correlation_id: Option<String>,
    pub(crate) semantic_event_id: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct DiscordOutboundReference {
    pub(crate) channel_id: String,
    pub(crate) message_id: String,
}

impl DiscordOutboundMessage {
    pub(crate) fn new(channel_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            channel_id: channel_id.into(),
            thread_id: None,
            edit_message_id: None,
            reference: None,
            correlation_id: None,
            semantic_event_id: None,
        }
    }

    pub(crate) fn with_thread_id(mut self, thread_id: impl Into<String>) -> Self {
        self.thread_id = Some(thread_id.into());
        self
    }

    pub(crate) fn with_edit_message_id(mut self, message_id: impl Into<String>) -> Self {
        self.edit_message_id = Some(message_id.into());
        self
    }

    pub(crate) fn with_reference(
        mut self,
        channel_id: impl Into<String>,
        message_id: impl Into<String>,
    ) -> Self {
        self.reference = Some(DiscordOutboundReference {
            channel_id: channel_id.into(),
            message_id: message_id.into(),
        });
        self
    }

    pub(crate) fn with_correlation(
        mut self,
        correlation_id: impl Into<String>,
        semantic_event_id: impl Into<String>,
    ) -> Self {
        self.correlation_id = Some(correlation_id.into());
        self.semantic_event_id = Some(semantic_event_id.into());
        self
    }

    /// Channel or thread id that receives the POST.
    fn target_channel(&self) -> &str {
        self.thread_id
            .as_deref()
            .unwrap_or(self.channel_id.as_str())
    }
}

fn legacy_delivery_ids(message: &DiscordOutboundMessage) -> (String, String, bool) {
    match (&message.correlation_id, &message.semantic_event_id) {
        (Some(correlation_id), Some(semantic_event_id)) => {
            (correlation_id.clone(), semantic_event_id.clone(), true)
        }
        (Some(correlation_id), None) => (correlation_id.clone(), "_".to_string(), true),
        (None, Some(semantic_event_id)) => ("_".to_string(), semantic_event_id.clone(), true),
        (None, None) => (
            "legacy:no-dedup".to_string(),
            "legacy:no-dedup".to_string(),
            false,
        ),
    }
}

fn legacy_dedup_key(message: &DiscordOutboundMessage) -> Option<String> {
    let (correlation_id, semantic_event_id, dedup_enabled) = legacy_delivery_ids(message);
    dedup_enabled.then(|| format!("{correlation_id}::{semantic_event_id}"))
}

fn parse_channel_id_lossy(raw: &str) -> ChannelId {
    raw.parse::<u64>()
        .map(ChannelId::new)
        .unwrap_or_else(|_| ChannelId::new(1))
}

fn parse_message_id_lossy(raw: &str) -> MessageId {
    raw.parse::<u64>()
        .map(MessageId::new)
        .unwrap_or_else(|_| MessageId::new(1))
}

fn legacy_policy_to_v3(
    policy: &DiscordOutboundPolicy,
    dedup_enabled: bool,
) -> v3_policy::DiscordOutboundPolicy {
    let length_strategy = match (policy.split_strategy, policy.file_fallback) {
        (_, FileFallback::AttachAsTextFile) => v3_policy::LengthStrategy::FileAttachment,
        (SplitStrategy::RejectOverLimit, _) => v3_policy::LengthStrategy::RejectOverLimit,
        (SplitStrategy::TruncateWithMarker | SplitStrategy::TruncateWithMinimalFallback, _) => {
            v3_policy::LengthStrategy::Compact
        }
    };
    let mut v3 = v3_policy::DiscordOutboundPolicy {
        length_strategy,
        fallback: v3_policy::FallbackPolicy::None,
        idempotency_window: std::time::Duration::from_secs(24 * 60 * 60),
    };
    if !dedup_enabled {
        v3 = v3.without_idempotency();
    }
    v3
}

fn legacy_message_to_v3(
    message: &DiscordOutboundMessage,
    policy: &DiscordOutboundPolicy,
) -> v3_message::DiscordOutboundMessage {
    let (correlation_id, semantic_event_id, dedup_enabled) = legacy_delivery_ids(message);
    let target = match message.thread_id.as_deref() {
        Some(thread_id) => v3_message::OutboundTarget::Thread {
            parent: parse_channel_id_lossy(&message.channel_id),
            thread: parse_channel_id_lossy(thread_id),
        },
        None => v3_message::OutboundTarget::Channel(parse_channel_id_lossy(&message.channel_id)),
    };
    let operation = message
        .edit_message_id
        .as_deref()
        .map(|message_id| v3_message::OutboundOperation::Edit {
            message_id: parse_message_id_lossy(message_id),
        })
        .unwrap_or(v3_message::OutboundOperation::Send);
    let mut v3 = v3_message::DiscordOutboundMessage::new(
        correlation_id,
        semantic_event_id,
        message.content.clone(),
        target,
        legacy_policy_to_v3(policy, dedup_enabled),
    )
    .with_operation(operation);

    if let Some(reference) = message.reference.as_ref() {
        v3 = v3.with_reference(v3_message::OutboundReferenceContext::reply_to(
            parse_channel_id_lossy(&reference.channel_id),
            parse_message_id_lossy(&reference.message_id),
        ));
    }
    if let Some(minimal) = policy
        .minimal_fallback
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        v3 = v3.with_summary(minimal.to_string());
    }
    v3
}

fn legacy_delivery_overrides(
    message: &DiscordOutboundMessage,
    policy: &DiscordOutboundPolicy,
) -> DeliveryTransportOverrides {
    DeliveryTransportOverrides {
        target_channel: Some(message.target_channel().to_string()),
        edit_message_id: message.edit_message_id.clone(),
        reference: message
            .reference
            .as_ref()
            .map(|reference| DeliveryReferenceOverride {
                channel_id: reference.channel_id.clone(),
                message_id: reference.message_id.clone(),
            }),
        limits: Some(super::decision::OutboundPolicyLimits {
            inline_char_limit: policy.max_len.max(1),
            split_chunk_char_limit: policy.max_len.max(1),
            compact_char_limit: policy.max_len.max(1),
        }),
    }
}

/// Fallback kind reported when delivery succeeded via a degraded path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FallbackKind {
    /// Primary content was truncated to fit the Discord limit.
    Truncated,
    /// Primary send returned a length error; minimal fallback content was
    /// posted instead.
    MinimalFallback,
}

/// Outcome of a [`deliver_outbound`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeliveryResult {
    /// Successfully posted; `message_id` is the Discord message id.
    Success { message_id: String },
    /// Posted, but via a degraded path (truncated or minimal fallback).
    Fallback {
        message_id: String,
        kind: FallbackKind,
    },
    /// Skipped for a non-duplicate caller-side precondition.
    Skipped { reason: SkipReason },
    /// The (correlation_id, semantic_event_id) pair was already delivered.
    Duplicate { message_id: Option<String> },
    /// Permanent send failure — the caller must surface this to the retry
    /// bookkeeping layer.
    PermanentFailure { detail: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SkipReason {
    Duplicate,
}

/// Transport abstraction over the Discord HTTP API. Production code uses an
/// implementation that wraps `reqwest`; tests inject a deterministic mock.
#[allow(async_fn_in_trait)]
pub(crate) trait DiscordOutboundClient: Send + Sync {
    /// Post `content` to `target_channel` (a channel id or thread id).
    /// Returns the created message id on success.
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError>;

    /// Post `content` as a reply/reference to another message. Clients that
    /// cannot express references may fall back to a plain post.
    async fn post_message_with_reference(
        &self,
        target_channel: &str,
        content: &str,
        _reference_channel: &str,
        _reference_message: &str,
    ) -> Result<String, DispatchMessagePostError> {
        self.post_message(target_channel, content).await
    }

    /// Edit an existing message. Returns the edited message id on success.
    async fn edit_message(
        &self,
        target_channel: &str,
        message_id: &str,
        _content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        Err(DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            format!(
                "outbound client does not support edit for channel {target_channel} message {message_id}"
            ),
        ))
    }

    /// Resolve or create the DM channel for a Discord user id.
    async fn resolve_dm_channel(&self, user_id: &str) -> Result<String, DispatchMessagePostError> {
        Err(DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            format!("outbound client does not support DM channel resolution for user {user_id}"),
        ))
    }
}

/// In-memory dedup table keyed on `correlation_id::semantic_event_id`. The
/// store remembers successful deliveries so that a replayed outbox row with
/// the same semantic key is short-circuited with [`DeliveryResult::Duplicate`].
///
/// Follow-up slices can swap this for a Postgres table
/// (`discord_outbound_dedup`) without touching callers.
///
/// #2368 — also tracks an `in_flight` set. Two concurrent retries that
/// race lookup→send→record could both observe an empty lookup, both
/// publish, and both record. The atomic `reserve_in_flight` primitive
/// closes that window: the second caller observes the reservation and
/// can either short-circuit as a duplicate or wait. Callers that did
/// NOT win the reservation MUST NOT publish a second copy.
#[derive(Default)]
struct OutboundDeduperInner {
    delivered: HashMap<String, String>,
    in_flight: HashSet<String>,
}

#[derive(Clone, Default)]
pub(crate) struct OutboundDeduper {
    inner: Arc<Mutex<OutboundDeduperInner>>,
}

/// Outcome of a `reserve_in_flight` attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OutboundDeduperReservation {
    /// No prior delivery, no concurrent in-flight send. The caller now
    /// owns the in-flight slot for `key` and MUST eventually call either
    /// `record` (success) or `release_in_flight` (failure).
    Acquired,
    /// A prior successful delivery exists. The caller MUST treat its
    /// send as a duplicate and short-circuit to the stored message_id.
    AlreadyDelivered { message_id: String },
    /// Another caller is currently sending for the same key. The caller
    /// MUST treat its send as a duplicate and skip — concurrent retries
    /// of the same outbox row would otherwise double-deliver.
    AlreadyInFlight,
}

impl OutboundDeduper {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns the previously-delivered message id, if any.
    pub(crate) fn lookup(&self, key: &str) -> Option<String> {
        let guard = self.inner.lock().ok()?;
        guard.delivered.get(key).cloned()
    }

    pub(crate) fn record(&self, key: &str, message_id: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            guard
                .delivered
                .insert(key.to_string(), message_id.to_string());
            // A successful record clears any in-flight reservation the
            // caller held for the same key. Idempotent: if no
            // reservation exists, the remove is a no-op.
            guard.in_flight.remove(key);
        }
    }

    /// #2368 atomic primitive — lookup + in-flight reserve in a single
    /// critical section. Callers that observe `Acquired` MUST follow up
    /// with `record` on success or `release_in_flight` on failure;
    /// callers that observe `AlreadyDelivered` or `AlreadyInFlight`
    /// MUST NOT publish a second copy.
    pub(crate) fn reserve_in_flight(&self, key: &str) -> OutboundDeduperReservation {
        let Ok(mut guard) = self.inner.lock() else {
            // Poisoned lock — fail closed by reporting in-flight so
            // callers do not double-publish.
            return OutboundDeduperReservation::AlreadyInFlight;
        };
        if let Some(message_id) = guard.delivered.get(key) {
            return OutboundDeduperReservation::AlreadyDelivered {
                message_id: message_id.clone(),
            };
        }
        if !guard.in_flight.insert(key.to_string()) {
            return OutboundDeduperReservation::AlreadyInFlight;
        }
        OutboundDeduperReservation::Acquired
    }

    /// #2368 — drop an in-flight reservation when the send failed and no
    /// `record` will be called. Idempotent.
    pub(crate) fn release_in_flight(&self, key: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.in_flight.remove(key);
        }
    }
}

/// Truncate `content` to at most `max_chars` characters, appending a truncation
/// marker on a new paragraph when truncation occurred. Returns `(content, was_truncated)`.
fn truncate_with_marker(content: &str, max_chars: usize) -> (String, bool) {
    super::delivery::truncate_with_marker(content, max_chars)
}

/// Compatibility adapter for legacy callers. The v3 delivery module owns
/// policy planning, length safety, fallback retry, and dedup recording.
pub(crate) async fn deliver_outbound<C>(
    client: &C,
    dedup: &OutboundDeduper,
    message: DiscordOutboundMessage,
    policy: DiscordOutboundPolicy,
) -> DeliveryResult
where
    C: DiscordOutboundClient,
{
    let legacy_dedup_key = legacy_dedup_key(&message);
    if let Some(key) = legacy_dedup_key.as_deref() {
        if let Some(message_id) = dedup.lookup(key) {
            return DeliveryResult::Duplicate {
                message_id: Some(message_id),
            };
        }
    }

    let v3_message = legacy_message_to_v3(&message, &policy);
    let overrides = legacy_delivery_overrides(&message, &policy);
    match super::delivery::deliver_outbound_with_overrides(client, dedup, v3_message, overrides)
        .await
    {
        v3_result::DeliveryResult::Sent { messages, .. } => {
            let message_id = super::delivery::first_raw_message_id(&messages).unwrap_or_default();
            record_legacy_dedup_success(dedup, legacy_dedup_key.as_deref(), &message_id);
            DeliveryResult::Success { message_id }
        }
        v3_result::DeliveryResult::Fallback {
            messages,
            fallback_used,
            ..
        } => {
            let message_id = super::delivery::first_raw_message_id(&messages).unwrap_or_default();
            record_legacy_dedup_success(dedup, legacy_dedup_key.as_deref(), &message_id);
            DeliveryResult::Fallback {
                message_id,
                kind: match fallback_used {
                    v3_result::FallbackUsed::MinimalFallback => FallbackKind::MinimalFallback,
                    _ => FallbackKind::Truncated,
                },
            }
        }
        v3_result::DeliveryResult::Duplicate {
            existing_messages, ..
        } => {
            let message_id = super::delivery::first_raw_message_id(&existing_messages);
            if let Some(message_id) = message_id.as_deref() {
                record_legacy_dedup_success(dedup, legacy_dedup_key.as_deref(), message_id);
            }
            DeliveryResult::Duplicate { message_id }
        }
        v3_result::DeliveryResult::Skip { .. } => DeliveryResult::Skipped {
            reason: SkipReason::Duplicate,
        },
        v3_result::DeliveryResult::PermanentFailure { reason } => {
            DeliveryResult::PermanentFailure { detail: reason }
        }
    }
}

fn record_legacy_dedup_success(
    dedup: &OutboundDeduper,
    legacy_dedup_key: Option<&str>,
    message_id: &str,
) {
    if let Some(key) = legacy_dedup_key.filter(|_| !message_id.is_empty()) {
        dedup.record(key, message_id);
    }
}

/// Reqwest-backed [`DiscordOutboundClient`] used by production call sites.
#[derive(Clone)]
pub(crate) struct HttpOutboundClient {
    client: reqwest::Client,
    token: String,
    discord_api_base: String,
}

impl HttpOutboundClient {
    pub(crate) fn new(
        client: reqwest::Client,
        token: impl Into<String>,
        discord_api_base: impl Into<String>,
    ) -> Self {
        Self {
            client,
            token: token.into(),
            discord_api_base: discord_api_base.into(),
        }
    }
}

impl DiscordOutboundClient for HttpOutboundClient {
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        crate::server::routes::dispatches::discord_delivery::post_raw_message_once(
            &self.client,
            &self.token,
            &self.discord_api_base,
            target_channel,
            content,
        )
        .await
    }

    async fn edit_message(
        &self,
        target_channel: &str,
        message_id: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        crate::server::routes::dispatches::discord_delivery::edit_raw_message_once(
            &self.client,
            &self.token,
            &self.discord_api_base,
            target_channel,
            message_id,
            content,
        )
        .await
    }

    async fn resolve_dm_channel(&self, user_id: &str) -> Result<String, DispatchMessagePostError> {
        let url = crate::server::routes::dispatches::discord_delivery::discord_api_url(
            &self.discord_api_base,
            "/users/@me/channels",
        );
        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bot {}", self.token))
            .json(&serde_json::json!({ "recipient_id": user_id }))
            .send()
            .await
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("failed to resolve DM channel for user {user_id}: {error}"),
                )
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                format!("failed to resolve DM channel for user {user_id}: HTTP {status} {body}"),
            ));
        }

        let body = response
            .json::<serde_json::Value>()
            .await
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("failed to parse DM channel response for user {user_id}: {error}"),
                )
            })?;
        body.get("id")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("DM channel response for user {user_id} omitted channel id"),
                )
            })
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── Mock Discord client ─────────────────────────────────────
    #[derive(Clone, Default)]
    struct MockScript {
        /// Each entry: (min_len_to_fail, is_length_error). Consumed per call.
        failures: Arc<Mutex<Vec<(usize, bool)>>>,
        /// Absolute send failure — always returns an Other error.
        always_send_fail: Arc<Mutex<bool>>,
        posts: Arc<Mutex<Vec<(String, String)>>>,
        referenced_posts: Arc<Mutex<Vec<(String, String, String, String)>>>,
        edits: Arc<Mutex<Vec<(String, String, String)>>>,
        call_count: Arc<AtomicUsize>,
    }

    impl MockScript {
        fn new() -> Self {
            Self::default()
        }

        fn push_length_failure(&self, min_len: usize) {
            self.failures.lock().unwrap().push((min_len, true));
        }

        fn push_non_length_failure(&self) {
            self.failures.lock().unwrap().push((0, false));
        }

        fn set_always_send_fail(&self) {
            *self.always_send_fail.lock().unwrap() = true;
        }

        fn posts(&self) -> Vec<(String, String)> {
            self.posts.lock().unwrap().clone()
        }

        fn referenced_posts(&self) -> Vec<(String, String, String, String)> {
            self.referenced_posts.lock().unwrap().clone()
        }

        fn edits(&self) -> Vec<(String, String, String)> {
            self.edits.lock().unwrap().clone()
        }
    }

    impl DiscordOutboundClient for MockScript {
        async fn post_message(
            &self,
            target_channel: &str,
            content: &str,
        ) -> Result<String, DispatchMessagePostError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.posts
                .lock()
                .unwrap()
                .push((target_channel.to_string(), content.to_string()));
            if *self.always_send_fail.lock().unwrap() {
                return Err(DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    "mock send failure".into(),
                ));
            }
            let mut failures = self.failures.lock().unwrap();
            if let Some((min_len, is_length)) = failures.first().cloned() {
                if content.chars().count() >= min_len {
                    failures.remove(0);
                    let kind = if is_length {
                        DispatchMessagePostErrorKind::MessageTooLong
                    } else {
                        DispatchMessagePostErrorKind::Other
                    };
                    return Err(DispatchMessagePostError::new(
                        kind,
                        "mock forced failure".into(),
                    ));
                }
            }
            Ok(format!(
                "msg-{}-{}",
                target_channel,
                content.chars().count()
            ))
        }

        async fn post_message_with_reference(
            &self,
            target_channel: &str,
            content: &str,
            reference_channel: &str,
            reference_message: &str,
        ) -> Result<String, DispatchMessagePostError> {
            self.referenced_posts.lock().unwrap().push((
                target_channel.to_string(),
                content.to_string(),
                reference_channel.to_string(),
                reference_message.to_string(),
            ));
            self.post_message(target_channel, content).await
        }

        async fn edit_message(
            &self,
            target_channel: &str,
            message_id: &str,
            content: &str,
        ) -> Result<String, DispatchMessagePostError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.edits.lock().unwrap().push((
                target_channel.to_string(),
                message_id.to_string(),
                content.to_string(),
            ));
            Ok(message_id.to_string())
        }
    }

    // ── Scenario tests ──────────────────────────────────────────
    #[tokio::test]
    async fn over_2000_is_truncated_inside_api() {
        let client = MockScript::new();
        let dedup = OutboundDeduper::new();
        let long_content: String = "A".repeat(4000);
        let msg = DiscordOutboundMessage::new("chan-1", long_content.clone());
        let policy = DiscordOutboundPolicy::default();

        let result = deliver_outbound(&client, &dedup, msg, policy).await;

        match result {
            DeliveryResult::Fallback {
                kind: FallbackKind::Truncated,
                ..
            } => {}
            other => panic!("expected truncated fallback, got {other:?}"),
        }
        let posts = client.posts();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].1.contains("[… truncated]"));
        assert!(posts[0].1.chars().count() <= DISCORD_SAFE_LIMIT_CHARS + 20);
    }

    #[tokio::test]
    async fn success_under_limit_is_plain_success() {
        let client = MockScript::new();
        let dedup = OutboundDeduper::new();
        let msg = DiscordOutboundMessage::new("chan-1", "hello")
            .with_correlation("dispatch-1", "dispatch:1:sent");
        let policy = DiscordOutboundPolicy::default();

        let result = deliver_outbound(&client, &dedup, msg, policy).await;

        assert!(matches!(result, DeliveryResult::Success { .. }));
        assert_eq!(client.posts().len(), 1);
    }

    #[tokio::test]
    async fn send_failure_is_reported_as_permanent_failure() {
        let client = MockScript::new();
        client.set_always_send_fail();
        let dedup = OutboundDeduper::new();
        let msg = DiscordOutboundMessage::new("chan-1", "hello");
        let policy = DiscordOutboundPolicy::default();

        let result = deliver_outbound(&client, &dedup, msg, policy).await;

        assert!(matches!(result, DeliveryResult::PermanentFailure { .. }));
        assert_eq!(client.posts().len(), 1);
    }

    #[tokio::test]
    async fn duplicate_semantic_event_id_is_duplicate() {
        let client = MockScript::new();
        let dedup = OutboundDeduper::new();
        let make = || {
            DiscordOutboundMessage::new("chan-1", "hello")
                .with_correlation("dispatch-42", "dispatch:42:sent")
        };
        let policy = DiscordOutboundPolicy::default();

        let first = deliver_outbound(&client, &dedup, make(), policy.clone()).await;
        assert!(matches!(first, DeliveryResult::Success { .. }));

        let second = deliver_outbound(&client, &dedup, make(), policy).await;
        assert!(matches!(
            second,
            DeliveryResult::Duplicate {
                message_id: Some(_)
            }
        ));
        // Only one POST landed on the wire.
        assert_eq!(client.posts().len(), 1);
    }

    #[tokio::test]
    async fn compatibility_adapter_honors_existing_legacy_dedup_key() {
        let client = MockScript::new();
        let dedup = OutboundDeduper::new();
        dedup.record("dispatch-42::dispatch:42:sent", "msg-existing");
        let msg = DiscordOutboundMessage::new("chan-1", "hello")
            .with_correlation("dispatch-42", "dispatch:42:sent");

        let result = deliver_outbound(&client, &dedup, msg, DiscordOutboundPolicy::default()).await;

        assert_eq!(
            result,
            DeliveryResult::Duplicate {
                message_id: Some("msg-existing".to_string())
            }
        );
        assert!(client.posts().is_empty());
    }

    #[tokio::test]
    async fn compatibility_adapter_records_legacy_dedup_key_after_v3_success() {
        let client = MockScript::new();
        let dedup = OutboundDeduper::new();
        let msg = DiscordOutboundMessage::new("chan-1", "hello")
            .with_correlation("dispatch-42", "dispatch:42:sent");

        let result = deliver_outbound(&client, &dedup, msg, DiscordOutboundPolicy::default()).await;

        assert!(matches!(result, DeliveryResult::Success { .. }));
        assert_eq!(
            dedup.lookup("dispatch-42::dispatch:42:sent").as_deref(),
            Some("msg-chan-1-5")
        );
    }

    #[tokio::test]
    async fn referenced_send_is_routed_through_outbound_client() {
        let client = MockScript::new();
        let dedup = OutboundDeduper::new();
        let msg = DiscordOutboundMessage::new("chan-1", "...").with_reference("chan-1", "msg-user");

        let result = deliver_outbound(&client, &dedup, msg, DiscordOutboundPolicy::default()).await;

        assert!(matches!(result, DeliveryResult::Success { .. }));
        assert_eq!(
            client.referenced_posts(),
            vec![(
                "chan-1".to_string(),
                "...".to_string(),
                "chan-1".to_string(),
                "msg-user".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn edit_operation_uses_outbound_client_edit() {
        let client = MockScript::new();
        let dedup = OutboundDeduper::new();
        let msg = DiscordOutboundMessage::new("chan-1", "updated")
            .with_edit_message_id("msg-placeholder");

        let result = deliver_outbound(&client, &dedup, msg, DiscordOutboundPolicy::default()).await;

        assert_eq!(
            result,
            DeliveryResult::Success {
                message_id: "msg-placeholder".to_string()
            }
        );
        assert_eq!(
            client.edits(),
            vec![(
                "chan-1".to_string(),
                "msg-placeholder".to_string(),
                "updated".to_string()
            )]
        );
        assert!(client.posts().is_empty());
    }

    #[tokio::test]
    async fn preserve_inline_policy_keeps_1900_to_2000_char_edit_intact() {
        let client = MockScript::new();
        let dedup = OutboundDeduper::new();
        let content = "x".repeat(DISCORD_SAFE_LIMIT_CHARS + 75);
        assert!(content.chars().count() > DISCORD_SAFE_LIMIT_CHARS);
        assert!(content.chars().count() <= DISCORD_HARD_LIMIT_CHARS);
        let msg =
            DiscordOutboundMessage::new("chan-1", &content).with_edit_message_id("msg-placeholder");

        let result = deliver_outbound(
            &client,
            &dedup,
            msg,
            DiscordOutboundPolicy::preserve_inline_content(),
        )
        .await;

        assert_eq!(
            result,
            DeliveryResult::Success {
                message_id: "msg-placeholder".to_string()
            }
        );
        assert_eq!(client.edits()[0].2, content);
    }

    #[tokio::test]
    async fn preserve_inline_policy_rejects_above_hard_limit_without_editing() {
        let client = MockScript::new();
        let dedup = OutboundDeduper::new();
        let content = "x".repeat(DISCORD_HARD_LIMIT_CHARS + 1);
        let msg =
            DiscordOutboundMessage::new("chan-1", &content).with_edit_message_id("msg-placeholder");

        let result = deliver_outbound(
            &client,
            &dedup,
            msg,
            DiscordOutboundPolicy::preserve_inline_content(),
        )
        .await;

        assert!(matches!(result, DeliveryResult::PermanentFailure { .. }));
        assert!(client.edits().is_empty());
    }

    #[tokio::test]
    async fn thread_length_error_with_minimal_fallback_retries_in_same_thread() {
        let client = MockScript::new();
        // Primary send (post-truncate) still triggers a Discord-side length
        // error because the mock forces a length failure on content >= 50 chars
        // even within our safe limit.
        client.push_length_failure(50);
        let dedup = OutboundDeduper::new();
        let msg = DiscordOutboundMessage::new("chan-1", "A".repeat(200))
            .with_thread_id("thread-existing");
        let policy = DiscordOutboundPolicy::dispatch_outbox("short".into());

        let result = deliver_outbound(&client, &dedup, msg, policy).await;

        match result {
            DeliveryResult::Fallback {
                kind: FallbackKind::MinimalFallback,
                ..
            } => {}
            other => panic!("expected minimal fallback, got {other:?}"),
        }
        let posts = client.posts();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].0, "thread-existing");
        assert_eq!(posts[1].0, "thread-existing");
        assert_eq!(posts[1].1, "short");
    }

    #[tokio::test]
    async fn thread_length_error_with_failing_minimal_fallback_returns_permanent_failure() {
        let client = MockScript::new();
        // Primary fails with length, minimal fallback also fails with length.
        client.push_length_failure(50);
        client.push_length_failure(0);
        let dedup = OutboundDeduper::new();
        let msg = DiscordOutboundMessage::new("chan-1", "A".repeat(200))
            .with_thread_id("thread-existing")
            .with_correlation("dispatch-failfall", "dispatch:failfall:sent");
        let policy = DiscordOutboundPolicy::dispatch_outbox("short".into());

        let result = deliver_outbound(&client, &dedup, msg, policy).await;

        match result {
            DeliveryResult::PermanentFailure { .. } => {}
            other => panic!("expected permanent failure, got {other:?}"),
        }
        // dedup should NOT have recorded the failed delivery.
        assert!(
            dedup
                .lookup("dispatch-failfall::dispatch:failfall:sent")
                .is_none()
        );
    }

    #[tokio::test]
    async fn reject_over_limit_strategy_skips_send() {
        let client = MockScript::new();
        let dedup = OutboundDeduper::new();
        let msg = DiscordOutboundMessage::new("chan-1", "X".repeat(5000));
        let policy = DiscordOutboundPolicy {
            max_len: 100,
            split_strategy: SplitStrategy::RejectOverLimit,
            thread_fallback: ThreadFallback::None,
            file_fallback: FileFallback::None,
            minimal_fallback: None,
        };

        let result = deliver_outbound(&client, &dedup, msg, policy).await;

        assert!(matches!(result, DeliveryResult::PermanentFailure { .. }));
        assert!(client.posts().is_empty());
    }

    #[test]
    fn truncate_with_marker_respects_unicode_boundaries() {
        let content = "가나다라마바사아자차카타파하".repeat(300);
        let (out, truncated) = truncate_with_marker(&content, 100);
        assert!(truncated);
        assert!(out.ends_with("[… truncated]"));
        // Ensure no char was split (round-trip to String succeeds).
        assert!(out.chars().count() <= 110 + 20);
    }
}

// #2368 — OutboundDeduper atomic reserve_in_flight primitive tests.
// These are NOT gated on the legacy-sqlite-tests feature so they always
// run in the standard `cargo test` invocation.
#[cfg(test)]
mod deduper_concurrency_tests {
    use super::{OutboundDeduper, OutboundDeduperReservation};

    #[test]
    fn reserve_in_flight_acquired_on_empty_state() {
        let d = OutboundDeduper::new();
        match d.reserve_in_flight("key-a") {
            OutboundDeduperReservation::Acquired => {}
            other => panic!("expected Acquired, got {other:?}"),
        }
    }

    #[test]
    fn reserve_in_flight_returns_already_in_flight_for_second_caller() {
        let d = OutboundDeduper::new();
        let _first = d.reserve_in_flight("key-a");
        match d.reserve_in_flight("key-a") {
            OutboundDeduperReservation::AlreadyInFlight => {}
            other => panic!("expected AlreadyInFlight, got {other:?}"),
        }
    }

    #[test]
    fn reserve_in_flight_returns_already_delivered_when_record_present() {
        let d = OutboundDeduper::new();
        d.record("key-a", "discord-message-id-1");
        match d.reserve_in_flight("key-a") {
            OutboundDeduperReservation::AlreadyDelivered { message_id } => {
                assert_eq!(message_id, "discord-message-id-1");
            }
            other => panic!("expected AlreadyDelivered, got {other:?}"),
        }
    }

    #[test]
    fn release_in_flight_lets_a_subsequent_caller_acquire() {
        let d = OutboundDeduper::new();
        let _first = d.reserve_in_flight("key-a");
        d.release_in_flight("key-a");
        match d.reserve_in_flight("key-a") {
            OutboundDeduperReservation::Acquired => {}
            other => panic!("expected Acquired after release, got {other:?}"),
        }
    }

    #[test]
    fn record_clears_in_flight_slot() {
        let d = OutboundDeduper::new();
        let _first = d.reserve_in_flight("key-a");
        d.record("key-a", "msg-1");
        // A subsequent reserve now sees the recorded delivery, not the
        // stale in-flight reservation the first caller forgot to clear.
        match d.reserve_in_flight("key-a") {
            OutboundDeduperReservation::AlreadyDelivered { .. } => {}
            other => panic!("expected AlreadyDelivered, got {other:?}"),
        }
    }
}
