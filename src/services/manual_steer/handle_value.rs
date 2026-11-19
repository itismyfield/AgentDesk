//! The manual-steer action handle as a value: its bytes, canonical hex form and wire codec.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::fixed_str::deserialize_fixed_str;

pub(super) const HANDLE_BYTES: usize = 16;

/// One-time 128-bit action nonce. A custom-id carries only this opaque handle, and every
/// new click permit gets a new one so a stale card never matches a re-queued entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ActionHandle([u8; HANDLE_BYTES]);

impl ActionHandle {
    /// The all-zero value is reserved as invalid so an unset buffer never names a permit.
    pub(crate) fn from_bytes(bytes: [u8; HANDLE_BYTES]) -> Option<Self> {
        (bytes != [0; HANDLE_BYTES]).then_some(Self(bytes))
    }

    pub(super) fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    /// Accepts only the canonical lowercase form so one handle has exactly one spelling.
    pub(super) fn from_hex(text: &str) -> Option<Self> {
        if text.len() != HANDLE_BYTES * 2
            || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return None;
        }
        let mut bytes = [0; HANDLE_BYTES];
        hex::decode_to_slice(text, &mut bytes).ok()?;
        Self::from_bytes(bytes)
    }
}

impl Serialize for ActionHandle {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ActionHandle {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_fixed_str(
            deserializer,
            "a nonzero 128-bit lowercase hex handle",
            Self::from_hex,
        )
    }
}
