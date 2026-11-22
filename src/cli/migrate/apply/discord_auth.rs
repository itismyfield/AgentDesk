use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::services::agent_protocol::DEFAULT_ALLOWED_TOOLS;
use crate::utils::discord::discord_token_hash;

use super::super::plan::ImportPlan;
use super::super::source::{OpenClawBindingConfig, OpenClawDiscordGuildConfig, ResolvedSourceRoot};
use super::super::{DiscordTokenMode, OpenClawMigrateArgs, ToolPolicyMode};
use super::secrets::resolve_account_token;
use super::sessions::parse_u64_str;
use super::{
    BotSettingsEntryPlan, DiscordAccountReportEntry, DiscordAccountToBotMapping,
    DiscordAuthReportData,
};

pub(super) fn planned_bot_account_ids(plan: &ImportPlan) -> BTreeSet<String> {
    let importable_agents = plan
        .agents
        .iter()
        .filter(|agent| agent.eligible_for_v1)
        .map(|agent| agent.source_id.as_str())
        .collect::<BTreeSet<_>>();
    plan.discord
        .bindings
        .iter()
        .filter(|binding| importable_agents.contains(binding.agent_id.as_str()))
        .filter_map(|binding| binding.selected_account_id.clone())
        .collect()
}

#[derive(Default)]
struct DiscordGuildStats {
    guild_count: usize,
    channel_override_count: usize,
    user_allowlist_count: usize,
    role_allowlist_count: usize,
}

fn collect_discord_guild_stats(
    guilds: &BTreeMap<String, OpenClawDiscordGuildConfig>,
) -> DiscordGuildStats {
    let mut stats = DiscordGuildStats::default();
    for guild in guilds.values() {
        stats.guild_count += 1;
        stats.channel_override_count += guild.channels.len();
        stats.user_allowlist_count += guild.users.len();
        stats.role_allowlist_count += guild.roles.len();
        for channel in guild.channels.values() {
            stats.user_allowlist_count += channel.users.len();
            stats.role_allowlist_count += channel.roles.len();
        }
    }
    stats
}

fn allow_bots_enabled(value: Option<&serde_json::Value>) -> bool {
    match value {
        Some(serde_json::Value::Bool(value)) => *value,
        Some(serde_json::Value::Number(value)) => value.as_u64().unwrap_or_default() > 0,
        Some(serde_json::Value::String(value)) => {
            let normalized = value.trim().to_ascii_lowercase();
            !normalized.is_empty()
                && normalized != "false"
                && normalized != "off"
                && normalized != "disabled"
                && normalized != "no"
        }
        _ => false,
    }
}

fn binding_targets_account(
    binding: &OpenClawBindingConfig,
    account_id: &str,
    plan: &ImportPlan,
) -> bool {
    if !binding.r#match.channel.eq_ignore_ascii_case("discord") {
        return false;
    }
    if binding.r#match.account_id.as_deref() == Some(account_id) {
        return true;
    }
    if binding.r#match.account_id.is_some() {
        return false;
    }
    plan.discord.bindings.iter().any(|planned| {
        planned.agent_id == binding.agent_id
            && planned.requested_account_id.is_none()
            && planned.selected_account_id.as_deref() == Some(account_id)
    })
}

