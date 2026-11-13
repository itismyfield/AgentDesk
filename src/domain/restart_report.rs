use super::provider::ProviderKind;
use serde::{Deserialize, Serialize};

pub(crate) const RESTART_REPORT_VERSION: u32 = 2;
pub(crate) const AWAITING_REPORT_MAX_AGE: chrono::Duration = chrono::Duration::minutes(5);
pub(crate) const RESTART_REPORT_CHANNEL_ENV: &str = "AGENTDESK_REPORT_CHANNEL_ID";
pub(crate) const RESTART_REPORT_PROVIDER_ENV: &str = "AGENTDESK_REPORT_PROVIDER";
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
pub(crate) struct RestartAttempt(pub(crate) String);
impl RestartAttempt {
    pub(crate) fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RestartFailure {
    Cancelled,
    RolledBack,
    Unverified,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub(crate) enum RestartReportState {
    Awaiting,
    Succeeded,
    Failed(RestartFailure),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RestartCompletionReport {
    pub version: u32,
    pub provider: String,
    pub channel_id: u64,
    pub attempt: RestartAttempt,
    #[serde(default)]
    pub predecessor: Option<RestartAttempt>,
    pub state: RestartReportState,
    pub created_at: String,
    #[serde(default)]
    pub current_msg_id: Option<u64>,
    #[serde(default)]
    pub channel_name: Option<String>,
    #[serde(default)]
    pub user_msg_id: Option<u64>,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub doctor_summary: Option<serde_json::Value>,
}
impl RestartCompletionReport {
    pub(crate) fn provider_kind(&self) -> Option<ProviderKind> {
        ProviderKind::from_str(&self.provider)
    }
}
pub(crate) type RestartReportError = String;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettleOutcome {
    Settled,
    Absent,
    Stale,
    AlreadyTerminal,
}
pub(crate) fn restart_report_context_from_env() -> Option<RestartReportContext> {
    let provider = ProviderKind::from_str(&std::env::var(RESTART_REPORT_PROVIDER_ENV).ok()?)?;
    let channel_id = std::env::var(RESTART_REPORT_CHANNEL_ENV)
        .ok()?
        .parse()
        .ok()?;
    Some(RestartReportContext {
        provider,
        channel_id,
        current_msg_id: None,
        channel_name: None,
    })
}
