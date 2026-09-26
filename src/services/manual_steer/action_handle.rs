//! Discord button custom-id codec for the manual-steer action handle.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::operation::deserialize_fixed_str;

const CUSTOM_ID_PREFIX: &str = "msteer:";
const CUSTOM_ID_VERSION: &str = "v1";
const HANDLE_BYTES: usize = 16;

/// One-time 128-bit action nonce. A custom-id carries only this opaque handle, and every
/// new click permit gets a new one so a stale card never matches a re-queued entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ActionHandle([u8; HANDLE_BYTES]);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CustomIdError {
    /// The custom-id belongs to another component.
    NotManualSteer,
    UnsupportedVersion,
    MalformedHandle,
}

impl ActionHandle {
    pub(crate) fn generate() -> Self {
        loop {
            if let Some(handle) = Self::from_bytes(rand::random()) {
                return handle;
            }
        }
    }

    /// The all-zero value is reserved as invalid so an unset buffer never names a permit.
    pub(crate) fn from_bytes(bytes: [u8; HANDLE_BYTES]) -> Option<Self> {
        (bytes != [0; HANDLE_BYTES]).then_some(Self(bytes))
    }

    pub(crate) fn to_custom_id(self) -> String {
        format!(
            "{CUSTOM_ID_PREFIX}{CUSTOM_ID_VERSION}:{}",
            hex::encode(self.0)
        )
    }

    pub(crate) fn parse_custom_id(custom_id: &str) -> Result<Self, CustomIdError> {
        let rest = custom_id
            .strip_prefix(CUSTOM_ID_PREFIX)
            .ok_or(CustomIdError::NotManualSteer)?;
        let handle = rest
            .strip_prefix(CUSTOM_ID_VERSION)
            .and_then(|tail| tail.strip_prefix(':'))
            .ok_or(CustomIdError::UnsupportedVersion)?;
        Self::from_hex(handle).ok_or(CustomIdError::MalformedHandle)
    }

    fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    /// Accepts only the canonical lowercase form so one handle has exactly one spelling.
    fn from_hex(text: &str) -> Option<Self> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_id_round_trips_to_the_same_handle_within_discord_limit() {
        for _ in 0..64 {
            let handle = ActionHandle::generate();
            let custom_id = handle.to_custom_id();
            assert!(
                custom_id.len() <= 100,
                "Discord caps custom_id at 100: {custom_id}"
            );
            assert_eq!(ActionHandle::parse_custom_id(&custom_id), Ok(handle));
        }
    }

    #[test]
    fn custom_id_parse_rejects_foreign_versioned_and_malformed_ids() {
        let valid = ActionHandle::from_bytes([0xab; HANDLE_BYTES])
            .unwrap()
            .to_custom_id();
        let hex = &valid["msteer:v1:".len()..];
        let cases = [
            (
                "idle-recap:clear:1".to_string(),
                CustomIdError::NotManualSteer,
            ),
            (format!("MSTEER:v1:{hex}"), CustomIdError::NotManualSteer),
            (
                format!("msteer:v2:{hex}"),
                CustomIdError::UnsupportedVersion,
            ),
            (format!("msteer:v1{hex}"), CustomIdError::UnsupportedVersion),
            (
                format!("msteer:v1:{}", hex.to_uppercase()),
                CustomIdError::MalformedHandle,
            ),
            (
                format!("msteer:v1:{}", &hex[1..]),
                CustomIdError::MalformedHandle,
            ),
            (format!("msteer:v1:{hex}0"), CustomIdError::MalformedHandle),
            (format!("msteer:v1:{hex} "), CustomIdError::MalformedHandle),
            (
                format!("msteer:v1:+{}", &hex[1..]),
                CustomIdError::MalformedHandle,
            ),
            (
                format!("msteer:v1:{}", "0".repeat(32)),
                CustomIdError::MalformedHandle,
            ),
            ("msteer:v1:".to_string(), CustomIdError::MalformedHandle),
        ];
        for (custom_id, expected) in cases {
            assert_eq!(
                ActionHandle::parse_custom_id(&custom_id),
                Err(expected),
                "{custom_id}"
            );
        }
    }
}