fn build_discord_account_mappings(plan: &ImportPlan) -> Vec<DiscordAccountToBotMapping> {
    #[derive(Default)]
    struct MappingAccumulator {
        role_ids: BTreeSet<String>,
        providers: BTreeSet<String>,
        live_channel_ids: BTreeSet<String>,
        live_binding_agents: BTreeSet<String>,
        preview_only_binding_agents: BTreeSet<String>,
    }

    let agent_lookup = plan
        .agents
        .iter()
        .filter(|agent| agent.eligible_for_v1)
        .map(|agent| (agent.source_id.as_str(), agent))
        .collect::<BTreeMap<_, _>>();
    let mut mappings = BTreeMap::<String, MappingAccumulator>::new();

    for binding in &plan.discord.bindings {
        let Some(account_id) = binding.selected_account_id.as_deref() else {
            continue;
        };
        let Some(agent) = agent_lookup.get(binding.agent_id.as_str()) else {
            continue;
        };
        let entry = mappings.entry(account_id.to_string()).or_default();
        entry.role_ids.insert(agent.final_role_id.clone());
        if let Some(provider) = agent.mapped_provider.as_ref() {
            entry.providers.insert(provider.clone());
        }
        if binding.mode == "live_applicable" {
            entry.live_binding_agents.insert(agent.source_id.clone());
            entry
                .live_channel_ids
                .extend(binding.channel_ids.iter().cloned());
        } else {
            entry
                .preview_only_binding_agents
                .insert(agent.source_id.clone());
        }
    }

    mappings
        .into_iter()
        .map(|(account_id, entry)| {
            let mode = if entry.live_channel_ids.is_empty() {
                "preview_only"
            } else {
                "live_applicable"
            };
            DiscordAccountToBotMapping {
                account_id,
                role_ids: entry.role_ids.into_iter().collect(),
                providers: entry.providers.into_iter().collect(),
                live_channel_ids: entry.live_channel_ids.into_iter().collect(),
                live_binding_agents: entry.live_binding_agents.into_iter().collect(),
                preview_only_binding_agents: entry
                    .preview_only_binding_agents
                    .into_iter()
                    .collect(),
                mode,
            }
        })
        .collect()
}

fn classify_discord_token_status(
    source: &ResolvedSourceRoot,
    account_id: &str,
    has_token: bool,
    token_kind_name: &str,
    token_mode: DiscordTokenMode,
    write_bot_settings: bool,
    planned_bot_accounts: &BTreeSet<String>,
) -> String {
    if !has_token {
        return "missing".to_string();
    }
    if !write_bot_settings || !planned_bot_accounts.contains(account_id) {
        return "skipped".to_string();
    }
    if token_mode == DiscordTokenMode::Report {
        return "skipped".to_string();
    }
    if token_mode == DiscordTokenMode::PlaintextOnly && token_kind_name != "plaintext" {
        return "skipped".to_string();
    }
    if token_mode == DiscordTokenMode::ResolveEnvFile && token_kind_name == "exec" {
        return "skipped".to_string();
    }
    match resolve_account_token(source, account_id, token_mode) {
        Ok(Some(_)) => "imported".to_string(),
        Ok(None) => "missing".to_string(),
        Err(_) => "could_not_be_resolved".to_string(),
    }
}

pub(super) fn build_discord_auth_report(
    plan: &ImportPlan,
    source: &ResolvedSourceRoot,
    args: &OpenClawMigrateArgs,
) -> Result<DiscordAuthReportData, String> {
    let token_mode = DiscordTokenMode::parse(&args.discord_token_mode)?;
    let planned_bot_accounts = planned_bot_account_ids(plan);
    let mut accounts = Vec::new();

    if let Some(discord) = source.config.channels.discord.as_ref() {
        for account in &plan.discord.accounts {
            let (guilds, allow_bots) = if account.account_id == "default" {
                (&discord.guilds, discord.allow_bots.as_ref())
            } else {
                let account_config = discord.accounts.get(&account.account_id);
                (
                    account_config
                        .map(|value| &value.guilds)
                        .unwrap_or(&discord.guilds),
                    account_config.and_then(|value| value.allow_bots.as_ref()),
                )
            };
            let stats = collect_discord_guild_stats(guilds);
            let binding_roles_present = source.config.bindings.iter().any(|binding| {
                !binding.r#match.roles.is_empty()
                    && binding_targets_account(binding, &account.account_id, plan)
            });
            accounts.push(DiscordAccountReportEntry {
                account_id: account.account_id.clone(),
                source: account.source,
                enabled: account.enabled,
                has_token: account.has_token,
                token_kind: account.token_kind.clone(),
                token_status: classify_discord_token_status(
                    source,
                    &account.account_id,
                    account.has_token,
                    &account.token_kind,
                    token_mode,
                    args.write_bot_settings,
                    &planned_bot_accounts,
                ),
                importable: account.importable,
                guild_count: stats.guild_count,
                channel_override_count: stats.channel_override_count,
                user_allowlist_count: stats.user_allowlist_count,
                role_allowlist_count: stats.role_allowlist_count,
                binding_roles_present,
                allow_bots_enabled: allow_bots_enabled(allow_bots),
            });
        }
    }

    Ok(DiscordAuthReportData {
        default_token_configured: source
            .config
            .channels
            .discord
            .as_ref()
            .and_then(|discord| discord.token.as_ref())
            .is_some(),
        has_named_accounts: source
            .config
            .channels
            .discord
            .as_ref()
            .map(|discord| !discord.accounts.is_empty())
            .unwrap_or(false),
        requested_token_mode: args.discord_token_mode.clone(),
        write_bot_settings_enabled: args.write_bot_settings,
        accounts,
        account_to_bot_mappings: build_discord_account_mappings(plan),
    })
}

