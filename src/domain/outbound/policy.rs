//! Per-message outbound delivery policy: length handling, target fallback and dedup window.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How to handle content beyond the Discord per-message limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LengthStrategy {
    Split,
    Compact,
    FileAttachment,
    /// Reject without sending; stream/edit callers already chunk and must not alter content.
    RejectOverLimit,
}

/// What to do when the primary target is unreachable, such as an archived thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FallbackPolicy {
    None,
    /// Retry a failed thread target against its parent channel.
    ThreadOrChannel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DiscordOutboundPolicy {
    pub(crate) length_strategy: LengthStrategy,
    pub(crate) fallback: FallbackPolicy,
    /// How long dedup entries are kept; zero leaves dedup to the caller.
    #[serde(with = "duration_secs")]
    pub(crate) idempotency_window: Duration,
}

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

/// Encodes [`Duration`] as whole seconds on the wire.
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
