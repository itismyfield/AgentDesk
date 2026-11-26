//! Discord snowflake ids for outbound values; serde bytes match the Serenity ids.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OutboundChannelId(#[serde(with = "snowflake")] NonZeroU64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OutboundMessageId(#[serde(with = "snowflake")] NonZeroU64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OutboundUserId(#[serde(with = "snowflake")] NonZeroU64);

impl OutboundChannelId {
    /// Panics on zero, as the Serenity id constructors do.
    #[track_caller]
    pub fn new(id: u64) -> Self {
        Self(NonZeroU64::new(id).expect("channel id must be non-zero"))
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }
}

impl OutboundMessageId {
    #[track_caller]
    pub fn new(id: u64) -> Self {
        Self(NonZeroU64::new(id).expect("message id must be non-zero"))
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }
}

impl OutboundUserId {
    #[track_caller]
    pub fn new(id: u64) -> Self {
        Self(NonZeroU64::new(id).expect("user id must be non-zero"))
    }

    pub fn get(self) -> u64 {
        self.0.get()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflake_string_and_integer_fixtures_keep_legacy_wire() {
        for raw in [1, 42, u64::MAX] {
            for wire in [raw.to_string(), format!("\"{raw}\"")] {
                let channel: OutboundChannelId = serde_json::from_str(&wire).unwrap();
                let message: OutboundMessageId = serde_json::from_str(&wire).unwrap();
                let user: OutboundUserId = serde_json::from_str(&wire).unwrap();
                assert_eq!((channel.get(), message.get(), user.get()), (raw, raw, raw));
                for encoded in [
                    serde_json::to_string(&channel).unwrap(),
                    serde_json::to_string(&message).unwrap(),
                    serde_json::to_string(&user).unwrap(),
                ] {
                    assert_eq!(encoded, format!("\"{raw}\""));
                }
            }
        }
        for wire in ["0", "\"0\"", "-1", "\"opaque\""] {
            assert!(serde_json::from_str::<OutboundChannelId>(wire).is_err());
            assert!(serde_json::from_str::<OutboundMessageId>(wire).is_err());
            assert!(serde_json::from_str::<OutboundUserId>(wire).is_err());
        }
    }
}