pub(super) fn build_bot_settings_entry_plans(
    plan: &ImportPlan,
    source: &ResolvedSourceRoot,
    args: &OpenClawMigrateArgs,
    warnings: &mut Vec<String>,
) -> Result<Vec<BotSettingsEntryPlan>, String> {
    let token_mode = DiscordTokenMode::parse(&args.discord_token_mode)?;
    let tool_policy_mode = ToolPolicyMode::parse(&args.tool_policy_mode)?;

    if token_mode == DiscordTokenMode::Report {
        warnings.push(
            "Discord token mode is report-only, so bot_settings.json live import is skipped."
                .to_string(),
        );
        return Ok(Vec::new());
    }

    let agent_lookup = plan
        .agents
        .iter()
        .map(|agent| (agent.source_id.as_str(), agent))
        .collect::<BTreeMap<_, _>>();

    #[derive(Default)]
    struct BotAccountAccumulator {
        role_ids: BTreeSet<String>,
        providers: BTreeSet<String>,
        channel_ids: BTreeSet<u64>,
        source_agent_ids: BTreeSet<String>,
    }

    let mut by_account = BTreeMap::<String, BotAccountAccumulator>::new();
    for binding in &plan.discord.bindings {
        if binding.mode != "live_applicable" {
            continue;
        }
        let Some(account_id) = binding.selected_account_id.as_deref() else {
            continue;
        };
        let Some(agent) = agent_lookup.get(binding.agent_id.as_str()) else {
            continue;
        };
        if !agent.eligible_for_v1 {
            continue;
        }
        let entry = by_account.entry(account_id.to_string()).or_default();
        entry.role_ids.insert(agent.final_role_id.clone());
        entry.source_agent_ids.insert(agent.source_id.clone());
        if let Some(provider) = agent.mapped_provider.as_ref() {
            entry.providers.insert(provider.clone());
        }
        if args.with_channel_bindings && args.write_org && binding.mode == "live_applicable" {
            for channel_id in &binding.channel_ids {
                if let Some(parsed) = parse_u64_str(channel_id) {
                    entry.channel_ids.insert(parsed);
                }
            }
        }
    }

    let mut entries = Vec::new();
    for (account_id, accumulator) in by_account {
        let Some(token) = resolve_account_token(source, &account_id, token_mode)? else {
            warnings.push(format!(
                "Skipping bot_settings import for Discord account '{}': token could not be resolved under mode '{}'.",
                account_id,
                token_mode.as_str()
            ));
            continue;
        };
        if accumulator.providers.len() > 1 {
            warnings.push(format!(
                "Skipping bot_settings import for Discord account '{}': multiple providers map to one token ({:?}).",
                account_id,
                accumulator.providers
            ));
            continue;
        }
        let provider = accumulator
            .providers
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| "claude".to_string());
        if tool_policy_mode == ToolPolicyMode::BotIntersection
            && plan
                .tool_policy
                .agents
                .iter()
                .filter(|scan| accumulator.source_agent_ids.contains(&scan.agent_id))
                .any(|scan| scan.has_sender_scoped_policy)
        {
            warnings.push(format!(
                "Skipping bot_settings tool auto-apply for Discord account '{}': bot-intersection cannot preserve sender-scoped OpenClaw tool policy.",
                account_id
            ));
            continue;
        }
        let role_id = if accumulator.role_ids.len() == 1 {
            accumulator.role_ids.iter().next().cloned()
        } else {
            None
        };
        let allowed_tools = build_bot_allowed_tools(
            plan,
            &accumulator.source_agent_ids,
            &provider,
            tool_policy_mode,
            warnings,
        );
        let allowed_channel_ids = if args.with_channel_bindings
            && args.write_org
            && !accumulator.channel_ids.is_empty()
        {
            Some(accumulator.channel_ids.into_iter().collect())
        } else {
            None
        };
        if accumulator.role_ids.len() > 1 && allowed_channel_ids.is_none() {
            warnings.push(format!(
                "Skipping bot_settings import for Discord account '{}': multiple imported agents share the same token, but no live channel allowlist could be written to keep routing scoped.",
                account_id
            ));
            continue;
        }
        entries.push(BotSettingsEntryPlan {
            provider,
            role_id,
            token,
            allowed_channel_ids,
            allowed_tools,
        });
    }

    Ok(entries)
}

