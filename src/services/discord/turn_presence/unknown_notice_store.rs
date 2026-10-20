use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::services::discord::runtime_store;
use crate::services::discord::turn_presence::activity::notice_identity::ExecutionIdentity;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Scope {
    pub root: Option<PathBuf>,
    pub provider: String,
    pub token_hash: String,
    pub channel: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Key {
    pub identity: ExecutionIdentity,
    pub target_label: Option<String>,
    pub reason: String,
    pub leaf: Option<String>,
    pub provisional: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub schema: u32,
    pub provider: String,
    pub token_hash: String,
    pub channel: u64,
    pub key: Key,
    pub unknown_since: DateTime<Utc>,
    pub cause_since: DateTime<Utc>,
    pub posted_at: DateTime<Utc>,
    pub id: String,
}

impl Scope {
    pub fn path(&self) -> Result<PathBuf, String> {
        let safe = |value: &str| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        };
        if !safe(&self.provider) || !safe(&self.token_hash) || self.channel == 0 {
            return Err("invalid notice namespace".into());
        }
        Ok(self
            .root
            .as_ref()
            .ok_or("notice runtime root unavailable")?
            .join("discord_turn_presence_notices")
            .join(&self.provider)
            .join(&self.token_hash)
            .join(format!("{}.json", self.channel)))
    }

    pub fn load(&self) -> Result<Option<Receipt>, String> {
        let raw = match std::fs::read_to_string(self.path()?) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        if value.is_null() {
            return Ok(None);
        }
        if !value.is_object() || !value.get("key").is_some_and(serde_json::Value::is_object) {
            return Err("invalid notice receipt shape".into());
        }
        let receipt: Receipt = serde_json::from_value(value).map_err(|e| e.to_string())?;
        let atom = |value: &str| {
            value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                && value
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        };
        if receipt.schema != 1
            || receipt.provider != self.provider
            || receipt.token_hash != self.token_hash
            || receipt.channel != self.channel
            || uuid::Uuid::parse_str(&receipt.id).is_err()
            || receipt
                .key
                .target_label
                .as_ref()
                .is_some_and(String::is_empty)
            || matches!(&receipt.key.identity, ExecutionIdentity::LegacySession(label) if receipt.key.target_label.as_ref() != Some(label))
            || (receipt.key.provisional
                && (receipt.key.reason != "catching_up" || receipt.key.leaf.is_some()))
            || !atom(&receipt.key.reason)
            || receipt.key.leaf.as_ref().is_some_and(|leaf| !atom(leaf))
            || matches!(&receipt.key.identity, ExecutionIdentity::Nonce(value) | ExecutionIdentity::LegacySession(value) if value.is_empty())
        {
            return Err("invalid notice receipt".into());
        }
        Ok(Some(receipt))
    }

    pub fn save(&self, receipt: Option<&Receipt>) -> Result<(), String> {
        let path = self.path()?;
        #[cfg(test)]
        super::notices::pause_store_for_tests(self);
        if receipt.is_none() && !path.try_exists().map_err(|error| error.to_string())? {
            return Ok(());
        }
        let json = serde_json::to_string(&receipt).map_err(|error| error.to_string())?;
        let context = runtime_store::AtomicWriteContext::new("turn_presence_notice")
            .provider(&self.provider)
            .token_hash(&self.token_hash)
            .channel_id(self.channel);
        runtime_store::critical_atomic_write(&path, &json, context)?;
        runtime_store::fsync_parent_dir(&path).map_err(|error| error.to_string())
    }
}
