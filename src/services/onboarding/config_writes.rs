fn write_credential_token(
    runtime_root: &Path,
    bot_name: &str,
    token: Option<&str>,
) -> Result<(), String> {
    crate::runtime_layout::ensure_credential_layout(runtime_root)?;
    let credential_dir = crate::runtime_layout::credential_dir(runtime_root);
    std::fs::create_dir_all(&credential_dir).map_err(|e| e.to_string())?;
    let path = crate::runtime_layout::credential_token_path(runtime_root, bot_name);

    match token.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => std::fs::write(path, format!("{value}\n")).map_err(|e| e.to_string()),
        None => {
            if path.exists() {
                std::fs::remove_file(path).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }
}

fn onboarding_config_path(runtime_root: &Path) -> PathBuf {
    let canonical = crate::runtime_layout::config_file_path(runtime_root);
    let legacy = crate::runtime_layout::legacy_config_file_path(runtime_root);
    if canonical.is_file() || !legacy.is_file() {
        canonical
    } else {
        legacy
    }
}

fn default_secondary_command_provider(primary_provider: &str) -> &'static str {
    match primary_provider {
        "codex" => "claude",
        "gemini" => "codex",
        "opencode" => "claude",
        _ => "codex",
    }
}

fn parse_owner_id(owner_id: Option<&str>) -> Result<Option<u64>, String> {
    let Some(value) = owner_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    if !(17..=20).contains(&value.len()) || !value.chars().all(|ch| ch.is_ascii_digit()) {
        return Err("owner_id must be a Discord user id with 17-20 digits".to_string());
    }

    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| "owner_id must be a valid Discord user id".to_string())
}

fn upsert_command_bot(
    config: &mut crate::config::Config,
    bot_name: &str,
    token: &str,
    provider: &str,
) {
    let mut bot = config
        .discord
        .bots
        .get(bot_name)
        .cloned()
        .unwrap_or_default();
    bot.token = Some(token.trim().to_string());
    bot.provider = Some(provider.trim().to_string());
    config.discord.bots.insert(bot_name.to_string(), bot);
}

fn write_agentdesk_discord_config(
    runtime_root: &Path,
    guild_id: &str,
    primary_token: &str,
    primary_provider: &str,
    secondary_token: Option<&str>,
    secondary_provider: Option<&str>,
    owner_id: Option<&str>,
) -> Result<(), String> {
    let config_path = onboarding_config_path(runtime_root);
    let mut config = if config_path.is_file() {
        crate::config::load_from_path(&config_path)
            .map_err(|e| format!("Failed to load config {}: {e}", config_path.display()))?
    } else {
        crate::config::Config::default()
    };

    config.discord.guild_id = Some(guild_id.trim().to_string());
    config.discord.owner_id = parse_owner_id(owner_id)?;

    upsert_command_bot(&mut config, "command", primary_token, primary_provider);

    match secondary_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(token) => {
            let provider = secondary_provider
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(default_secondary_command_provider(primary_provider));
            upsert_command_bot(&mut config, "command_2", token, provider);
        }
        None => {
            config.discord.bots.remove("command_2");
        }
    }

    crate::config::save_to_path(&config_path, &config)
        .map_err(|e| format!("Failed to write config {}: {e}", config_path.display()))
}

fn tilde_display_path(path: &Path) -> String {
    dirs::home_dir()
        .and_then(|home| {
            path.strip_prefix(&home)
                .ok()
                .map(|relative| format!("~/{}", relative.display()))
        })
        .unwrap_or_else(|| path.display().to_string())
}

fn agent_channel_slot_mut<'a>(
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

fn channel_config_from_existing(
    current: Option<crate::config::AgentChannel>,
) -> crate::config::AgentChannelConfig {
    match current {
        Some(crate::config::AgentChannel::Detailed(config)) => config,
        Some(crate::config::AgentChannel::Legacy(raw)) => {
            let mut config = crate::config::AgentChannelConfig::default();
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                if trimmed.parse::<u64>().is_ok() {
                    config.id = Some(trimmed.to_string());
                } else {
                    config.name = Some(trimmed.to_string());
                }
            }
            config
        }
        None => crate::config::AgentChannelConfig::default(),
    }
}

fn push_channel_alias(config: &mut crate::config::AgentChannelConfig, alias: String) {
    let trimmed = alias.trim();
    if trimmed.is_empty() || config.name.as_deref() == Some(trimmed) {
        return;
    }
    if !config.aliases.iter().any(|existing| existing == trimmed) {
        config.aliases.push(trimmed.to_string());
        config.aliases.sort();
        config.aliases.dedup();
    }
}

fn load_onboarding_config(runtime_root: &Path) -> Result<crate::config::Config, String> {
    let config_path = onboarding_config_path(runtime_root);
    if config_path.is_file() {
        crate::config::load_from_path(&config_path)
            .map_err(|e| format!("Failed to load config {}: {e}", config_path.display()))
    } else {
        Ok(crate::config::Config::default())
    }
}

fn load_onboarding_role_map(runtime_root: &Path) -> Result<serde_json::Value, String> {
    let path = crate::runtime_layout::role_map_path(runtime_root);
    if !path.is_file() {
        return Ok(json!({
            "version": 1,
            "byChannelId": {},
            "byChannelName": {},
            "fallbackByChannelName": { "enabled": true },
        }));
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read role map {}: {e}", path.display()))?;
    serde_json::from_str(&content)
        .map_err(|e| format!("failed to parse role map {}: {e}", path.display()))
}

fn validate_unique_resolved_channels(
    resolved_channels: &[ResolvedChannelMapping],
) -> Result<(), String> {
    let mut seen_roles = std::collections::BTreeSet::new();
    let mut seen_channel_ids = std::collections::BTreeMap::new();

    for mapping in resolved_channels {
        if !seen_roles.insert(mapping.role_id.clone()) {
            return Err(format!(
                "duplicate onboarding agent id '{}' in completion payload",
                mapping.role_id
            ));
        }

        if let Some(previous_role) =
            seen_channel_ids.insert(mapping.channel_id.clone(), mapping.role_id.clone())
        {
            return Err(format!(
                "channel '{}' is assigned to both '{}' and '{}'",
                mapping.channel_id, previous_role, mapping.role_id
            ));
        }
    }

    Ok(())
}
