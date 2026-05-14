use super::*;

#[derive(Debug, Default)]
struct AgentChannelUpdate {
    id: Option<String>,
    name: Option<String>,
    prompt_file: Option<String>,
    workspace: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    peer_agents: Option<bool>,
    dispatch_profile: Option<String>,
}

pub(super) fn merge_role_map_into_agentdesk_yaml(root: &Path) -> Result<(), String> {
    let role_map = role_map_path(root);
    if !role_map.is_file() {
        return Ok(());
    }

    let yaml_path = config_file_path(root);
    let mut config = if yaml_path.is_file() {
        crate::config::load_from_path(&yaml_path)
            .map_err(|e| format!("Failed to load config '{}': {e}", yaml_path.display()))?
    } else {
        crate::config::Config::default()
    };

    let content = fs::read_to_string(&role_map)
        .map_err(|e| format!("Failed to read '{}': {e}", role_map.display()))?;
    let json: Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse '{}': {e}", role_map.display()))?;

    let changed = preview_role_map_merge(&mut config, &json);

    if changed {
        crate::config::save_to_path(&yaml_path, &config)
            .map_err(|e| format!("Failed to write config '{}': {e}", yaml_path.display()))?;
    }

    let migrated = role_map.with_extension("json.migrated");
    if let Err(e) = fs::rename(&role_map, &migrated) {
        tracing::warn!(
            "Failed to rename '{}' -> '{}': {e}",
            role_map.display(),
            migrated.display()
        );
    } else {
        tracing::info!(
            "[role-map] Migrated '{}' -> '{}' (one-time merge into agentdesk.yaml)",
            role_map.display(),
            migrated.display()
        );
    }

    Ok(())
}

pub(crate) fn preview_role_map_merge(config: &mut crate::config::Config, json: &Value) -> bool {
    let mut changed = false;
    changed |= merge_role_map_shared_prompt(config, json);
    changed |= merge_role_map_meeting(config, json);

    let mut providers_by_channel_id = BTreeMap::<String, String>::new();
    if let Some(by_id) = json.get("byChannelId").and_then(Value::as_object) {
        for (channel_id, entry) in by_id {
            if let Some((provider_key, entry_changed)) =
                merge_role_map_channel_id_entry(config, channel_id, entry)
            {
                providers_by_channel_id.insert(channel_id.clone(), provider_key);
                changed |= entry_changed;
            }
        }
    }
    if let Some(by_name) = json.get("byChannelName").and_then(Value::as_object) {
        for (channel_name, entry) in by_name {
            if merge_role_map_channel_name_entry(
                config,
                channel_name,
                entry,
                &providers_by_channel_id,
            ) {
                changed = true;
            }
        }
    }

    changed
}

fn merge_role_map_shared_prompt(config: &mut crate::config::Config, json: &Value) -> bool {
    if config.shared_prompt.is_some() {
        return false;
    }
    let Some(shared_prompt) = json_string_field(json, &["sharedPromptFile", "shared_prompt"])
    else {
        return false;
    };
    config.shared_prompt = Some(shared_prompt);
    true
}

fn merge_role_map_meeting(config: &mut crate::config::Config, json: &Value) -> bool {
    if config.meeting.is_some() {
        return false;
    }
    let Some(meeting) = json.get("meeting").and_then(role_map_meeting_to_config) else {
        return false;
    };
    config.meeting = Some(meeting);
    true
}

