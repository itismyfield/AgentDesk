//! New outbound delivery policy (#1006 v3).
//!
//! Delivery policy for v3 outbound envelopes:
//!
//! - [`LengthStrategy`] — how to handle content beyond the Discord
//!   per-message limit (split into multiple posts, compact via summary,
//!   or escalate to a file attachment).
//! - [`FallbackPolicy`] — what to do when the primary delivery target
//!   is unavailable (e.g. archived thread). Either fail or transparently
//!   re-route to the parent channel.
//! - `idempotency_window` — how long the dedup store should remember the
//!   `(correlation_id, semantic_event_id)` pair. Stored as
//!   [`std::time::Duration`] so wire formats can serialise it as seconds.
//!
//! These policies are consumed by [`super::delivery`].

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Strategy for handling outbound content that exceeds the Discord
/// per-message character limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LengthStrategy {
    /// Split the payload across multiple sequential messages.
    Split,
    /// Compact the payload (e.g. via a summarising prefix + truncation
    /// marker) so it fits in a single message.
    Compact,
    /// Escalate the payload to an attached text file when it exceeds the
    /// inline limit.
    FileAttachment,
    /// Reject payloads beyond the inline limit without sending. Used by
    /// stream/edit callsites that already manage chunking and must not
    /// silently alter content.
    RejectOverLimit,
}

/// Behaviour when the primary [`super::message::OutboundTarget`] is
/// unreachable (e.g. archived/locked thread, missing channel permission).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FallbackPolicy {
    /// No fallback — surface the error to the caller.
    None,
    /// If the target is a thread that fails, retry against the parent
    /// channel; if the target is already a channel, behave as [`Self::None`].
    ThreadOrChannel,
}

/// New per-message delivery policy (#1006 v3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DiscordOutboundPolicy {
    pub(crate) length_strategy: LengthStrategy,
    pub(crate) fallback: FallbackPolicy,
    /// Duration the dedup store should retain
    /// `(correlation_id, semantic_event_id)` entries. A zero duration
    /// disables retention (caller-side dedup only).
    #[serde(with = "duration_secs")]
    pub(crate) idempotency_window: Duration,
}

/// Default policy — `Split` length strategy, no fallback re-route, and a
/// 24-hour idempotency window. Chosen to mirror the conservative behaviour
/// of the legacy outbox path; the deliver impl in slice 1.1 will document
/// any per-callsite overrides.
impl Default for DiscordOutboundPolicy {
    fn default() -> Self {
        Self {
            length_strategy: LengthStrategy::Split,
            fallback: FallbackPolicy::None,
            idempotency_window: Duration::from_secs(24 * 60 * 60),
        }
    }
}

impl DiscordOutboundPolicy {
    pub(crate) fn dispatch_outbox() -> Self {
        Self {
            length_strategy: LengthStrategy::Compact,
            fallback: FallbackPolicy::None,
            idempotency_window: Duration::from_secs(24 * 60 * 60),
        }
    }

    pub(crate) fn review_notification() -> Self {
        Self {
            length_strategy: LengthStrategy::Compact,
            fallback: FallbackPolicy::None,
            idempotency_window: Duration::from_secs(24 * 60 * 60),
        }
    }

    pub(crate) fn preserve_inline_content() -> Self {
        Self {
            length_strategy: LengthStrategy::RejectOverLimit,
            fallback: FallbackPolicy::None,
            idempotency_window: Duration::from_secs(24 * 60 * 60),
        }
    }

    pub(crate) fn without_idempotency(mut self) -> Self {
        self.idempotency_window = Duration::ZERO;
        self
    }
}

/// Serde adapter that encodes [`Duration`] as a non-negative integer number
/// of seconds. Keeps the wire format human-readable in JSON dumps.
mod duration_secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(value: &Duration, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_u64(value.as_secs())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(de)?;
        Ok(Duration::from_secs(secs))
    }
}
