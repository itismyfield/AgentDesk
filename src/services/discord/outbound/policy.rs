//! New outbound delivery policy (#1006 v3, slice 1.0 — types only).
//!
//! Replaces the legacy [`super::legacy::DiscordOutboundPolicy`] (which mixed
//! `max_len`, three independent fallback enums, and a `minimal_fallback`
//! string) with a tighter three-field shape:
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
//! The new types are not yet consumed by any callsite; the deliver
//! implementation that interprets them lands in slice 1.1.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_has_split_and_none_with_24h_window() {
        let policy = DiscordOutboundPolicy::default();
        assert_eq!(policy.length_strategy, LengthStrategy::Split);
        assert_eq!(policy.fallback, FallbackPolicy::None);
        assert_eq!(policy.idempotency_window, Duration::from_secs(86_400));
    }

    #[test]
    fn policy_equality_is_structural() {
        let a = DiscordOutboundPolicy {
            length_strategy: LengthStrategy::Compact,
            fallback: FallbackPolicy::ThreadOrChannel,
            idempotency_window: Duration::from_secs(120),
        };
        let b = DiscordOutboundPolicy {
            length_strategy: LengthStrategy::Compact,
            fallback: FallbackPolicy::ThreadOrChannel,
            idempotency_window: Duration::from_secs(120),
        };
        assert_eq!(a, b);
    }

    #[test]
    fn length_strategy_serde_roundtrip() {
        for variant in [
            LengthStrategy::Split,
            LengthStrategy::Compact,
            LengthStrategy::FileAttachment,
        ] {
            let json = serde_json::to_string(&variant).expect("serialize");
            let back: LengthStrategy = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(variant, back);
        }
    }

    #[test]
    fn fallback_policy_serde_roundtrip() {
        for variant in [FallbackPolicy::None, FallbackPolicy::ThreadOrChannel] {
            let json = serde_json::to_string(&variant).expect("serialize");
            let back: FallbackPolicy = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(variant, back);
        }
    }

    #[test]
    fn policy_serde_roundtrip_encodes_window_as_seconds() {
        let policy = DiscordOutboundPolicy {
            length_strategy: LengthStrategy::FileAttachment,
            fallback: FallbackPolicy::ThreadOrChannel,
            idempotency_window: Duration::from_secs(300),
        };
        let json = serde_json::to_string(&policy).expect("serialize");
        assert!(json.contains("\"idempotency_window\":300"), "got: {json}");
        let back: DiscordOutboundPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(policy, back);
    }
}
