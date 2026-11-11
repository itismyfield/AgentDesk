//! Discord snowflake ids for outbound values; serde bytes match the Serenity ids.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct OutboundChannelId(#[serde(with = "snowflake")] NonZeroU64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct OutboundMessageId(#[serde(with = "snowflake")] NonZeroU64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct OutboundUserId(#[serde(with = "snowflake")] NonZeroU64);

impl OutboundChannelId {
    /// Panics on zero, as the Serenity id constructors do.
    #[track_caller]
    pub(crate) fn new(id: u64) -> Self {
        Self(NonZeroU64::new(id).expect("channel id must be non-zero"))
    }
}

impl OutboundMessageId {
    #[track_caller]
    pub(crate) fn new(id: u64) -> Self {
        Self(NonZeroU64::new(id).expect("message id must be non-zero"))
    }
}

impl OutboundUserId {
    #[track_caller]
    pub(crate) fn new(id: u64) -> Self {
        Self(NonZeroU64::new(id).expect("user id must be non-zero"))
    }
}

/// Writes the id as a decimal string and reads either a string or an integer.
mod snowflake {
    use std::fmt;
    use std::num::NonZeroU64;

    use serde::de::{Error, Visitor};
    use serde::{Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(id: &NonZeroU64, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_str(id)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<NonZeroU64, D::Error> {
        de.deserialize_any(SnowflakeVisitor)
    }

    struct SnowflakeVisitor;

    impl Visitor<'_> for SnowflakeVisitor {
        type Value = NonZeroU64;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a non-zero string or integer snowflake")
        }

        fn visit_i64<E: Error>(self, value: i64) -> Result<Self::Value, E> {
            self.visit_u64(u64::try_from(value).map_err(E::custom)?)
        }

        fn visit_u64<E: Error>(self, value: u64) -> Result<Self::Value, E> {
            NonZeroU64::new(value).ok_or_else(|| E::custom("invalid value, expected non-zero"))
        }

        fn visit_str<E: Error>(self, value: &str) -> Result<Self::Value, E> {
            value.parse().map_err(E::custom)
        }
    }
}
