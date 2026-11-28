use serde::{Deserialize, Serialize};

use crate::services::provider::ProviderKind;

#[derive(Debug, Clone)]
pub(crate) struct RestartReportContext {
    pub provider: ProviderKind,
    pub channel_id: u64,
    pub current_msg_id: Option<u64>,
    pub channel_name: Option<String>,
}
impl RestartReportContext {
    pub(crate) fn from_bridge(
        provider: ProviderKind,
        channel_id: u64,
        current_msg_id: Option<u64>,
        channel_name: Option<String>,
    ) -> Self {
        Self {
            provider,
            channel_id,
            current_msg_id,
            channel_name,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct RestartAttempt(pub(in crate::services::discord) String);
impl RestartAttempt {
    pub(crate) fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}
