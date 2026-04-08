#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::utils::format::expand_tilde_path;

pub(super) const OPENCLAW_CONFIG_NAME: &str = "openclaw.json";
const INCLUDE_KEY: &str = "$include";
const MAX_INCLUDE_DEPTH: usize = 10;

const PRUNE_DIR_NAMES: &[&str] = &[".git", "node_modules", "target", "dist", ".venv", ".cache"];

#[derive(Clone, Debug)]
struct OpenClawCandidate {
    root: PathBuf,
    config_path: PathBuf,
    config: OpenClawConfig,
    resolved_config_json: String,
    resolved_config_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub(super) struct ResolvedSourceRoot {
    pub(super) root: PathBuf,
    pub(super) config_path: PathBuf,
    pub(super) config: OpenClawConfig,
    pub(super) resolved_config_json: String,
    pub(super) resolved_config_paths: Vec<PathBuf>,
    pub(super) discovered_candidate_roots: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct OpenClawConfig {
    #[serde(default)]
    pub(super) agents: OpenClawAgentsConfig,
    #[serde(default)]
    pub(super) bindings: Vec<OpenClawBindingConfig>,
    #[serde(default)]
    pub(super) channels: OpenClawChannelsConfig,
    #[serde(default)]
    pub(super) secrets: Option<OpenClawSecretsConfig>,
    #[serde(default)]
    pub(super) tools: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct OpenClawSecretsConfig {
    #[serde(default)]
    pub(super) providers: BTreeMap<String, OpenClawSecretProviderConfig>,
    #[serde(default)]
    pub(super) defaults: OpenClawSecretDefaultsConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct OpenClawSecretDefaultsConfig {
    #[serde(default)]
    pub(super) env: Option<String>,
    #[serde(default)]
    pub(super) file: Option<String>,
    #[serde(default)]
    pub(super) exec: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub(super) enum OpenClawSecretProviderConfig {
    Env {
        #[serde(default)]
        allowlist: Vec<String>,
    },
    File {
        path: String,
        #[serde(default)]
        mode: Option<String>,
        #[serde(rename = "timeoutMs")]
        #[serde(default)]
        timeout_ms: Option<u64>,
        #[serde(rename = "maxBytes")]
        #[serde(default)]
        max_bytes: Option<u64>,
    },
    Exec {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(rename = "timeoutMs")]
        #[serde(default)]
        timeout_ms: Option<u64>,
        #[serde(rename = "noOutputTimeoutMs")]
        #[serde(default)]
        no_output_timeout_ms: Option<u64>,
        #[serde(rename = "maxOutputBytes")]
        #[serde(default)]
        max_output_bytes: Option<u64>,
        #[serde(rename = "jsonOnly")]
        #[serde(default)]
        json_only: Option<bool>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(rename = "passEnv")]
        #[serde(default)]
        pass_env: Vec<String>,
        #[serde(rename = "trustedDirs")]
        #[serde(default)]
        trusted_dirs: Vec<String>,
        #[serde(rename = "allowInsecurePath")]
        #[serde(default)]
        allow_insecure_path: Option<bool>,
        #[serde(rename = "allowSymlinkCommand")]
        #[serde(default)]
        allow_symlink_command: Option<bool>,
    },
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct OpenClawAgentsConfig {
    #[serde(rename = "defaultAgent")]
    pub(super) legacy_default_agent: Option<String>,
    #[serde(default)]
    pub(super) defaults: OpenClawAgentDefaultsConfig,
    #[serde(default)]
    pub(super) list: Vec<OpenClawAgentConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct OpenClawAgentDefaultsConfig {
    #[serde(default)]
    pub(super) workspace: Option<String>,
    #[serde(default)]
    pub(super) model: Option<OpenClawModelConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum OpenClawModelConfig {
    String(String),
    Structured {
        #[serde(default)]
        primary: Option<String>,
        #[serde(default)]
        fallbacks: Vec<String>,
    },
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct OpenClawAgentConfig {
    pub(super) id: String,
    #[serde(default)]
    pub(super) default: bool,
    #[serde(default)]
    pub(super) name: Option<String>,
    #[serde(default)]
    pub(super) workspace: Option<String>,
    #[serde(default)]
    pub(super) model: Option<OpenClawModelConfig>,
    #[serde(default)]
    pub(super) identity: Option<OpenClawIdentityConfig>,
    #[serde(default)]
    pub(super) runtime: serde_json::Value,
    #[serde(default)]
    pub(super) tools: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct OpenClawIdentityConfig {
    #[serde(default)]
    pub(super) emoji: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct OpenClawChannelsConfig {
    #[serde(default)]
    pub(super) discord: Option<OpenClawDiscordConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct OpenClawDiscordConfig {
    #[serde(rename = "defaultAccount")]
    pub(super) default_account: Option<String>,
    #[serde(default)]
    pub(super) token: Option<serde_json::Value>,
    #[serde(rename = "allowBots")]
    #[serde(default)]
    pub(super) allow_bots: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) guilds: BTreeMap<String, OpenClawDiscordGuildConfig>,
    #[serde(default)]
    pub(super) accounts: BTreeMap<String, OpenClawDiscordAccountConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct OpenClawDiscordAccountConfig {
    #[serde(default)]
    pub(super) enabled: Option<bool>,
    #[serde(default)]
    pub(super) token: Option<serde_json::Value>,
    #[serde(rename = "allowBots")]
    #[serde(default)]
    pub(super) allow_bots: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) guilds: BTreeMap<String, OpenClawDiscordGuildConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct OpenClawDiscordGuildConfig {
    #[serde(default)]
    pub(super) require_mention: Option<bool>,
    #[serde(default)]
    pub(super) ignore_other_mentions: Option<bool>,
    #[serde(default)]
    pub(super) users: Vec<String>,
    #[serde(default)]
    pub(super) roles: Vec<String>,
    #[serde(default)]
    pub(super) tools: Option<serde_json::Value>,
    #[serde(rename = "toolsBySender")]
    #[serde(default)]
    pub(super) tools_by_sender: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) channels: BTreeMap<String, OpenClawDiscordGuildChannelConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct OpenClawDiscordGuildChannelConfig {
    #[serde(default)]
    pub(super) allow: Option<bool>,
    #[serde(default)]
    pub(super) require_mention: Option<bool>,
    #[serde(default)]
    pub(super) ignore_other_mentions: Option<bool>,
    #[serde(default)]
    pub(super) users: Vec<String>,
    #[serde(default)]
    pub(super) roles: Vec<String>,
    #[serde(default)]
    pub(super) tools: Option<serde_json::Value>,
    #[serde(rename = "toolsBySender")]
    #[serde(default)]
    pub(super) tools_by_sender: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) system_prompt: Option<String>,
    #[serde(default)]
    pub(super) enabled: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct OpenClawBindingConfig {
    #[serde(default)]
    pub(super) r#type: Option<String>,
    #[serde(rename = "agentId")]
    pub(super) agent_id: String,
    pub(super) r#match: OpenClawBindingMatchConfig,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct OpenClawBindingMatchConfig {
    pub(super) channel: String,
    #[serde(rename = "accountId")]
    pub(super) account_id: Option<String>,
    #[serde(default)]
    pub(super) peer: Option<OpenClawBindingPeerConfig>,
    #[serde(rename = "guildId")]
    #[serde(default)]
    pub(super) guild_id: Option<String>,
    #[serde(rename = "teamId")]
    #[serde(default)]
    pub(super) team_id: Option<String>,
    #[serde(default)]
    pub(super) roles: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct OpenClawBindingPeerConfig {
    pub(super) kind: String,
    pub(super) id: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct DiscordChannelImportResult {
    pub(super) agent_channels: BTreeMap<String, Vec<String>>,
    pub(super) selected_account_ids: Vec<String>,
    pub(super) warnings: Vec<String>,
    pub(super) accounts: Vec<DiscordAccountImportPlan>,
    pub(super) bindings: Vec<DiscordBindingImportPlan>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct DiscordAccountImportPlan {
    pub(super) account_id: String,
    pub(super) source: &'static str,
    pub(super) enabled: bool,
    pub(super) has_token: bool,
    pub(super) token_kind: String,
    pub(super) importable: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct DiscordBindingImportPlan {
    pub(super) agent_id: String,
    pub(super) requested_account_id: Option<String>,
    pub(super) selected_account_id: Option<String>,
    pub(super) candidate_account_ids: Vec<String>,
    pub(super) channel_ids: Vec<String>,
    pub(super) mode: &'static str,
    pub(super) reason: Option<String>,
    pub(super) warnings: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct ToolPolicyScanResult {
    pub(super) global_sources: Vec<String>,
    pub(super) global_normalized_candidate_tools: Vec<String>,
    pub(super) global_unsupported_tools: Vec<String>,
    pub(super) has_channel_scoped_policy: bool,
    pub(super) has_sender_scoped_policy: bool,
    pub(super) has_subagent_scoped_policy: bool,
    pub(super) agents: Vec<ToolPolicyAgentScan>,
    pub(super) warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ToolPolicyAgentScan {
    pub(super) agent_id: String,
    pub(super) sources: Vec<String>,
    pub(super) normalized_candidate_tools: Vec<String>,
    pub(super) unsupported_tools: Vec<String>,
    pub(super) has_channel_scoped_policy: bool,
    pub(super) has_sender_scoped_policy: bool,
    pub(super) has_subagent_scoped_policy: bool,
}

#[derive(Default)]
struct ToolPolicySignals {
    sources: Vec<String>,
    normalized_candidate_tools: Vec<String>,
    unsupported_tools: Vec<String>,
    has_channel_scoped_policy: bool,
    has_sender_scoped_policy: bool,
    has_subagent_scoped_policy: bool,
}

impl OpenClawAgentsConfig {
    pub(super) fn default_agent_id(&self) -> Result<Option<&str>, String> {
        let mut flagged_default = None;
        for agent in &self.list {
            if agent.default {
                if let Some(existing) = flagged_default {
                    return Err(format!(
                        "openclaw.json marks multiple default agents in agents.list: '{existing}' and '{}'.",
                        agent.id
                    ));
                }
                flagged_default = Some(agent.id.as_str());
            }
        }

        if let Some(legacy_default) = self.legacy_default_agent.as_deref() {
            let exists = self.list.iter().any(|agent| agent.id == legacy_default);
            if !exists {
                return Err(format!(
                    "openclaw.json legacy defaultAgent '{}' is missing from agents.list.",
                    legacy_default
                ));
            }

            if let Some(flagged_default) = flagged_default {
                if flagged_default != legacy_default {
                    return Err(format!(
                        "openclaw.json default agent markers disagree: agents.list[].default='{}', legacy defaultAgent='{}'.",
                        flagged_default, legacy_default
                    ));
                }
            } else {
                return Ok(Some(legacy_default));
            }
        }

        Ok(flagged_default)
    }
}

impl OpenClawModelConfig {
    pub(super) fn primary(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value.as_str()),
            Self::Structured { primary, .. } => primary.as_deref(),
        }
    }
}

pub(super) fn normalize_tool_names_with_unmapped<I>(tools: I) -> (Vec<String>, Vec<String>)
where
    I: IntoIterator<Item = String>,
{
    const KNOWN: &[&str] = &[
        "Bash",
        "Read",
        "Edit",
        "Write",
        "Glob",
        "Grep",
        "Task",
        "TaskOutput",
        "TaskStop",
        "WebFetch",
        "WebSearch",
        "NotebookEdit",
        "Skill",
        "TaskCreate",
        "TaskGet",
        "TaskUpdate",
        "TaskList",
        "AskUserQuestion",
        "EnterPlanMode",
        "ExitPlanMode",
    ];

    let mut seen_known = BTreeSet::new();
    let mut seen_unknown = BTreeSet::new();
    let mut normalized = Vec::new();
    let mut unsupported = Vec::new();

    for tool in tools {
        let trimmed = tool.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(canonical) = KNOWN
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(trimmed))
        {
            if seen_known.insert((*canonical).to_string()) {
                normalized.push((*canonical).to_string());
            }
        } else if seen_unknown.insert(trimmed.to_string()) {
            unsupported.push(trimmed.to_string());
        }
    }

    (normalized, unsupported)
}

fn collect_tool_policy_candidates(
    value: Option<&serde_json::Value>,
    signals: &mut ToolPolicySignals,
) {
    let Some(value) = value else {
        return;
    };
    let mut raw_tools = Vec::new();
    let mut has_subagent_scoped_policy = false;
    let mut path = Vec::new();
    collect_tool_policy_candidates_inner(
        value,
        &mut path,
        &mut raw_tools,
        &mut has_subagent_scoped_policy,
    );
    let (normalized, unsupported) = normalize_tool_names_with_unmapped(raw_tools);
    signals.normalized_candidate_tools.extend(normalized);
    signals.unsupported_tools.extend(unsupported);
    signals.has_subagent_scoped_policy |= has_subagent_scoped_policy;
}

fn collect_tool_policy_candidates_inner(
    value: &serde_json::Value,
    path: &mut Vec<String>,
    raw_tools: &mut Vec<String>,
    has_subagent_scoped_policy: &mut bool,
) {
    match value {
        serde_json::Value::Array(items) => {
            let should_collect = path.is_empty()
                || path
                    .last()
                    .map(|key| {
                        key.eq_ignore_ascii_case("allow") || key.eq_ignore_ascii_case("alsoAllow")
                    })
                    .unwrap_or(false);
            if should_collect {
                raw_tools.extend(
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(ToOwned::to_owned),
                );
            }
            for item in items {
                if item.is_object() || item.is_array() {
                    path.push("[]".to_string());
                    collect_tool_policy_candidates_inner(
                        item,
                        path,
                        raw_tools,
                        has_subagent_scoped_policy,
                    );
                    path.pop();
                }
            }
        }
        serde_json::Value::Object(map) => {
            for (key, nested) in map {
                if key.eq_ignore_ascii_case("subagents") {
                    *has_subagent_scoped_policy = true;
                }
                path.push(key.clone());
                collect_tool_policy_candidates_inner(
                    nested,
                    path,
                    raw_tools,
                    has_subagent_scoped_policy,
                );
                path.pop();
            }
        }
        _ => {}
    }
}

fn merge_tool_policy_signals(target: &mut ToolPolicySignals, source: &ToolPolicySignals) {
    target.sources.extend(source.sources.iter().cloned());
    target
        .normalized_candidate_tools
        .extend(source.normalized_candidate_tools.iter().cloned());
    target
        .unsupported_tools
        .extend(source.unsupported_tools.iter().cloned());
    target.has_channel_scoped_policy |= source.has_channel_scoped_policy;
    target.has_sender_scoped_policy |= source.has_sender_scoped_policy;
    target.has_subagent_scoped_policy |= source.has_subagent_scoped_policy;
}

fn finalize_tool_policy_signals(signals: &mut ToolPolicySignals) {
    signals.sources.sort();
    signals.sources.dedup();
    signals.normalized_candidate_tools.sort();
    signals.normalized_candidate_tools.dedup();
    signals.unsupported_tools.sort();
    signals.unsupported_tools.dedup();
}

pub(super) fn scan_tool_policy(
    config: &OpenClawConfig,
    selected_agent_ids: &BTreeSet<String>,
) -> ToolPolicyScanResult {
    let mut global_signals = ToolPolicySignals::default();
    let mut agent_entries = Vec::new();

    if config.tools.is_some() {
        global_signals.sources.push("tools".to_string());
        collect_tool_policy_candidates(config.tools.as_ref(), &mut global_signals);
    }
    collect_discord_tool_sources(&config.channels, &mut global_signals, "channels.discord");
    finalize_tool_policy_signals(&mut global_signals);

    for agent in config
        .agents
        .list
        .iter()
        .filter(|agent| selected_agent_ids.contains(&agent.id))
    {
        let mut signals = ToolPolicySignals::default();
        merge_tool_policy_signals(&mut signals, &global_signals);
        if agent.tools.is_some() {
            signals
                .sources
                .push(format!("agents.list[{}].tools", agent.id));
            collect_tool_policy_candidates(agent.tools.as_ref(), &mut signals);
        }
        finalize_tool_policy_signals(&mut signals);
        agent_entries.push(ToolPolicyAgentScan {
            agent_id: agent.id.clone(),
            sources: signals.sources,
            normalized_candidate_tools: signals.normalized_candidate_tools,
            unsupported_tools: signals.unsupported_tools,
            has_channel_scoped_policy: signals.has_channel_scoped_policy,
            has_sender_scoped_policy: signals.has_sender_scoped_policy,
            has_subagent_scoped_policy: signals.has_subagent_scoped_policy,
        });
    }

    ToolPolicyScanResult {
        global_sources: global_signals.sources,
        global_normalized_candidate_tools: global_signals.normalized_candidate_tools,
        global_unsupported_tools: global_signals.unsupported_tools,
        has_channel_scoped_policy: global_signals.has_channel_scoped_policy,
        has_sender_scoped_policy: global_signals.has_sender_scoped_policy,
        has_subagent_scoped_policy: global_signals.has_subagent_scoped_policy,
        agents: agent_entries,
        warnings: if global_signals.has_sender_scoped_policy {
            vec![
                "OpenClaw Discord sender-scoped tool policy remains report-only in AgentDesk."
                    .to_string(),
            ]
        } else {
            Vec::new()
        },
    }
}

pub(super) fn collect_representable_discord_channel_imports(
    config: &OpenClawConfig,
    selected_agent_ids: &BTreeSet<String>,
) -> DiscordChannelImportResult {
    let mut warnings = Vec::new();
    let mut records = Vec::new();
    let accounts = collect_importable_discord_accounts(config.channels.discord.as_ref());

    let discord_bindings = config
        .bindings
        .iter()
        .filter(|binding| {
            binding.r#type.as_deref().unwrap_or("route") == "route"
                && binding.r#match.channel.eq_ignore_ascii_case("discord")
                && selected_agent_ids.contains(&binding.agent_id)
        })
        .collect::<Vec<_>>();

    if discord_bindings.is_empty() {
        return DiscordChannelImportResult {
            accounts,
            ..DiscordChannelImportResult::default()
        };
    }

    let Some(discord) = config.channels.discord.as_ref() else {
        warnings.push(
            "OpenClaw config has Discord route bindings but no channels.discord section; channel bindings stay preview-only."
                .to_string(),
        );
        return DiscordChannelImportResult {
            accounts,
            warnings,
            ..DiscordChannelImportResult::default()
        };
    };

    let mut binding_plans = Vec::new();
    for binding in discord_bindings {
        if !binding_is_representable(binding) {
            let reason = format!(
                "Discord binding for agent '{}' is not representable in AgentDesk org.yaml.",
                binding.agent_id
            );
            warnings.push(format!("{reason} It stays preview-only."));
            binding_plans.push(DiscordBindingImportPlan {
                agent_id: binding.agent_id.clone(),
                requested_account_id: binding.r#match.account_id.clone(),
                selected_account_id: None,
                candidate_account_ids: Vec::new(),
                channel_ids: Vec::new(),
                mode: "preview_only",
                reason: Some(reason),
                warnings: Vec::new(),
            });
            continue;
        }

        let mut binding_warnings = Vec::new();
        let resolution =
            resolve_binding_channel_ids(discord, binding, &accounts, &mut binding_warnings);
        warnings.extend(binding_warnings.iter().cloned());
        let channel_ids = resolution
            .channel_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if channel_ids.is_empty() {
            let reason = resolution.reason.clone().unwrap_or_else(|| {
                format!(
                    "Discord binding for agent '{}' did not resolve any importable live channel ids.",
                    binding.agent_id
                )
            });
            warnings.push(reason.clone());
            binding_plans.push(DiscordBindingImportPlan {
                agent_id: binding.agent_id.clone(),
                requested_account_id: binding.r#match.account_id.clone(),
                selected_account_id: resolution.selected_account_id.clone(),
                candidate_account_ids: resolution.candidate_account_ids.clone(),
                channel_ids: Vec::new(),
                mode: "preview_only",
                reason: Some(reason),
                warnings: dedupe_warnings(binding_warnings),
            });
            continue;
        }

        for channel_id in channel_ids {
            records.push((
                binding.agent_id.clone(),
                channel_id,
                resolution
                    .selected_account_id
                    .clone()
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
            ));
        }

        binding_plans.push(DiscordBindingImportPlan {
            agent_id: binding.agent_id.clone(),
            requested_account_id: binding.r#match.account_id.clone(),
            selected_account_id: resolution.selected_account_id,
            candidate_account_ids: resolution.candidate_account_ids,
            channel_ids: resolution.channel_ids,
            mode: "live_applicable",
            reason: None,
            warnings: dedupe_warnings(binding_warnings),
        });
    }

    let mut by_channel = BTreeMap::<String, BTreeSet<String>>::new();
    for (agent_id, channel_id, _) in &records {
        by_channel
            .entry(channel_id.clone())
            .or_default()
            .insert(agent_id.clone());
    }
    let conflicts = by_channel
        .iter()
        .filter(|(_, agent_ids)| agent_ids.len() > 1)
        .map(|(channel_id, _)| channel_id.clone())
        .collect::<BTreeSet<_>>();
    if !conflicts.is_empty() {
        warnings.push(format!(
            "Some Discord channel ids resolve to multiple selected agents and will be skipped from live channel imports: {}.",
            conflicts.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    let mut agent_channels = BTreeMap::<String, BTreeSet<String>>::new();
    let mut selected_account_ids = BTreeSet::<String>::new();
    for (agent_id, channel_id, account_ids) in records {
        if conflicts.contains(&channel_id) {
            continue;
        }
        agent_channels
            .entry(agent_id)
            .or_default()
            .insert(channel_id);
        selected_account_ids.extend(account_ids);
    }

    for binding in &mut binding_plans {
        if binding.mode != "live_applicable" {
            continue;
        }
        let conflicting_channels = binding
            .channel_ids
            .iter()
            .filter(|channel_id| conflicts.contains(*channel_id))
            .cloned()
            .collect::<BTreeSet<_>>();
        if conflicting_channels.is_empty() {
            continue;
        }
        binding
            .channel_ids
            .retain(|channel_id| !conflicts.contains(channel_id));
        let warning = format!(
            "Conflicting Discord channel ids were skipped from live import: {}.",
            conflicting_channels
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ")
        );
        binding.warnings.push(warning);
        if binding.channel_ids.is_empty() {
            binding.mode = "preview_only";
            binding.reason =
                Some("All resolved channel ids conflict with another selected agent.".to_string());
        }
        binding.warnings = dedupe_warnings(std::mem::take(&mut binding.warnings));
    }

    DiscordChannelImportResult {
        agent_channels: agent_channels
            .into_iter()
            .map(|(agent_id, channel_ids)| (agent_id, channel_ids.into_iter().collect()))
            .collect(),
        selected_account_ids: selected_account_ids.into_iter().collect(),
        warnings: dedupe_warnings(warnings),
        accounts,
        bindings: binding_plans,
    }
}

fn binding_is_representable(binding: &OpenClawBindingConfig) -> bool {
    binding.r#match.peer.is_none()
        && binding.r#match.guild_id.is_none()
        && binding.r#match.team_id.is_none()
        && binding.r#match.roles.is_empty()
}

#[derive(Clone, Debug)]
struct BindingResolution {
    selected_account_id: Option<String>,
    candidate_account_ids: Vec<String>,
    channel_ids: Vec<String>,
    reason: Option<String>,
}

fn collect_discord_tool_sources(
    channels: &OpenClawChannelsConfig,
    signals: &mut ToolPolicySignals,
    prefix: &str,
) {
    let Some(discord) = channels.discord.as_ref() else {
        return;
    };

    collect_discord_guild_tool_sources(&discord.guilds, signals, &format!("{prefix}.guilds"));
    for (account_id, account) in &discord.accounts {
        collect_discord_guild_tool_sources(
            &account.guilds,
            signals,
            &format!("{prefix}.accounts.{account_id}.guilds"),
        );
    }
}

fn collect_discord_guild_tool_sources(
    guilds: &BTreeMap<String, OpenClawDiscordGuildConfig>,
    signals: &mut ToolPolicySignals,
    prefix: &str,
) {
    for (guild_id, guild) in guilds {
        if guild.tools.is_some() {
            signals.sources.push(format!("{prefix}.{guild_id}.tools"));
            signals.has_channel_scoped_policy = true;
            collect_tool_policy_candidates(guild.tools.as_ref(), signals);
        }
        if guild.tools_by_sender.is_some() {
            signals.has_channel_scoped_policy = true;
            signals.has_sender_scoped_policy = true;
            signals
                .sources
                .push(format!("{prefix}.{guild_id}.toolsBySender"));
            collect_tool_policy_candidates(guild.tools_by_sender.as_ref(), signals);
        }
        for (channel_id, channel) in &guild.channels {
            if channel.tools.is_some() {
                signals
                    .sources
                    .push(format!("{prefix}.{guild_id}.channels.{channel_id}.tools"));
                signals.has_channel_scoped_policy = true;
                collect_tool_policy_candidates(channel.tools.as_ref(), signals);
            }
            if channel.tools_by_sender.is_some() {
                signals.has_channel_scoped_policy = true;
                signals.has_sender_scoped_policy = true;
                signals.sources.push(format!(
                    "{prefix}.{guild_id}.channels.{channel_id}.toolsBySender"
                ));
                collect_tool_policy_candidates(channel.tools_by_sender.as_ref(), signals);
            }
        }
    }
}

fn collect_importable_discord_accounts(
    discord: Option<&OpenClawDiscordConfig>,
) -> Vec<DiscordAccountImportPlan> {
    let Some(discord) = discord else {
        return Vec::new();
    };

    let mut accounts = Vec::new();
    if let Some(token) = discord.token.as_ref() {
        accounts.push(DiscordAccountImportPlan {
            account_id: "default".to_string(),
            source: "top_level",
            enabled: true,
            has_token: true,
            token_kind: token_kind(token).to_string(),
            importable: true,
        });
    }

    for (account_id, account) in &discord.accounts {
        let enabled = account.enabled != Some(false);
        let has_token = account.token.is_some();
        accounts.push(DiscordAccountImportPlan {
            account_id: account_id.clone(),
            source: "named",
            enabled,
            has_token,
            token_kind: account
                .token
                .as_ref()
                .map(token_kind)
                .unwrap_or("missing")
                .to_string(),
            importable: enabled && has_token,
        });
    }

    accounts.sort_by(|left, right| left.account_id.cmp(&right.account_id));
    accounts
}

fn resolve_binding_channel_ids(
    discord: &OpenClawDiscordConfig,
    binding: &OpenClawBindingConfig,
    accounts: &[DiscordAccountImportPlan],
    warnings: &mut Vec<String>,
) -> BindingResolution {
    let mut channel_ids = BTreeSet::new();
    let importable_account_ids = accounts
        .iter()
        .filter(|account| account.importable)
        .map(|account| account.account_id.clone())
        .collect::<Vec<_>>();
    let selected_account_id = match binding.r#match.account_id.as_deref() {
        Some("*") => {
            return BindingResolution {
                selected_account_id: None,
                candidate_account_ids: importable_account_ids,
                channel_ids: Vec::new(),
                reason: Some(format!(
                    "Discord binding for agent '{}' uses wildcard account '*' and stays preview-only.",
                    binding.agent_id
                )),
            };
        }
        Some(account_id) => {
            resolve_explicit_account(discord, binding, account_id, accounts, warnings)
        }
        None => resolve_implicit_account(discord, binding, accounts),
    };

    let Some(account_id) = selected_account_id.clone() else {
        if importable_account_ids.is_empty() {
            channel_ids.extend(collect_channels_from_guilds(
                &discord.guilds,
                "channels.discord.guilds",
                warnings,
            ));
            return BindingResolution {
                selected_account_id: None,
                candidate_account_ids: Vec::new(),
                channel_ids: channel_ids.into_iter().collect(),
                reason: None,
            };
        }
        return BindingResolution {
            selected_account_id: None,
            candidate_account_ids: importable_account_ids,
            channel_ids: Vec::new(),
            reason: Some(format!(
                "Discord binding for agent '{}' could not select a unique importable account and stays preview-only.",
                binding.agent_id
            )),
        };
    };

    if account_id != "default" {
        if let Some(account) = discord.accounts.get(&account_id) {
            channel_ids.extend(collect_channels_from_guilds(
                &account.guilds,
                &format!("channels.discord.accounts.{account_id}.guilds"),
                warnings,
            ));
        }
    }

    if channel_ids.is_empty() {
        channel_ids.extend(collect_channels_from_guilds(
            &discord.guilds,
            "channels.discord.guilds",
            warnings,
        ));
    }

    BindingResolution {
        selected_account_id: Some(account_id),
        candidate_account_ids: importable_account_ids,
        channel_ids: channel_ids.into_iter().collect(),
        reason: None,
    }
}

fn resolve_explicit_account(
    discord: &OpenClawDiscordConfig,
    binding: &OpenClawBindingConfig,
    account_id: &str,
    accounts: &[DiscordAccountImportPlan],
    warnings: &mut Vec<String>,
) -> Option<String> {
    if account_id == "default" {
        if discord.token.is_some() {
            return Some("default".to_string());
        }
        warnings.push(format!(
            "Discord binding for agent '{}' references default account but channels.discord.token is missing.",
            binding.agent_id
        ));
        return None;
    }

    let Some(account) = discord.accounts.get(account_id) else {
        warnings.push(format!(
            "Discord binding for agent '{}' references unknown account '{}'.",
            binding.agent_id, account_id
        ));
        return None;
    };

    if account.enabled == Some(false) {
        warnings.push(format!(
            "Discord binding for agent '{}' points at disabled account '{}'.",
            binding.agent_id, account_id
        ));
        return None;
    }

    if !accounts
        .iter()
        .any(|candidate| candidate.account_id == account_id && candidate.importable)
    {
        warnings.push(format!(
            "Discord binding for agent '{}' points at account '{}' without an importable token.",
            binding.agent_id, account_id
        ));
        return None;
    }

    Some(account_id.to_string())
}

fn resolve_implicit_account(
    discord: &OpenClawDiscordConfig,
    binding: &OpenClawBindingConfig,
    accounts: &[DiscordAccountImportPlan],
) -> Option<String> {
    let importable_accounts = accounts
        .iter()
        .filter(|account| account.importable)
        .map(|account| account.account_id.clone())
        .collect::<Vec<_>>();
    if importable_accounts.len() == 1 {
        return importable_accounts.into_iter().next();
    }

    if let Some(default_account) = discord.default_account.as_ref() {
        if accounts
            .iter()
            .any(|account| account.account_id == *default_account && account.importable)
        {
            return Some(default_account.clone());
        }
    }

    if accounts
        .iter()
        .any(|account| account.account_id == "default" && account.importable)
    {
        return Some("default".to_string());
    }

    let _ = binding;
    None
}

pub(super) fn token_kind(token: &serde_json::Value) -> &'static str {
    match token {
        serde_json::Value::String(value) => {
            let trimmed = value.trim();
            if trimmed.starts_with("${") && trimmed.ends_with('}') {
                "env"
            } else {
                "plaintext"
            }
        }
        serde_json::Value::Object(map) => {
            if let Some(source) = map.get("source").and_then(serde_json::Value::as_str) {
                match source {
                    "env" => "env",
                    "file" => "file",
                    "exec" => "exec",
                    _ => "structured",
                }
            } else if map.contains_key("env") {
                "env"
            } else if map.contains_key("file") {
                "file"
            } else if map.contains_key("exec") {
                "exec"
            } else {
                "structured"
            }
        }
        serde_json::Value::Null => "missing",
        _ => "structured",
    }
}

fn collect_channels_from_guilds(
    guilds: &BTreeMap<String, OpenClawDiscordGuildConfig>,
    source_label: &str,
    warnings: &mut Vec<String>,
) -> BTreeSet<String> {
    let mut channel_ids = BTreeSet::new();
    for (guild_id, guild) in guilds {
        if !guild_entry_is_representable(guild) {
            warnings.push(format!(
                "{source_label}.{guild_id} has guild-level Discord routing semantics AgentDesk cannot preserve; keeping those channels preview-only."
            ));
            continue;
        }
        for (channel_id, channel) in &guild.channels {
            if channel.enabled == Some(false) || channel.allow != Some(true) {
                continue;
            }
            if !guild_channel_is_representable(channel) {
                warnings.push(format!(
                    "{source_label}.{guild_id}.channels.{channel_id} has channel-level Discord semantics AgentDesk cannot preserve; keeping it preview-only."
                ));
                continue;
            }
            channel_ids.insert(channel_id.clone());
        }
    }
    channel_ids
}

fn guild_entry_is_representable(guild: &OpenClawDiscordGuildConfig) -> bool {
    guild.require_mention.is_none()
        && guild.ignore_other_mentions.is_none()
        && guild.users.is_empty()
        && guild.roles.is_empty()
        && guild.tools.is_none()
        && guild.tools_by_sender.is_none()
}

fn guild_channel_is_representable(channel: &OpenClawDiscordGuildChannelConfig) -> bool {
    channel.require_mention.is_none()
        && channel.ignore_other_mentions.is_none()
        && channel.users.is_empty()
        && channel.roles.is_empty()
        && channel.tools.is_none()
        && channel.tools_by_sender.is_none()
        && channel.system_prompt.is_none()
}

fn dedupe_warnings(warnings: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    warnings
        .into_iter()
        .filter(|warning| seen.insert(warning.clone()))
        .collect()
}

pub(super) fn resolve_source_root(
    raw_root_path: Option<&str>,
    cwd: &Path,
    runtime_root: Option<&Path>,
) -> Result<ResolvedSourceRoot, String> {
    let requested_path = match raw_root_path {
        Some(raw) => expand_tilde_path(raw),
        None => cwd.to_path_buf(),
    };
    let absolute_path = absolutize_path(cwd, &requested_path);
    let metadata = fs::metadata(&absolute_path).map_err(|e| {
        format!(
            "Failed to access OpenClaw path '{}': {e}",
            absolute_path.display()
        )
    })?;

    if metadata.is_file() {
        if absolute_path.file_name().and_then(|name| name.to_str()) != Some(OPENCLAW_CONFIG_NAME) {
            return Err(format!(
                "Expected '{}' or a directory to search, got file '{}'.",
                OPENCLAW_CONFIG_NAME,
                absolute_path.display()
            ));
        }
        let candidate = load_candidate(&absolute_path)?;
        return Ok(ResolvedSourceRoot {
            root: candidate.root.clone(),
            config_path: candidate.config_path.clone(),
            config: candidate.config,
            resolved_config_json: candidate.resolved_config_json,
            resolved_config_paths: candidate.resolved_config_paths,
            discovered_candidate_roots: vec![candidate.root],
        });
    }

    if !metadata.is_dir() {
        return Err(format!(
            "OpenClaw path '{}' is neither a file nor a directory.",
            absolute_path.display()
        ));
    }

    let candidates = discover_candidates(&absolute_path, runtime_root)?;
    match candidates.len() {
        0 => Err(format!(
            "No valid '{}' candidate found under '{}'.",
            OPENCLAW_CONFIG_NAME,
            absolute_path.display()
        )),
        1 => {
            let candidate = candidates.into_iter().next().expect("one candidate");
            Ok(ResolvedSourceRoot {
                root: candidate.root.clone(),
                config_path: candidate.config_path.clone(),
                config: candidate.config,
                resolved_config_json: candidate.resolved_config_json,
                resolved_config_paths: candidate.resolved_config_paths,
                discovered_candidate_roots: vec![candidate.root],
            })
        }
        _ => {
            let candidates_list = candidates
                .iter()
                .map(|candidate| candidate.root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "Multiple valid '{}' candidates found under '{}': {}. Re-run with an explicit path.",
                OPENCLAW_CONFIG_NAME,
                absolute_path.display(),
                candidates_list
            ))
        }
    }
}

fn discover_candidates(
    search_root: &Path,
    runtime_root: Option<&Path>,
) -> Result<Vec<OpenClawCandidate>, String> {
    let import_root = runtime_root.map(|root| root.join("openclaw"));
    let mut queue = VecDeque::from([search_root.to_path_buf()]);
    let mut visited = BTreeSet::new();
    let mut candidates_by_root = BTreeMap::new();
    let mut invalid_candidate_errors = Vec::new();

    while let Some(dir) = queue.pop_front() {
        let canonical_dir = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !visited.insert(canonical_dir) || should_prune_dir(&dir, import_root.as_deref()) {
            continue;
        }

        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                if dir == search_root {
                    return Err(format!("Failed to read '{}': {err}", dir.display()));
                }
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            if file_type.is_symlink() {
                continue;
            }

            if file_type.is_dir() {
                if !should_prune_dir(&path, import_root.as_deref()) {
                    queue.push_back(path);
                }
                continue;
            }

            if !file_type.is_file() {
                continue;
            }

            if entry.file_name().to_string_lossy() != OPENCLAW_CONFIG_NAME {
                continue;
            }

            match load_candidate(&path) {
                Ok(candidate) => {
                    let key = fs::canonicalize(&candidate.root)
                        .unwrap_or_else(|_| candidate.root.clone());
                    candidates_by_root.entry(key).or_insert(candidate);
                }
                Err(err) => invalid_candidate_errors.push(err),
            }
        }
    }

    if candidates_by_root.is_empty() && !invalid_candidate_errors.is_empty() {
        invalid_candidate_errors.sort();
        invalid_candidate_errors.dedup();
        return Err(format!(
            "Discovered '{}' candidates under '{}', but all failed validation: {}",
            OPENCLAW_CONFIG_NAME,
            search_root.display(),
            invalid_candidate_errors.join(" | ")
        ));
    }

    Ok(candidates_by_root.into_values().collect())
}

fn should_prune_dir(path: &Path, import_root: Option<&Path>) -> bool {
    if let Some(import_root) = import_root {
        if path.starts_with(import_root) {
            return true;
        }
    }

    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| PRUNE_DIR_NAMES.iter().any(|candidate| candidate == &name))
        .unwrap_or(false)
}

fn deep_merge_json(target: serde_json::Value, source: serde_json::Value) -> serde_json::Value {
    match (target, source) {
        (serde_json::Value::Array(mut left), serde_json::Value::Array(right)) => {
            left.extend(right);
            serde_json::Value::Array(left)
        }
        (serde_json::Value::Object(mut left), serde_json::Value::Object(right)) => {
            for (key, value) in right {
                match left.remove(&key) {
                    Some(existing) => {
                        left.insert(key, deep_merge_json(existing, value));
                    }
                    None => {
                        left.insert(key, value);
                    }
                }
            }
            serde_json::Value::Object(left)
        }
        (_, source) => source,
    }
}

fn resolve_includes(
    value: serde_json::Value,
    current_path: &Path,
    root_dir: &Path,
    include_paths: &mut BTreeSet<PathBuf>,
    visited: &mut Vec<PathBuf>,
    depth: usize,
) -> Result<serde_json::Value, String> {
    if depth > MAX_INCLUDE_DEPTH {
        return Err(format!(
            "Maximum include depth ({MAX_INCLUDE_DEPTH}) exceeded while resolving '{}'.",
            current_path.display()
        ));
    }

    match value {
        serde_json::Value::Array(values) => Ok(serde_json::Value::Array(
            values
                .into_iter()
                .map(|entry| {
                    resolve_includes(entry, current_path, root_dir, include_paths, visited, depth)
                })
                .collect::<Result<Vec<_>, _>>()?,
        )),
        serde_json::Value::Object(mut object) => {
            if let Some(include_value) = object.remove(INCLUDE_KEY) {
                let included = resolve_include_value(
                    include_value,
                    current_path,
                    root_dir,
                    include_paths,
                    visited,
                    depth + 1,
                )?;
                if object.is_empty() {
                    return Ok(included);
                }
                let rest = resolve_includes(
                    serde_json::Value::Object(object),
                    current_path,
                    root_dir,
                    include_paths,
                    visited,
                    depth,
                )?;
                if !included.is_object() {
                    return Err(format!(
                        "Config include at '{}' must resolve to an object when sibling keys are present.",
                        current_path.display()
                    ));
                }
                return Ok(deep_merge_json(included, rest));
            }

            let mut resolved = serde_json::Map::new();
            for (key, value) in object {
                resolved.insert(
                    key,
                    resolve_includes(value, current_path, root_dir, include_paths, visited, depth)?,
                );
            }
            Ok(serde_json::Value::Object(resolved))
        }
        other => Ok(other),
    }
}

fn resolve_include_value(
    include_value: serde_json::Value,
    current_path: &Path,
    root_dir: &Path,
    include_paths: &mut BTreeSet<PathBuf>,
    visited: &mut Vec<PathBuf>,
    depth: usize,
) -> Result<serde_json::Value, String> {
    match include_value {
        serde_json::Value::String(path) => {
            load_include_path(&path, current_path, root_dir, include_paths, visited, depth)
        }
        serde_json::Value::Array(paths) => {
            let mut merged = serde_json::Value::Object(serde_json::Map::new());
            for path in paths {
                let serde_json::Value::String(path) = path else {
                    return Err(format!(
                        "Invalid $include array item in '{}': expected string path.",
                        current_path.display()
                    ));
                };
                let loaded = load_include_path(
                    &path,
                    current_path,
                    root_dir,
                    include_paths,
                    visited,
                    depth,
                )?;
                merged = deep_merge_json(merged, loaded);
            }
            Ok(merged)
        }
        other => Err(format!(
            "Invalid $include value in '{}': expected string or array of strings, got {}.",
            current_path.display(),
            json_type_name(&other)
        )),
    }
}

fn load_include_path(
    include_path: &str,
    current_path: &Path,
    root_dir: &Path,
    include_paths: &mut BTreeSet<PathBuf>,
    visited: &mut Vec<PathBuf>,
    depth: usize,
) -> Result<serde_json::Value, String> {
    let current_dir = current_path.parent().ok_or_else(|| {
        format!(
            "Failed to resolve parent directory for include path '{}'.",
            current_path.display()
        )
    })?;
    let resolved = if Path::new(include_path).is_absolute() {
        PathBuf::from(include_path)
    } else {
        current_dir.join(include_path)
    };
    let canonical = fs::canonicalize(&resolved).map_err(|e| {
        format!(
            "Failed to resolve include '{}' from '{}': {e}",
            include_path,
            current_path.display()
        )
    })?;
    let canonical_root = fs::canonicalize(root_dir).unwrap_or_else(|_| root_dir.to_path_buf());
    if !canonical.starts_with(&canonical_root) {
        return Err(format!(
            "Include path '{}' escapes config directory '{}'.",
            include_path,
            root_dir.display()
        ));
    }
    if visited.contains(&canonical) {
        let chain = visited
            .iter()
            .chain(std::iter::once(&canonical))
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(" -> ");
        return Err(format!("Circular include detected: {chain}."));
    }

    let raw = fs::read_to_string(&canonical).map_err(|e| {
        format!(
            "Failed to read include '{}' (resolved '{}'): {e}",
            include_path,
            canonical.display()
        )
    })?;
    let parsed = json5::from_str::<serde_json::Value>(&raw).map_err(|e| {
        format!(
            "Failed to parse include '{}' (resolved '{}'): {e}",
            include_path,
            canonical.display()
        )
    })?;
    include_paths.insert(canonical.clone());
    visited.push(canonical.clone());
    let resolved = resolve_includes(parsed, &canonical, root_dir, include_paths, visited, depth)?;
    visited.pop();
    Ok(resolved)
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn load_candidate(config_path: &Path) -> Result<OpenClawCandidate, String> {
    let content = fs::read_to_string(config_path)
        .map_err(|e| format!("Failed to read '{}': {e}", config_path.display()))?;
    let parsed = json5::from_str::<serde_json::Value>(&content)
        .map_err(|e| format!("Failed to parse '{}': {e}", config_path.display()))?;
    let root = config_path
        .parent()
        .ok_or_else(|| format!("Failed to resolve parent for '{}'.", config_path.display()))?
        .to_path_buf();
    let canonical_config_path =
        fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    let mut include_paths = BTreeSet::from([canonical_config_path.clone()]);
    let mut visited = vec![canonical_config_path];
    let resolved = resolve_includes(
        parsed,
        config_path,
        &root,
        &mut include_paths,
        &mut visited,
        0,
    )?;
    let resolved_config_json = serde_json::to_string_pretty(&resolved).map_err(|e| {
        format!(
            "Failed to serialize resolved '{}': {e}",
            config_path.display()
        )
    })?;
    let config: OpenClawConfig = serde_json::from_value(resolved)
        .map_err(|e| format!("Failed to decode resolved '{}': {e}", config_path.display()))?;

    if config.agents.list.is_empty() {
        return Err(format!(
            "'{}' is not a valid OpenClaw root: agents.list is empty.",
            config_path.display()
        ));
    }

    let mut agent_ids = BTreeSet::new();
    for agent in &config.agents.list {
        let trimmed = agent.id.trim();
        if trimmed.is_empty() {
            return Err(format!(
                "'{}' contains an agent with an empty id.",
                config_path.display()
            ));
        }
        if !agent_ids.insert(trimmed.to_string()) {
            return Err(format!(
                "'{}' contains duplicate agent id '{}'.",
                config_path.display(),
                trimmed
            ));
        }
    }

    if !root.join("agents").is_dir() {
        return Err(format!(
            "'{}' is not a valid OpenClaw root: missing required agents/ directory under '{}'.",
            config_path.display(),
            root.display()
        ));
    }

    config.agents.default_agent_id()?;

    Ok(OpenClawCandidate {
        root,
        config_path: config_path.to_path_buf(),
        config,
        resolved_config_json,
        resolved_config_paths: include_paths.into_iter().collect(),
    })
}

fn absolutize_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}
