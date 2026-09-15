use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::services::provider::ProviderKind;

pub const DEFAULT_STALL_SECS: u32 = 180;
pub const MIN_STALL_SECS: u32 = 60;
pub const MAX_STALL_SECS: u32 = 900;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TriggerKind {
    IdleTimeout,
    MailboxStall,
    RateLimit,
    ProcessDeath,
}

impl TriggerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IdleTimeout => "idle_timeout",
            Self::MailboxStall => "mailbox_stall",
            Self::RateLimit => "rate_limit",
            Self::ProcessDeath => "process_death",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "idle_timeout" => Some(Self::IdleTimeout),
            "mailbox_stall" => Some(Self::MailboxStall),
            "rate_limit" => Some(Self::RateLimit),
            "process_death" => Some(Self::ProcessDeath),
            _ => None,
        }
    }

    pub fn all() -> [Self; 4] {
        [
            Self::IdleTimeout,
            Self::MailboxStall,
            Self::RateLimit,
            Self::ProcessDeath,
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceMode {
    Inherit,
}

impl WorkspaceMode {
    pub fn as_str(self) -> &'static str {
        "inherit"
    }

    fn parse(value: &str) -> Result<Self, PolicyError> {
        match value.trim() {
            "" | "inherit" => Ok(Self::Inherit),
            other => Err(PolicyError::InvalidWorkspaceMode {
                value: other.to_string(),
            }),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct RecoveryConfigWire {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub fallback_agent_id: Option<String>,
    #[serde(default)]
    pub stall_secs: Option<u32>,
    #[serde(default)]
    pub workspace_mode: Option<String>,
    #[serde(default)]
    pub triggers: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryPolicy {
    pub enabled: bool,
    pub fallback_agent_id: String,
    pub stall_secs: u32,
    pub workspace_mode: WorkspaceMode,
    pub triggers: BTreeSet<TriggerKind>,
}

impl RecoveryPolicy {
    pub fn allows(&self, trigger: TriggerKind) -> bool {
        self.enabled && self.triggers.contains(&trigger)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrgAgentInput {
    pub id: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub workspace: Option<String>,
    pub recovery: Option<RecoveryConfigWire>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrgChannelInput {
    pub channel_id: String,
    pub agent: String,
    pub provider: Option<String>,
    pub workspace: Option<String>,
    pub recovery: Option<RecoveryConfigWire>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelRecoveryBinding {
    pub channel_id: String,
    pub owner_agent_id: String,
    pub owner_provider: ProviderKind,
    pub owner_model: Option<String>,
    pub workspace: String,
    pub policy: Option<RecoveryPolicy>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryCatalog {
    pub agents: BTreeMap<String, OrgAgentInput>,
    pub channels: BTreeMap<String, ChannelRecoveryBinding>,
}

impl RecoveryCatalog {
    pub fn policy_for_channel(&self, channel_id: &str) -> Option<&RecoveryPolicy> {
        self.channels
            .get(channel_id)
            .and_then(|binding| binding.policy.as_ref())
            .filter(|policy| policy.enabled)
    }

    pub fn agent_provider(&self, agent_id: &str) -> Option<ProviderKind> {
        self.agents
            .get(agent_id)
            .and_then(|agent| agent.provider.as_deref())
            .and_then(ProviderKind::from_str)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyError {
    DistinctFallback,
    MissingFallback {
        agent_id: String,
    },
    UnknownFallback {
        agent_id: String,
        fallback_agent_id: String,
    },
    UnknownOwner {
        agent_id: String,
    },
    MissingOwnerProvider {
        agent_id: String,
    },
    SameProviderFallback {
        agent_id: String,
        fallback_agent_id: String,
        provider: String,
    },
    InvalidWorkspaceMode {
        value: String,
    },
    InvalidTrigger {
        value: String,
    },
    StallSecsOutOfRange {
        value: u32,
    },
}

impl PolicyError {
    pub fn message(&self) -> String {
        match self {
            Self::DistinctFallback => {
                "recovery fallback_agent_id must differ from the primary agent".to_string()
            }
            Self::MissingFallback { agent_id } => {
                format!("recovery.enabled for '{agent_id}' requires fallback_agent_id")
            }
            Self::UnknownFallback {
                agent_id,
                fallback_agent_id,
            } => format!(
                "recovery fallback_agent_id '{fallback_agent_id}' for '{agent_id}' is not a known agent"
            ),
            Self::UnknownOwner { agent_id } => {
                format!("recovery channel references unknown agent '{agent_id}'")
            }
            Self::MissingOwnerProvider { agent_id } => {
                format!("recovery-enabled agent '{agent_id}' requires a provider")
            }
            Self::SameProviderFallback {
                agent_id,
                fallback_agent_id,
                provider,
            } => format!(
                "recovery fallback_agent_id '{fallback_agent_id}' for '{agent_id}' must use a different provider than '{provider}' until provider-level channel ownership supports agent identity"
            ),
            Self::InvalidWorkspaceMode { value } => {
                format!("recovery workspace_mode '{value}' is not supported (P0: inherit only)")
            }
            Self::InvalidTrigger { value } => {
                format!("recovery triggers contains unknown name '{value}'")
            }
            Self::StallSecsOutOfRange { value } => {
                format!(
                    "recovery stall_secs {value} must be between {MIN_STALL_SECS} and {MAX_STALL_SECS}"
                )
            }
        }
    }
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// Shared distinct-agent helper used by routines and live recovery.
pub fn validate_distinct_fallback_agent(
    agent_id: Option<&str>,
    fallback_agent_id: Option<&str>,
) -> AppResult<()> {
    let Some(agent_id) = agent_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    let Some(fallback_agent_id) = fallback_agent_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    if agent_id == fallback_agent_id {
        return Err(AppError::bad_request(
            "routine fallback_agent_id must differ from agent_id",
        ));
    }
    Ok(())
}

pub fn merge_recovery_wire(
    channel: Option<&RecoveryConfigWire>,
    agent: Option<&RecoveryConfigWire>,
) -> Option<RecoveryConfigWire> {
    let enabled = channel
        .and_then(|wire| wire.enabled)
        .or_else(|| agent.and_then(|wire| wire.enabled))?;
    if !enabled {
        return None;
    }
    Some(RecoveryConfigWire {
        enabled: Some(true),
        fallback_agent_id: first_non_empty(
            channel.and_then(|wire| wire.fallback_agent_id.clone()),
            agent.and_then(|wire| wire.fallback_agent_id.clone()),
        ),
        stall_secs: channel
            .and_then(|wire| wire.stall_secs)
            .or_else(|| agent.and_then(|wire| wire.stall_secs)),
        workspace_mode: first_non_empty(
            channel.and_then(|wire| wire.workspace_mode.clone()),
            agent.and_then(|wire| wire.workspace_mode.clone()),
        ),
        triggers: channel
            .and_then(|wire| wire.triggers.clone())
            .or_else(|| agent.and_then(|wire| wire.triggers.clone())),
    })
}

pub fn policy_from_wire(
    owner_agent_id: &str,
    known_agents: &BTreeSet<String>,
    wire: &RecoveryConfigWire,
) -> Result<RecoveryPolicy, PolicyError> {
    if wire.enabled != Some(true) {
        return Err(PolicyError::MissingFallback {
            agent_id: owner_agent_id.to_string(),
        });
    }
    let fallback_agent_id = wire
        .fallback_agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PolicyError::MissingFallback {
            agent_id: owner_agent_id.to_string(),
        })?
        .to_string();
    if validate_distinct_fallback_agent(Some(owner_agent_id), Some(&fallback_agent_id)).is_err() {
        return Err(PolicyError::DistinctFallback);
    }
    if !known_agents.contains(&fallback_agent_id) {
        return Err(PolicyError::UnknownFallback {
            agent_id: owner_agent_id.to_string(),
            fallback_agent_id,
        });
    }
    let stall_secs = match wire.stall_secs {
        Some(value) if (MIN_STALL_SECS..=MAX_STALL_SECS).contains(&value) => value,
        Some(value) => return Err(PolicyError::StallSecsOutOfRange { value }),
        None => DEFAULT_STALL_SECS,
    };
    let workspace_mode = match wire.workspace_mode.as_deref() {
        Some(value) => WorkspaceMode::parse(value)?,
        None => WorkspaceMode::Inherit,
    };
    let triggers = match wire.triggers.as_ref() {
        Some(names) => {
            let mut parsed = BTreeSet::new();
            for name in names {
                let trigger =
                    TriggerKind::parse(name).ok_or_else(|| PolicyError::InvalidTrigger {
                        value: name.clone(),
                    })?;
                parsed.insert(trigger);
            }
            parsed
        }
        None => TriggerKind::all().into_iter().collect(),
    };
    Ok(RecoveryPolicy {
        enabled: true,
        fallback_agent_id,
        stall_secs,
        workspace_mode,
        triggers,
    })
}

pub fn build_recovery_catalog(
    agents: &[OrgAgentInput],
    channels: &[OrgChannelInput],
) -> Result<RecoveryCatalog, PolicyError> {
    let known_agents: BTreeSet<String> = agents.iter().map(|agent| agent.id.clone()).collect();
    let mut agent_map = BTreeMap::new();
    for agent in agents {
        agent_map.insert(agent.id.clone(), agent.clone());
    }
    let mut catalog = RecoveryCatalog {
        agents: agent_map.clone(),
        channels: BTreeMap::new(),
    };
    for channel in channels {
        let owner = agent_map
            .get(&channel.agent)
            .ok_or_else(|| PolicyError::UnknownOwner {
                agent_id: channel.agent.clone(),
            })?;
        let wire = merge_recovery_wire(channel.recovery.as_ref(), owner.recovery.as_ref());
        let policy = match wire.as_ref() {
            Some(wire) => Some(policy_from_wire(&channel.agent, &known_agents, wire)?),
            None => None,
        };
        if policy.is_some() {
            let provider_raw = channel
                .provider
                .as_deref()
                .or(owner.provider.as_deref())
                .ok_or_else(|| PolicyError::MissingOwnerProvider {
                    agent_id: channel.agent.clone(),
                })?;
            if ProviderKind::from_str(provider_raw).is_none() {
                return Err(PolicyError::MissingOwnerProvider {
                    agent_id: channel.agent.clone(),
                });
            }
            let fallback_id = policy.as_ref().map(|item| item.fallback_agent_id.as_str());
            if let Some(fallback_id) = fallback_id {
                let fallback_provider = catalog.agent_provider(fallback_id).ok_or_else(|| {
                    PolicyError::UnknownFallback {
                        agent_id: channel.agent.clone(),
                        fallback_agent_id: fallback_id.to_string(),
                    }
                })?;
                let owner_provider = channel
                    .provider
                    .as_deref()
                    .or(owner.provider.as_deref())
                    .and_then(ProviderKind::from_str)
                    .expect("recovery owner provider was validated above");
                if fallback_provider == owner_provider {
                    return Err(PolicyError::SameProviderFallback {
                        agent_id: channel.agent.clone(),
                        fallback_agent_id: fallback_id.to_string(),
                        provider: owner_provider.as_str().to_string(),
                    });
                }
            }
        }
        let owner_provider = channel
            .provider
            .as_deref()
            .or(owner.provider.as_deref())
            .and_then(ProviderKind::from_str)
            .unwrap_or_else(|| ProviderKind::Unsupported("unknown".to_string()));
        let workspace = channel
            .workspace
            .as_deref()
            .or(owner.workspace.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(".")
            .to_string();
        catalog.channels.insert(
            channel.channel_id.clone(),
            ChannelRecoveryBinding {
                channel_id: channel.channel_id.clone(),
                owner_agent_id: channel.agent.clone(),
                owner_provider,
                owner_model: owner.model.clone(),
                workspace,
                policy,
            },
        );
    }
    Ok(catalog)
}

#[derive(Debug, Deserialize)]
struct OrgRecoveryDocument {
    #[serde(default)]
    agents: BTreeMap<String, OrgRecoveryAgentWire>,
    #[serde(default)]
    channels: Option<OrgRecoveryChannelsWire>,
}

#[derive(Debug, Deserialize)]
struct OrgRecoveryAgentWire {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    recovery: Option<RecoveryConfigWire>,
}

#[derive(Debug, Deserialize)]
struct OrgRecoveryChannelsWire {
    #[serde(default)]
    by_id: Option<BTreeMap<String, OrgRecoveryChannelWire>>,
}

#[derive(Debug, Deserialize)]
struct OrgRecoveryChannelWire {
    agent: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    recovery: Option<RecoveryConfigWire>,
}

pub fn load_org_recovery_catalog_from_yaml(yaml: &str) -> Result<RecoveryCatalog, PolicyError> {
    let document: OrgRecoveryDocument =
        serde_yaml::from_str(yaml).map_err(|_| PolicyError::UnknownOwner {
            agent_id: "<yaml>".to_string(),
        })?;
    let agents: Vec<OrgAgentInput> = document
        .agents
        .into_iter()
        .map(|(id, wire)| OrgAgentInput {
            id,
            provider: wire.provider,
            model: wire.model,
            workspace: wire.workspace,
            recovery: wire.recovery,
        })
        .collect();
    let channels: Vec<OrgChannelInput> = document
        .channels
        .and_then(|channels| channels.by_id)
        .unwrap_or_default()
        .into_iter()
        .map(|(channel_id, wire)| OrgChannelInput {
            channel_id,
            agent: wire.agent,
            provider: wire.provider,
            workspace: wire.workspace,
            recovery: wire.recovery,
        })
        .collect();
    build_recovery_catalog(&agents, &channels)
}

fn first_non_empty(primary: Option<String>, secondary: Option<String>) -> Option<String> {
    primary
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            secondary
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}
