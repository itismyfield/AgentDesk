//! The launch binding belongs to the durable lease, not the latest org.yaml.

use super::policy::{
    ChannelRecoveryBinding, RecoveryCatalog, RecoveryPolicy, TriggerKind, WorkspaceMode,
};
use crate::services::provider::ProviderKind;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryContext {
    pub owner_provider: String,
    pub fallback_provider: String,
    pub owner_model: Option<String>,
    /// Profile IDs only; never persist credentials in recovery WAL/state.
    #[serde(default)]
    pub owner_auth_profile: Option<String>,
    #[serde(default)]
    pub fallback_auth_profile: Option<String>,
    #[serde(default)]
    pub last_provider_error_at: Option<chrono::DateTime<chrono::Utc>>,
    pub workspace: String,
}

impl RecoveryContext {
    pub(super) fn capture(
        binding: &ChannelRecoveryBinding,
        catalog: &RecoveryCatalog,
    ) -> Option<Self> {
        let fallback = catalog.agent_provider(&binding.policy.as_ref()?.fallback_agent_id)?;
        Some(Self {
            owner_provider: binding.owner_provider.as_str().to_string(),
            fallback_provider: fallback.as_str().to_string(),
            owner_model: binding.owner_model.clone(),
            owner_auth_profile: Some(binding.owner_auth_profile.clone()),
            fallback_auth_profile: Some(super::policy::effective_auth_profile(
                None,
                &catalog
                    .agents
                    .get(&binding.policy.as_ref()?.fallback_agent_id)?
                    .auth_profile,
            )),
            last_provider_error_at: None,
            workspace: binding.workspace.clone(),
        })
    }

    pub(super) fn binding(&self, state: &super::ChannelState) -> Option<ChannelRecoveryBinding> {
        Some(ChannelRecoveryBinding {
            channel_id: state.channel_id.clone(),
            owner_agent_id: state.owner_agent_id.clone(),
            owner_provider: ProviderKind::from_str(&self.owner_provider)?,
            owner_model: self.owner_model.clone(),
            owner_auth_profile: self
                .owner_auth_profile
                .clone()
                .unwrap_or_else(|| "default".into()),
            workspace: self.workspace.clone(),
            policy: Some(RecoveryPolicy {
                enabled: true,
                fallback_agent_id: state.fallback_agent_id.clone(),
                stall_secs: super::policy::DEFAULT_STALL_SECS,
                workspace_mode: WorkspaceMode::Inherit,
                triggers: TriggerKind::all().into_iter().collect(),
            }),
        })
    }

    pub(super) fn provider(
        &self,
        state: &super::ChannelState,
        agent_id: &str,
    ) -> Option<ProviderKind> {
        if agent_id == state.owner_agent_id {
            ProviderKind::from_str(&self.owner_provider)
        } else if agent_id == state.fallback_agent_id {
            ProviderKind::from_str(&self.fallback_provider)
        } else {
            None
        }
    }
}