fn build_bot_allowed_tools(
    plan: &ImportPlan,
    source_agent_ids: &BTreeSet<String>,
    provider: &str,
    mode: ToolPolicyMode,
    warnings: &mut Vec<String>,
) -> Option<Vec<String>> {
    if mode == ToolPolicyMode::Report {
        return None;
    }

    let mut sets = Vec::new();
    for agent in plan
        .tool_policy
        .agents
        .iter()
        .filter(|agent| source_agent_ids.contains(&agent.agent_id))
    {
        let tools = agent.normalized_candidate_tools.clone();
        if !tools.is_empty() {
            sets.push(tools);
        }
    }

    if sets.is_empty() {
        warnings.push(format!(
            "No explicit OpenClaw allowlist was recognized for provider '{}'; using AgentDesk default allowed tools.",
            provider
        ));
        return Some(
            DEFAULT_ALLOWED_TOOLS
                .iter()
                .map(|tool| (*tool).to_string())
                .collect(),
        );
    }

    let combined = match mode {
        ToolPolicyMode::BotUnion => {
            let mut union = BTreeSet::new();
            for set in sets {
                union.extend(set);
            }
            union.into_iter().collect::<Vec<_>>()
        }
        ToolPolicyMode::BotIntersection => {
            let mut iter = sets.into_iter();
            let Some(first) = iter.next() else {
                return Some(
                    DEFAULT_ALLOWED_TOOLS
                        .iter()
                        .map(|tool| (*tool).to_string())
                        .collect(),
                );
            };
            let mut intersection = first.into_iter().collect::<BTreeSet<_>>();
            for set in iter {
                let current = set.into_iter().collect::<BTreeSet<_>>();
                intersection = intersection.intersection(&current).cloned().collect();
            }
            if intersection.is_empty() {
                warnings.push(
                    "Tool-policy intersection resolved to an empty allowlist; using AgentDesk default allowed tools."
                        .to_string(),
                );
                DEFAULT_ALLOWED_TOOLS
                    .iter()
                    .map(|tool| (*tool).to_string())
                    .collect()
            } else {
                intersection.into_iter().collect()
            }
        }
        ToolPolicyMode::Report => unreachable!(),
    };

    Some(combined)
}

pub(super) fn render_bot_settings_json(
    path: &Path,
    entries: &[BotSettingsEntryPlan],
    _overwrite: bool,
) -> Result<String, String> {
    let mut root = if path.exists() {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
        serde_json::from_str::<serde_json::Value>(&content)
            .map_err(|e| format!("Failed to parse '{}': {e}", path.display()))?
    } else {
        serde_json::json!({})
    };
    let object = root
        .as_object_mut()
        .ok_or_else(|| format!("'{}' root must be a JSON object.", path.display()))?;
    for entry in entries {
        let key = discord_token_hash(&entry.token);
        let mut entry_json = object
            .get(&key)
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        entry_json.insert("token".to_string(), serde_json::json!(entry.token));
        entry_json.insert("provider".to_string(), serde_json::json!(entry.provider));
        entry_json.insert("agent".to_string(), serde_json::json!(entry.role_id));
        if let Some(allowed_channel_ids) = entry.allowed_channel_ids.as_ref() {
            entry_json.insert(
                "allowed_channel_ids".to_string(),
                serde_json::json!(allowed_channel_ids),
            );
        }
        if let Some(allowed_tools) = entry.allowed_tools.as_ref() {
            entry_json.insert(
                "allowed_tools".to_string(),
                serde_json::json!(allowed_tools),
            );
        }
        object.insert(key, serde_json::Value::Object(entry_json));
    }
    serde_json::to_string_pretty(&root)
        .map_err(|e| format!("Failed to serialize '{}': {e}", path.display()))
}
