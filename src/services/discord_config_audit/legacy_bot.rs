//! Parsing and field-by-field migration of one legacy bot-settings entry.

use super::*;

pub(super) fn parse_legacy_bot_entry(entry_key: &str, entry_value: &Value) -> LegacyBotEntry {
    let token = entry_value
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let hash_key = token
        .as_deref()
        .map(discord_token_hash)
        .unwrap_or_else(|| entry_key.to_string());

    LegacyBotEntry {
        hash_key,
        token,
        provider: entry_value
            .get("provider")
            .and_then(Value::as_str)
            .and_then(|raw| normalize_provider(Some(raw))),
        agent: entry_value
            .get("agent")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
        allowed_tools: entry_value
            .get("allowed_tools")
            .map(parse_string_array)
            .map(|values| normalize_allowed_tools(values.unwrap_or_default())),
        allowed_channel_ids: entry_value
            .get("allowed_channel_ids")
            .map(parse_u64_array)
            .unwrap_or(None),
        owner_user_id: entry_value.get("owner_user_id").and_then(parse_u64_value),
        allowed_user_ids: entry_value
            .get("allowed_user_ids")
            .map(parse_u64_array)
            .unwrap_or(None),
        allow_all_users: entry_value.get("allow_all_users").and_then(Value::as_bool),
        allowed_bot_ids: entry_value
            .get("allowed_bot_ids")
            .map(parse_u64_array)
            .unwrap_or(None),
        channel_model_overrides: entry_value
            .get("channel_model_overrides")
            .and_then(Value::as_object)
            .map(|obj| {
                obj.iter()
                    .filter_map(|(channel_id, model)| {
                        model
                            .as_str()
                            .map(|model| (channel_id.clone(), model.to_string()))
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default(),
        channel_fast_modes: entry_value
            .get("channel_fast_modes")
            .and_then(Value::as_object)
            .map(|obj| {
                obj.iter()
                    .filter_map(|(channel_id, enabled)| {
                        enabled
                            .as_bool()
                            .map(|enabled| (channel_id.clone(), enabled))
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default(),
        channel_fast_mode_reset_pending: entry_value
            .get("channel_fast_mode_reset_pending")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default(),
        channel_codex_goals: entry_value
            .get("channel_codex_goals")
            .and_then(Value::as_object)
            .map(|obj| {
                obj.iter()
                    .filter_map(|(channel_id, enabled)| {
                        enabled
                            .as_bool()
                            .map(|enabled| (channel_id.clone(), enabled))
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default(),
        channel_codex_goals_reset_pending: entry_value
            .get("channel_codex_goals_reset_pending")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default(),
    }
}

pub(super) fn migrate_provider(
    legacy: &LegacyBotEntry,
    bot: &mut BotConfig,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> bool {
    let Some(legacy_provider) = legacy.provider.as_ref() else {
        return false;
    };
    match bot
        .provider
        .as_deref()
        .and_then(|raw| normalize_provider(Some(raw)))
    {
        None => {
            report.actions.push(format!(
                "{} provider={} for bot '{}'",
                dry_run_action_prefix(dry_run),
                legacy_provider,
                legacy.hash_key
            ));
            if !dry_run {
                bot.provider = Some(legacy_provider.clone());
            }
            true
        }
        Some(existing) if existing == *legacy_provider => false,
        Some(existing) => {
            report.warnings.push(format!(
                "Legacy bot_settings entry '{}' provider={} conflicts with agentdesk.yaml provider={}; agentdesk.yaml wins",
                legacy.hash_key, legacy_provider, existing
            ));
            false
        }
    }
}

pub(super) fn migrate_agent(
    legacy: &LegacyBotEntry,
    bot: &mut BotConfig,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> bool {
    let Some(legacy_agent) = legacy.agent.as_ref() else {
        return false;
    };
    match bot
        .agent
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        None => {
            report.actions.push(format!(
                "{} agent={} for bot '{}'",
                dry_run_action_prefix(dry_run),
                legacy_agent,
                legacy.hash_key
            ));
            if !dry_run {
                bot.agent = Some(legacy_agent.clone());
            }
            true
        }
        Some(existing) if existing == legacy_agent => false,
        Some(existing) => {
            report.warnings.push(format!(
                "Legacy bot_settings entry '{}' agent={} conflicts with agentdesk.yaml agent={}; agentdesk.yaml wins",
                legacy.hash_key, legacy_agent, existing
            ));
            false
        }
    }
}

pub(super) fn migrate_tool_list(
    hash_key: &str,
    legacy_value: &Option<Vec<String>>,
    yaml_value: &mut Option<Vec<String>>,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> bool {
    let Some(legacy_tools) = legacy_value.as_ref() else {
        return false;
    };
    match yaml_value {
        None => {
            report.actions.push(format!(
                "{} allowed_tools for bot '{}'",
                dry_run_action_prefix(dry_run),
                hash_key
            ));
            if !dry_run {
                *yaml_value = Some(legacy_tools.clone());
            }
            true
        }
        Some(existing) if *existing == *legacy_tools => false,
        Some(existing) => {
            report.warnings.push(format!(
                "Legacy bot_settings entry '{}' allowed_tools={:?} conflicts with agentdesk.yaml allowed_tools={:?}; agentdesk.yaml wins",
                hash_key, legacy_tools, existing
            ));
            false
        }
    }
}

pub(super) fn migrate_u64_list(
    hash_key: &str,
    field: &str,
    legacy_value: &Option<Vec<u64>>,
    yaml_value: &mut Option<Vec<u64>>,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> bool {
    let Some(legacy_ids) = legacy_value.as_ref() else {
        return false;
    };
    match yaml_value {
        None => {
            report.actions.push(format!(
                "{} {} for bot '{}'",
                dry_run_action_prefix(dry_run),
                field,
                hash_key
            ));
            if !dry_run {
                *yaml_value = Some(legacy_ids.clone());
            }
            true
        }
        Some(existing) if normalized_u64s(existing) == normalized_u64s(legacy_ids) => false,
        Some(existing) => {
            report.warnings.push(format!(
                "Legacy bot_settings entry '{}' {}={:?} conflicts with agentdesk.yaml {}={:?}; agentdesk.yaml wins",
                hash_key, field, legacy_ids, field, existing
            ));
            false
        }
    }
}

pub(super) fn migrate_bool_field(
    hash_key: &str,
    field: &str,
    legacy_value: Option<bool>,
    yaml_value: &mut Option<bool>,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> bool {
    let Some(legacy_flag) = legacy_value else {
        return false;
    };
    match yaml_value {
        None => {
            report.actions.push(format!(
                "{} {}={} for bot '{}'",
                dry_run_action_prefix(dry_run),
                field,
                legacy_flag,
                hash_key
            ));
            if !dry_run {
                *yaml_value = Some(legacy_flag);
            }
            true
        }
        Some(existing) if *existing == legacy_flag => false,
        Some(existing) => {
            report.warnings.push(format!(
                "Legacy bot_settings entry '{}' {}={} conflicts with agentdesk.yaml {}={}; agentdesk.yaml wins",
                hash_key, field, legacy_flag, field, existing
            ));
            false
        }
    }
}

pub(super) fn parse_u64_value(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_str()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
    })
}

pub(super) fn parse_u64_array(value: &Value) -> Option<Vec<u64>> {
    value.as_array().map(|values| {
        values
            .iter()
            .filter_map(parse_u64_value)
            .collect::<Vec<_>>()
    })
}

pub(super) fn parse_string_array(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|values| {
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    })
}

pub(super) fn normalized_u64s(values: &[u64]) -> Vec<u64> {
    let mut normalized = values.to_vec();
    normalized.sort_unstable();
    normalized.dedup();
    normalized
}
