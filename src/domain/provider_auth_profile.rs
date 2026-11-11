//! Provider auth catalog and launch overlay values; no storage or account routing.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::provider::ProviderKind;

mod inputs;
pub(crate) use inputs::ProfileOverrides;

pub const DEFAULT_PROFILE_ID: &str = "default";

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProviderAuthProfileDef {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderAuthOverlay {
    pub profile_id: String,
    pub provider: ProviderKind,
    pub env: BTreeMap<String, String>,
    pub unset: BTreeSet<String>,
    pub home: Option<PathBuf>,
}

impl ProviderAuthOverlay {
    pub fn default_for(provider: ProviderKind) -> Self {
        Self {
            profile_id: DEFAULT_PROFILE_ID.to_string(),
            provider,
            env: BTreeMap::new(),
            unset: BTreeSet::new(),
            home: None,
        }
    }

    pub fn is_default(&self) -> bool {
        self.profile_id == DEFAULT_PROFILE_ID && self.env.is_empty() && self.unset.is_empty()
    }
}

impl fmt::Debug for ProviderAuthOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted: BTreeMap<&String, &'static str> =
            self.env.keys().map(|key| (key, "<redacted>")).collect();
        f.debug_struct("ProviderAuthOverlay")
            .field("profile_id", &self.profile_id)
            .field("provider", &self.provider.as_str())
            .field("env", &redacted)
            .field("unset", &self.unset)
            .field("home", &self.home)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProfileBinding {
    pub agent_id: String,
    pub provider: String,
    pub profile_id: String,
    /// `None` is an agent-level profile; `Some` identifies an independent channel override.
    pub channel_id: Option<String>,
    /// Whether this effective profile was explicitly pinned at the agent or
    /// channel level rather than inherited from the provider primary.
    pub is_explicit: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct FallbackPolicy {
    pub enabled: bool,
    pub fallback_profile: Option<String>,
    pub priority: Vec<String>,
    pub include_remaining: bool,
    pub cooldown_secs: u32,
}

impl Default for FallbackPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            fallback_profile: None,
            priority: Vec::new(),
            include_remaining: true,
            cooldown_secs: 300,
        }
    }
}
