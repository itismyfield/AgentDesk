//! Internal conversation keys and opaque messenger message references.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ConversationId(pub(crate) u64);

impl ConversationId {
    pub(crate) const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MessageRef {
    pub(crate) conversation: ConversationId,
    pub(crate) raw: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_values_preserve_zero_and_opaque_message_wire() {
        for key in [0, 1, u64::MAX] {
            let conversation = ConversationId::new(key);
            assert_eq!(conversation.get(), key);
            assert_eq!(
                serde_json::to_string(&conversation).unwrap(),
                key.to_string()
            );
            assert_eq!(
                serde_json::from_str::<ConversationId>(&key.to_string()).unwrap(),
                conversation
            );
            for raw in [
                "1712345678.000001",
                "opaque/α:00042",
                "00042",
                "",
                "18446744073709551616",
            ] {
                let reference = MessageRef {
                    conversation,
                    raw: raw.to_owned(),
                };
                let wire = serde_json::to_value(&reference).unwrap();
                assert_eq!(wire, serde_json::json!({"conversation": key, "raw": raw}));
                assert_eq!(
                    serde_json::from_value::<MessageRef>(wire).unwrap(),
                    reference
                );
            }
        }
    }
}
