//! Discord snowflake ids for outbound values; serde bytes match the Serenity ids.

use std::fmt;
use std::num::NonZeroU64;

use serde::de::{Error, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

macro_rules! outbound_id {
    ($($name:ident),* $(,)?) => {$(
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub(crate) struct $name(NonZeroU64);

        impl $name {
            /// Panics on zero, as the Serenity id constructors do.
            #[track_caller]
            pub(crate) const fn new(id: u64) -> Self {
                match NonZeroU64::new(id) {
                    Some(inner) => Self(inner),
                    None => panic!(concat!(stringify!($name), "::new called with 0")),
                }
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserializer.deserialize_any(SnowflakeVisitor).map(Self)
            }
        }
    )*};
}

outbound_id!(OutboundChannelId, OutboundMessageId, OutboundUserId);

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
