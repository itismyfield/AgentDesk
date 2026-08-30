//! Shared StreamJson turn request.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::services::provider::CancelToken;
use crate::services::provider::ProviderKind;
use crate::services::provider_auth_profile::ProviderAuthOverlay;
use crate::services::remote::RemoteProfile;

use super::policy::ConfiguredToolPolicy;
use super::session::ProviderSessionToken;

#[derive(Clone)]
pub struct ProviderTurnRequest {
    pub provider: ProviderKind,
    pub prompt: String,
    pub system_prompt: Option<String>,
    pub tool_policy: ConfiguredToolPolicy,
    pub model: Option<String>,
    /// Provider-native reasoning effort (for example, Grok's `xhigh`).
    pub reasoning_effort: Option<String>,
    pub working_directory: PathBuf,
    pub session: Option<ProviderSessionToken>,
    pub remote_profile: Option<RemoteProfile>,
    pub timeout: Duration,
    pub cancel: Option<Arc<CancelToken>>,
    pub auth_overlay: ProviderAuthOverlay,
}

impl ProviderTurnRequest {
    pub fn is_fresh(&self) -> bool {
        self.session.is_none()
    }

    pub fn for_discord_turn(
        provider: ProviderKind,
        prompt: String,
        system_prompt: Option<String>,
        tool_policy: ConfiguredToolPolicy,
        model: Option<String>,
        reasoning_effort: Option<String>,
        working_directory: PathBuf,
        session_raw: Option<&str>,
        force_fresh: bool,
        remote_profile: Option<RemoteProfile>,
        timeout: Duration,
        cancel: Option<Arc<CancelToken>>,
        channel_id: Option<u64>,
    ) -> Self {
        let session = if force_fresh {
            None
        } else {
            session_raw
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ProviderSessionToken::new_opaque)
        };
        let auth_overlay =
            crate::services::discord::spawn_auth_overlay(provider.clone(), channel_id)
                .unwrap_or_else(|_| ProviderAuthOverlay::default_for(provider.clone()));
        Self {
            provider,
            prompt,
            system_prompt,
            tool_policy,
            model,
            reasoning_effort,
            working_directory,
            session,
            remote_profile,
            timeout,
            cancel,
            auth_overlay,
        }
    }
}
