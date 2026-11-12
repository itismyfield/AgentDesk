//! Structured management evidence; a display name never supplies identity fields.

use serde::{Deserialize, Serialize};

pub(crate) const IDENTITY_ENV: &str = "AGENTDESK_IDENTITY";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ManagedIdentity {
    pub v: u32,
    pub owner_scope: String,
    pub provider: Option<String>,
    pub token_namespace: Option<String>,
    pub channel_id: Option<String>,
    pub parent_channel_id: Option<String>,
    pub thread_id: Option<String>,
    pub identity_kind: Option<String>,
    pub name_kind: String,
    pub session_record_id: Option<i64>,
    pub logical_key: Option<String>,
    pub spawn_nonce: Option<String>,
    pub display_name: String,
}

impl ManagedIdentity {
    pub(crate) fn decode_environment(output: &str) -> Option<Self> {
        let value = output
            .trim_end_matches(['\r', '\n'])
            .strip_prefix("AGENTDESK_IDENTITY=")?;
        let identity: Self = serde_json::from_str(value).ok()?;
        (identity.v == 1
            && identity.owner_scope.len() == 64
            && identity.owner_scope.bytes().all(|b| b.is_ascii_hexdigit())
            && !identity.name_kind.is_empty())
        .then_some(identity)
    }

    pub(crate) fn environment_assignment(&self) -> Result<String, String> {
        serde_json::to_string(self)
            .map(|json| format!("{IDENTITY_ENV}={json}"))
            .map_err(|error| format!("encode managed session identity: {error}"))
    }
}
