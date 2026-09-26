//! Durable manual-steer operation: a queue work receipt, never a turn owner.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::action_handle::ActionHandle;

/// Untruncated 256-bit value: a full BLAKE3 digest or an episode nonce.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Bytes256([u8; 32]);

impl Bytes256 {
    pub(crate) fn digest_of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 64 || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        let mut bytes = [0; 32];
        hex::decode_to_slice(text, &mut bytes).ok()?;
        Some(Self(bytes))
    }
}

impl Serialize for Bytes256 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> Deserialize<'de> for Bytes256 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_fixed_str(
            deserializer,
            "a 256-bit lowercase hex value",
            Self::from_hex,
        )
    }
}

/// Hands the input string to `parse` without building an owned `String`, so an overlong value
/// is refused by its length check rather than copied first.
pub(super) fn deserialize_fixed_str<'de, D: Deserializer<'de>, T>(
    deserializer: D,
    expecting: &'static str,
    parse: fn(&str) -> Option<T>,
) -> Result<T, D::Error> {
    struct FixedStr<T> {
        expecting: &'static str,
        parse: fn(&str) -> Option<T>,
    }
    impl<T> Visitor<'_> for FixedStr<T> {
        type Value = T;
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str(self.expecting)
        }
        fn visit_str<E: de::Error>(self, text: &str) -> Result<T, E> {
            (self.parse)(text).ok_or_else(|| E::custom(format_args!("expected {}", self.expecting)))
        }
    }
    deserializer.deserialize_str(FixedStr { expecting, parse })
}

/// One Discord source message folded into the entry, at the generation it was queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceRef {
    pub(crate) message_id: u64,
    pub(crate) queued_generation: u64,
}

/// The queue card whose controls issued the click, at its binding epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CardBinding {
    pub(crate) card_message_id: u64,
    pub(crate) epoch: u64,
}

/// Everything a click must match at once before it may act; any mismatch is a stale click.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperationIdentity {
    pub(crate) channel_id: u64,
    pub(crate) entry_id: u64,
    pub(crate) entry_version: u64,
    pub(crate) payload_digest: Bytes256,
    pub(crate) sources: Vec<SourceRef>,
    /// Queue position the entry returns to after a verified non-delivery.
    pub(crate) ordinal: u64,
    /// Episode nonce of the live turn A the payload is injected into.
    pub(crate) a_episode: Bytes256,
    pub(crate) runtime_incarnation: u64,
    pub(crate) card: CardBinding,
    pub(crate) action: ActionHandle,
}

/// Provider acknowledgement of the single submit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubmitAck {
    AcceptedOrQueued,
    /// No proof either way; never retried and never read as not-delivered.
    Uncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnresolvedReason {
    DeliveryFailed,
    SubmitUnknown,
    ConsumptionUnknown,
    /// Explicit cancel, successful clear/purge, or runtime replacement withdrew the permit.
    Revoked,
}

/// Terminal result that releases the reservation. Only the first two are positive observations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Settlement {
    ConsumedInA,
    /// Observation proxy for reservation release, not proof of B's own delivery.
    SeparateDelivered,
    Unresolved(UnresolvedReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum OperationStage {
    Queued,
    Preparing,
    Reserved,
    /// Recorded before the first composer mutation; `enter_attempted` is set before Enter.
    MutationArmed {
        enter_attempted: bool,
    },
    Watching(SubmitAck),
    /// Enter never attempted and the composer provably untouched; the entry returns to `ordinal`.
    VerifiedNotDelivered,
    Retired(Settlement),
}

/// Settlement evidence kept independently until every condition of one outcome is seen.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceState {
    pub(crate) consumed_in_a: bool,
    pub(crate) a_episode_ended: bool,
    pub(crate) delivery_failed: bool,
    /// Wall-clock ms of the first no-evidence observation; cleared only by positive live evidence.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) no_evidence_since_ms: Option<i64>,
}

/// One observation fed to settlement. A single not-live or idle frame only starts the
/// no-evidence clock; it never settles on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObservationEvent {
    /// Busy together with progress inside the provider's progress window.
    PositiveLive,
    NoEvidenceStart {
        at_ms: i64,
    },
    /// Capture failure, unknown mtime or expired tracker: neither live nor idle.
    ProbeFailed,
    /// Positive idle held for the whole progress window with no transcript progress.
    StableIdle,
    ConsumedInA,
    AEpisodeEnded,
    FirstReacquireFinalized,
    DeliveryFailed,
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManualSteerOperation {
    pub(crate) identity: OperationIdentity,
    pub(crate) stage: OperationStage,
    pub(crate) evidence: EvidenceState,
    /// Expiry of the unused click permit, not a reservation deadline.
    pub(crate) permit_expires_at_ms: i64,
    /// Digest of the exact prompt written, relay prefix included; set once armed.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) wire_digest: Option<Bytes256>,
}
