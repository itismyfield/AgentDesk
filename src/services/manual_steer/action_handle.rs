//! Discord button custom-id codec for the action handle, and its random issuance. Manual steer
//! owns `msteer:v1`; a Decision button carries the same handle under `decision:v1`.

pub(crate) use super::handle_value::ActionHandle;

const CUSTOM_ID_PREFIX: &str = "msteer:";
const CUSTOM_ID_VERSION: &str = "v1";
const DECISION_PREFIX: &str = "decision:";

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

    pub(crate) fn to_custom_id(self) -> String {
        format!("{CUSTOM_ID_PREFIX}{CUSTOM_ID_VERSION}:{}", self.to_hex())
    }

    /// The handle as a Decision button's custom-id; manual steer's parse refuses it as foreign.
    pub(crate) fn to_decision_custom_id(self) -> String {
        format!("{DECISION_PREFIX}{CUSTOM_ID_VERSION}:{}", self.to_hex())
    }

    /// Whether `custom_id` is a well-formed Decision button handle.
    pub(crate) fn is_decision_custom_id(custom_id: &str) -> bool {
        (custom_id.strip_prefix(DECISION_PREFIX))
            .and_then(|rest| rest.strip_prefix(CUSTOM_ID_VERSION)?.strip_prefix(':'))
            .is_some_and(|handle| Self::from_hex(handle).is_some())
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
}

#[cfg(test)]
mod tests {
    use super::super::handle_value::HANDLE_BYTES;
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
            (format!("decision:v1:{hex}"), CustomIdError::NotManualSteer),
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
