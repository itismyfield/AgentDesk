//! Strict string deserialization shared by the fixed-width hex values.

use std::fmt;

use serde::Deserializer;
use serde::de::{self, Visitor};

/// Validates the borrowed string without creating an extra owned `String`; the deserializer
/// may still buffer escaped input first.
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
