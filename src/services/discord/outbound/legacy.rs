//! Unified length-safe idempotent Discord outbound delivery API (#1006).
//!
//! This module introduces a common outbound domain API that all Discord
//! message-sending code paths can migrate to. The first slice wires the
//! dispatch outbox path through it; subsequent slices will migrate review
//! notifications, DMs, and intake placeholder sends.
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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::server::routes::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};

/// Discord's hard per-message character limit.
pub(crate) const DISCORD_HARD_LIMIT_CHARS: usize = 2000;
/// Conservative soft limit — leaves headroom for the `[… truncated]` marker.
pub(crate) const DISCORD_SAFE_LIMIT_CHARS: usize = 1900;

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
    pub(crate) correlation_id: Option<String>,
    pub(crate) semantic_event_id: Option<String>,
}

impl DiscordOutboundMessage {
    pub(crate) fn new(channel_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            channel_id: channel_id.into(),
            thread_id: None,
            correlation_id: None,
            semantic_event_id: None,
        }
    }

    pub(crate) fn with_thread_id(mut self, thread_id: impl Into<String>) -> Self {
        self.thread_id = Some(thread_id.into());
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

    /// Dedup key derived from correlation/semantic ids.
    fn dedup_key(&self) -> Option<String> {
        match (&self.correlation_id, &self.semantic_event_id) {
            (Some(c), Some(s)) => Some(format!("{c}::{s}")),
            (Some(c), None) => Some(format!("{c}::_")),
            (None, Some(s)) => Some(format!("_::{s}")),
            (None, None) => None,
        }
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
    /// Skipped because the (correlation_id, semantic_event_id) pair was
    /// already delivered.
    Skipped { reason: SkipReason },
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
}

/// In-memory dedup table keyed on `correlation_id::semantic_event_id`. The
/// store remembers successful deliveries so that a replayed outbox row with
/// the same semantic key is short-circuited with [`DeliveryResult::Skipped`].
///
/// Follow-up slices can swap this for a Postgres table
/// (`discord_outbound_dedup`) without touching callers.
#[derive(Clone, Default)]
pub(crate) struct OutboundDeduper {
    inner: Arc<Mutex<HashMap<String, String>>>,
}

impl OutboundDeduper {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns the previously-delivered message id, if any.
    pub(crate) fn lookup(&self, key: &str) -> Option<String> {
        let guard = self.inner.lock().ok()?;
        guard.get(key).cloned()
    }

    pub(crate) fn record(&self, key: &str, message_id: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(key.to_string(), message_id.to_string());
        }
    }
}

/// Truncate `content` to at most `max_chars` characters, appending a truncation
/// marker on a new paragraph when truncation occurred. Returns `(content, was_truncated)`.
fn truncate_with_marker(content: &str, max_chars: usize) -> (String, bool) {
    if content.chars().count() <= max_chars {
        return (content.to_string(), false);
    }
    let boundary: usize = content
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(content.len());
    let cut = content[..boundary].rfind('\n').unwrap_or(boundary);
    (format!("{}\n\n[… truncated]", &content[..cut]), true)
}

/// Core delivery function. Applies the length policy, performs dedup lookup,
/// sends via `client`, and records the dedup key on success.
pub(crate) async fn deliver_outbound<C>(
    client: &C,
    dedup: &OutboundDeduper,
    message: DiscordOutboundMessage,
    policy: DiscordOutboundPolicy,
) -> DeliveryResult
where
    C: DiscordOutboundClient,
{
    // 1. Idempotency check.
    let dedup_key = message.dedup_key();
    if let Some(key) = dedup_key.as_deref() {
        if dedup.lookup(key).is_some() {
            return DeliveryResult::Skipped {
                reason: SkipReason::Duplicate,
            };
        }
    }

    let target = message.target_channel().to_string();

    // 2. Apply length policy.
    let (primary, truncated) = match policy.split_strategy {
        SplitStrategy::RejectOverLimit => {
            if message.content.chars().count() > policy.max_len {
                return DeliveryResult::PermanentFailure {
                    detail: format!(
                        "content length {} exceeds max_len {} (RejectOverLimit)",
                        message.content.chars().count(),
                        policy.max_len
                    ),
                };
            }
            (message.content.clone(), false)
        }
        SplitStrategy::TruncateWithMarker | SplitStrategy::TruncateWithMinimalFallback => {
            truncate_with_marker(&message.content, policy.max_len)
        }
    };

    // 3. Primary send attempt.
    let primary_result = client.post_message(&target, &primary).await;

    match primary_result {
        Ok(message_id) => {
            if let Some(key) = dedup_key.as_deref() {
                dedup.record(key, &message_id);
            }
            if truncated {
                DeliveryResult::Fallback {
                    message_id,
                    kind: FallbackKind::Truncated,
                }
            } else {
                DeliveryResult::Success { message_id }
            }
        }
        Err(error) => {
            let is_length = error.kind() == DispatchMessagePostErrorKind::MessageTooLong;

            // 4. Thread fallback gate — for reused threads, we preserve the
            //    error (matches #750 invariant: reused-thread length errors
            //    do not spawn a new thread).
            if is_length
                && policy.thread_fallback == ThreadFallback::PreserveOnLengthError
                && message.thread_id.is_some()
                && matches!(
                    policy.split_strategy,
                    SplitStrategy::TruncateWithMinimalFallback
                )
                && policy
                    .minimal_fallback
                    .as_deref()
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false)
            {
                // Even with thread preservation, we DO retry inside the same
                // thread with the minimal content — matches existing
                // `post_dispatch_message_to_channel` behaviour.
                let minimal = policy.minimal_fallback.clone().unwrap();
                if minimal == primary {
                    return DeliveryResult::PermanentFailure {
                        detail: format!(
                            "length error and minimal fallback matches primary: {error}"
                        ),
                    };
                }
                match client.post_message(&target, &minimal).await {
                    Ok(message_id) => {
                        if let Some(key) = dedup_key.as_deref() {
                            dedup.record(key, &message_id);
                        }
                        return DeliveryResult::Fallback {
                            message_id,
                            kind: FallbackKind::MinimalFallback,
                        };
                    }
                    Err(err) => {
                        return DeliveryResult::PermanentFailure {
                            detail: err.to_string(),
                        };
                    }
                }
            }

            // 5. Minimal fallback for non-thread or plain minimal policy.
            if is_length
                && matches!(
                    policy.split_strategy,
                    SplitStrategy::TruncateWithMinimalFallback
                )
            {
                if let Some(minimal) = policy
                    .minimal_fallback
                    .as_deref()
                    .filter(|v| !v.trim().is_empty() && *v != primary.as_str())
                {
                    match client.post_message(&target, minimal).await {
                        Ok(message_id) => {
                            if let Some(key) = dedup_key.as_deref() {
                                dedup.record(key, &message_id);
                            }
                            return DeliveryResult::Fallback {
                                message_id,
                                kind: FallbackKind::MinimalFallback,
                            };
                        }
                        Err(err) => {
                            return DeliveryResult::PermanentFailure {
                                detail: err.to_string(),
                            };
                        }
                    }
                }
            }

            DeliveryResult::PermanentFailure {
                detail: error.to_string(),
            }
        }
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
}

#[cfg(test)]
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
    async fn duplicate_semantic_event_id_is_skipped() {
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
            DeliveryResult::Skipped {
                reason: SkipReason::Duplicate
            }
        ));
        // Only one POST landed on the wire.
        assert_eq!(client.posts().len(), 1);
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