fn role_map_meeting_to_config(value: &Value) -> Option<crate::config::MeetingSettings> {
    let meeting = value.as_object()?;
    let channel_name = json_string_field_from_map(meeting, &["channel_name"])?;
    let max_rounds = meeting
        .get("max_rounds")
        .and_then(Value::as_u64)
        .map(|value| value as u32);
    let max_participants =
        json_usize_field_from_map(meeting, &["max_participants", "maxParticipants"]);
    let summary_agent = meeting
        .get("summary_agent")
        .and_then(role_map_summary_agent_to_config);
    let available_agents = meeting
        .get("available_agents")
        .and_then(Value::as_array)
        .map(|agents| {
            agents
                .iter()
                .filter_map(role_map_meeting_agent_to_config)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Some(crate::config::MeetingSettings {
        channel_name,
        max_rounds,
        max_participants,
        summary_agent,
        available_agents,
    })
}

fn role_map_summary_agent_to_config(
    value: &Value,
) -> Option<crate::config::MeetingSummaryAgentDef> {
    if let Some(agent) = value.as_str().and_then(normalize_non_empty) {
        return Some(crate::config::MeetingSummaryAgentDef::Static(agent));
    }

    let obj = value.as_object()?;
    let default = json_string_field_from_map(obj, &["default"])?;
    let rules = obj
        .get("rules")
        .and_then(Value::as_array)
        .map(|rules| {
            rules
                .iter()
                .filter_map(|rule| {
                    let rule_obj = rule.as_object()?;
                    let agent = json_string_field_from_map(rule_obj, &["agent"])?;
                    let keywords = rule_obj
                        .get("keywords")
                        .and_then(Value::as_array)
                        .map(|keywords| {
                            keywords
                                .iter()
                                .filter_map(Value::as_str)
                                .filter_map(normalize_non_empty)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    Some(crate::config::MeetingSummaryRuleDef { keywords, agent })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Some(crate::config::MeetingSummaryAgentDef::Dynamic { rules, default })
}

fn role_map_meeting_agent_to_config(value: &Value) -> Option<crate::config::MeetingAgentEntry> {
    let obj = value.as_object()?;
    let role_id = json_string_field_from_map(obj, &["role_id", "roleId"])?;
    let display_name = json_string_field_from_map(obj, &["display_name", "displayName"]);
    let prompt_file = json_string_field_from_map(obj, &["prompt_file", "promptFile"]);
    let keywords = json_string_list_field_from_map(obj, &["keywords"]);
    let domain_summary = json_string_field_from_map(obj, &["domain_summary", "domainSummary"]);
    let strengths = json_string_list_field_from_map(obj, &["strengths"]);
    let task_types = json_string_list_field_from_map(obj, &["task_types", "taskTypes"]);
    let anti_signals = json_string_list_field_from_map(obj, &["anti_signals", "antiSignals"]);
    let provider_hint = json_string_field_from_map(obj, &["provider_hint", "providerHint"]);

    Some(crate::config::MeetingAgentEntry::Detailed(
        crate::config::MeetingAgentDef {
            role_id,
            display_name,
            keywords,
            prompt_file,
            domain_summary,
            strengths,
            task_types,
            anti_signals,
            provider_hint,
        },
    ))
}

fn merge_role_map_channel_id_entry(
    config: &mut crate::config::Config,
    channel_id: &str,
    entry: &Value,
) -> Option<(String, bool)> {
    let obj = entry.as_object()?;
    let role_id = json_string_field_from_map(obj, &["roleId", "role_id"])?;
    let provider_key = json_string_field_from_map(obj, &["provider"])
        .as_deref()
        .and_then(normalize_provider_name)
        .or_else(|| infer_provider_for_role(config, &role_id, Some(channel_id), None))
        .unwrap_or_else(|| "claude".to_string());

    let (agent_index, agent_changed) = ensure_config_agent(config, &role_id, &provider_key);
    let update = AgentChannelUpdate {
        id: normalize_non_empty(channel_id),
        name: json_string_field_from_map(obj, &["channelName", "channel_name"]),
        prompt_file: json_string_field_from_map(obj, &["promptFile", "prompt_file"]),
        workspace: json_string_field_from_map(obj, &["workspace"]),
        provider: Some(provider_key.clone()),
        model: json_string_field_from_map(obj, &["model"]),
        reasoning_effort: json_string_field_from_map(obj, &["reasoningEffort", "reasoning_effort"]),
        peer_agents: json_bool_field_from_map(obj, &["peerAgents", "peer_agents"]),
        dispatch_profile: json_string_field_from_map(obj, &["dispatchProfile", "dispatch_profile"]),
    };

    let agent = &mut config.agents[agent_index];
    let slot = channel_slot_mut(&mut agent.channels, &provider_key)?;
    let channel_changed = apply_channel_update(slot, update, None);
    Some((provider_key, agent_changed || channel_changed))
}

fn merge_role_map_channel_name_entry(
    config: &mut crate::config::Config,
    channel_name: &str,
    entry: &Value,
    providers_by_channel_id: &BTreeMap<String, String>,
) -> bool {
    let Some(obj) = entry.as_object() else {
        return false;
    };
    let Some(role_id) = json_string_field_from_map(obj, &["roleId", "role_id"]) else {
        return false;
    };
    let channel_id = json_string_field_from_map(obj, &["channelId", "channel_id"]);
    let provider_key = json_string_field_from_map(obj, &["provider"])
        .as_deref()
        .and_then(normalize_provider_name)
        .or_else(|| {
            channel_id
                .as_ref()
                .and_then(|channel_id| providers_by_channel_id.get(channel_id).cloned())
        })
        .or_else(|| {
            infer_provider_for_role(config, &role_id, channel_id.as_deref(), Some(channel_name))
        })
        .unwrap_or_else(|| "claude".to_string());

    let (agent_index, agent_changed) = ensure_config_agent(config, &role_id, &provider_key);
    let update = AgentChannelUpdate {
        id: channel_id,
        name: normalize_non_empty(channel_name),
        prompt_file: json_string_field_from_map(obj, &["promptFile", "prompt_file"]),
        workspace: json_string_field_from_map(obj, &["workspace"]),
        provider: Some(provider_key.clone()),
        model: json_string_field_from_map(obj, &["model"]),
        reasoning_effort: json_string_field_from_map(obj, &["reasoningEffort", "reasoning_effort"]),
        peer_agents: json_bool_field_from_map(obj, &["peerAgents", "peer_agents"]),
        dispatch_profile: json_string_field_from_map(obj, &["dispatchProfile", "dispatch_profile"]),
    };

    let agent = &mut config.agents[agent_index];
    let Some(slot) = channel_slot_mut(&mut agent.channels, &provider_key) else {
        return agent_changed;
    };
    agent_changed || apply_channel_update(slot, update, None)
}

fn ensure_config_agent(
    config: &mut crate::config::Config,
    role_id: &str,
    provider_key: &str,
) -> (usize, bool) {
    if let Some(index) = config.agents.iter().position(|agent| agent.id == role_id) {
        let agent = &mut config.agents[index];
        if normalize_provider_name(&agent.provider).is_none() {
            agent.provider = provider_key.to_string();
            return (index, true);
        }
        return (index, false);
    }

    config.agents.push(crate::config::AgentDef {
        id: role_id.to_string(),
        name: role_id.to_string(),
        name_ko: None,
        aliases: Vec::new(),
        wake_word: None,
        voice_enabled: true,
        sensitivity_mode: None,
        voice: crate::config::AgentVoiceConfig::default(),
        provider: provider_key.to_string(),
        channels: crate::config::AgentChannels::default(),
        keywords: Vec::new(),
        department: None,
        avatar_emoji: None,
    });
    (config.agents.len() - 1, true)
}

fn infer_provider_for_role(
    config: &crate::config::Config,
    role_id: &str,
    channel_id: Option<&str>,
    channel_name: Option<&str>,
) -> Option<String> {
    let agent = config.agents.iter().find(|agent| agent.id == role_id)?;
    for (provider_key, maybe_channel) in agent.channels.iter() {
        let Some(channel) = maybe_channel else {
            continue;
        };
        if let Some(channel_id) = channel_id
            && (channel.channel_id().as_deref() == Some(channel_id)
                || channel.target().as_deref() == Some(channel_id))
        {
            return Some(provider_key.to_string());
        }
        if let Some(channel_name) = channel_name
            && (channel.channel_name().as_deref() == Some(channel_name)
                || channel.aliases().iter().any(|alias| alias == channel_name))
        {
            return Some(provider_key.to_string());
        }
    }
    normalize_provider_name(&agent.provider)
}

fn channel_slot_mut<'a>(
    channels: &'a mut crate::config::AgentChannels,
    provider: &str,
) -> Option<&'a mut Option<crate::config::AgentChannel>> {
    match provider {
        "claude" => Some(&mut channels.claude),
        "codex" => Some(&mut channels.codex),
        "gemini" => Some(&mut channels.gemini),
        "opencode" => Some(&mut channels.opencode),
        "qwen" => Some(&mut channels.qwen),
        _ => None,
    }
}

fn apply_channel_update(
    slot: &mut Option<crate::config::AgentChannel>,
    update: AgentChannelUpdate,
    extra_aliases: Option<Vec<String>>,
) -> bool {
    let current = slot.clone();
    let mut config = match current.clone() {
        Some(crate::config::AgentChannel::Detailed(config)) => config,
        Some(crate::config::AgentChannel::Legacy(raw)) => channel_config_from_legacy(raw),
        None => crate::config::AgentChannelConfig::default(),
    };

    if config.id.is_none() {
        config.id = update.id;
    }
    if let Some(name) = update.name {
        match config.name.as_deref() {
            Some(existing) if existing == name => {}
            Some(_) => push_channel_alias(&mut config, name),
            None => config.name = Some(name),
        }
    }
    if config.prompt_file.is_none() {
        config.prompt_file = update.prompt_file;
    }
    if config.workspace.is_none() {
        config.workspace = update.workspace;
    }
    if config.provider.is_none() {
        config.provider = update.provider;
    }
    if config.model.is_none() {
        config.model = update.model;
    }
    if config.reasoning_effort.is_none() {
        config.reasoning_effort = update.reasoning_effort;
    }
    if config.peer_agents.is_none() {
        config.peer_agents = update.peer_agents;
    }
    let update_dispatch_profile =
        crate::config::normalize_dispatch_profile(update.dispatch_profile);
    if config
        .dispatch_profile
        .as_deref()
        .is_some_and(|value| !crate::config::is_valid_dispatch_profile(value))
    {
        config.dispatch_profile = None;
    }
    if config.dispatch_profile.is_none() {
        config.dispatch_profile = update_dispatch_profile;
    }
    if let Some(extra_aliases) = extra_aliases {
        for alias in extra_aliases {
            push_channel_alias(&mut config, alias);
        }
    }

    let next = Some(crate::config::AgentChannel::Detailed(config));
    if next != current {
        *slot = next;
        true
    } else {
        false
    }
}

fn channel_config_from_legacy(raw: String) -> crate::config::AgentChannelConfig {
    let mut config = crate::config::AgentChannelConfig::default();
    let Some(raw) = normalize_non_empty(&raw) else {
        return config;
    };
    if raw.parse::<u64>().is_ok() {
        config.id = Some(raw);
    } else {
        config.name = Some(raw);
    }
    config
}

fn push_channel_alias(config: &mut crate::config::AgentChannelConfig, alias: String) {
    let Some(alias) = normalize_non_empty(&alias) else {
        return;
    };
    if config.name.as_deref() == Some(alias.as_str()) {
        return;
    }
    if !config.aliases.iter().any(|existing| existing == &alias) {
        config.aliases.push(alias);
        config.aliases.sort();
        config.aliases.dedup();
    }
}

fn normalize_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn json_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    let obj = value.as_object()?;
    json_string_field_from_map(obj, keys)
}

fn json_string_field_from_map(
    obj: &serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<String> {
    keys.iter().find_map(|key| {
        obj.get(*key).and_then(|value| match value {
            Value::String(raw) => normalize_non_empty(raw),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        })
    })
}

fn json_bool_field_from_map(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(Value::as_bool))
}

fn json_usize_field_from_map(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<usize> {
    keys.iter().find_map(|key| {
        obj.get(*key).and_then(|value| match value {
            Value::Number(number) => number
                .as_u64()
                .and_then(|value| usize::try_from(value).ok()),
            Value::String(raw) => normalize_non_empty(raw).and_then(|value| value.parse().ok()),
            _ => None,
        })
    })
}

fn json_string_list_field_from_map(
    obj: &serde_json::Map<String, Value>,
    keys: &[&str],
) -> Vec<String> {
    keys.iter()
        .find_map(|key| {
            obj.get(*key).and_then(Value::as_array).map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(normalize_non_empty)
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default()
}

pub(super) fn update_role_map_prompt_paths(root: &Path) -> Result<(), String> {
    let path = role_map_path(root);
    if !path.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    let mut json: Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse '{}': {e}", path.display()))?;
    rewrite_prompt_paths_json(&mut json, root);
    let rendered = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Failed to serialize '{}': {e}", path.display()))?;
    fs::write(&path, rendered).map_err(|e| format!("Failed to write '{}': {e}", path.display()))
}

pub(super) fn update_org_yaml_prompt_paths(root: &Path) -> Result<(), String> {
    let path = org_schema_path(root);
    if !path.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    let mut yaml: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|e| format!("Failed to parse '{}': {e}", path.display()))?;
    rewrite_prompt_paths_yaml(&mut yaml, root);
    let rendered = serde_yaml::to_string(&yaml)
        .map_err(|e| format!("Failed to serialize '{}': {e}", path.display()))?;
    fs::write(&path, rendered).map_err(|e| format!("Failed to write '{}': {e}", path.display()))
}

fn rewrite_prompt_paths_json(value: &mut Value, root: &Path) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if matches!(key.as_str(), "promptFile" | "prompt_file") {
                    if let Some(raw) = child.as_str() {
                        *child = Value::String(rewrite_prompt_path(raw));
                    }
                } else if key == "sharedPromptFile" {
                    *child = Value::String(shared_prompt_path(root).display().to_string());
                } else {
                    rewrite_prompt_paths_json(child, root);
                }
            }
        }
        Value::Array(items) => {
            for child in items {
                rewrite_prompt_paths_json(child, root);
            }
        }
        _ => {}
    }
}

fn rewrite_prompt_paths_yaml(value: &mut serde_yaml::Value, root: &Path) {
    match value {
        serde_yaml::Value::Mapping(map) => {
            for (key, child) in map.iter_mut() {
                let key_str = key.as_str().unwrap_or_default();
                if matches!(key_str, "promptFile" | "prompt_file") {
                    if let Some(raw) = child.as_str() {
                        *child = serde_yaml::Value::String(rewrite_prompt_path(raw));
                    }
                } else if key_str == "shared_prompt" {
                    *child =
                        serde_yaml::Value::String(shared_prompt_path(root).display().to_string());
                } else {
                    rewrite_prompt_paths_yaml(child, root);
                }
            }
        }
        serde_yaml::Value::Sequence(items) => {
            for child in items {
                rewrite_prompt_paths_yaml(child, root);
            }
        }
        _ => {}
    }
}

fn rewrite_prompt_path(raw: &str) -> String {
    raw.replace("role-context/", "agents/")
        .replace("role-context\\", "agents\\")
}
