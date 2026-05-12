
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn collect_onboarding_conflicts(
    conn: &sqlite_test::Connection,
    runtime_root: &Path,
    provider: &str,
    resolved_channels: &[ResolvedChannelMapping],
    rerun_policy: OnboardingRerunPolicy,
) -> Result<Vec<String>, String> {
    use sqlite_test::OptionalExtension;

    validate_unique_resolved_channels(resolved_channels)?;

    let config = load_onboarding_config(runtime_root)?;
    let role_map = load_onboarding_role_map(runtime_root)?;
    let by_channel_id = role_map
        .get("byChannelId")
        .and_then(|value| value.as_object());
    let by_channel_name = role_map
        .get("byChannelName")
        .and_then(|value| value.as_object());

    let mut conflicts = Vec::new();

    for mapping in resolved_channels {
        let existing_agent = conn
            .query_row(
                "SELECT provider, discord_channel_id, description, system_prompt \
                 FROM agents WHERE id = ?1",
                [mapping.role_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| format!("failed to query agent {}: {e}", mapping.role_id))?;

        if let Some((
            existing_provider,
            existing_channel_id,
            existing_description,
            existing_prompt,
        )) = existing_agent
        {
            if rerun_policy == OnboardingRerunPolicy::ReuseExisting {
                if let Some(existing_channel_id) =
                    normalized_optional_text(existing_channel_id.as_deref())
                {
                    if existing_channel_id != mapping.channel_id {
                        conflicts.push(format!(
                            "agent '{}' already uses Discord channel '{}' in DB; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            mapping.role_id, existing_channel_id, mapping.channel_id
                        ));
                    }
                }

                if let Some(existing_provider) =
                    normalized_optional_text(existing_provider.as_deref())
                {
                    if existing_provider != provider {
                        conflicts.push(format!(
                            "agent '{}' already uses provider '{}' in config DB state; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            mapping.role_id, existing_provider, provider
                        ));
                    }
                }

                if let (Some(existing), Some(requested)) = (
                    normalized_optional_text(existing_description.as_deref()),
                    normalized_optional_text(mapping.description.as_deref()),
                ) {
                    if existing != requested {
                        conflicts.push(format!(
                            "agent '{}' already has a different description in DB; rerun_policy=reuse_existing refuses to overwrite it",
                            mapping.role_id
                        ));
                    }
                }

                if let (Some(existing), Some(requested)) = (
                    normalized_optional_text(existing_prompt.as_deref()),
                    normalized_optional_text(mapping.system_prompt.as_deref()),
                ) {
                    if existing != requested {
                        conflicts.push(format!(
                            "agent '{}' already has a different system prompt in DB; rerun_policy=reuse_existing refuses to overwrite it",
                            mapping.role_id
                        ));
                    }
                }
            }
        }

        let conflicting_db_channel_owner = conn
            .query_row(
                "SELECT id FROM agents WHERE discord_channel_id = ?1 AND id != ?2 LIMIT 1",
                sqlite_test::params![mapping.channel_id, mapping.role_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| {
                format!(
                    "failed to check existing DB channel owner {}: {e}",
                    mapping.channel_id
                )
            })?;
        if let Some(other_agent_id) = conflicting_db_channel_owner {
            conflicts.push(format!(
                "Discord channel '{}' is already assigned to agent '{}' in DB",
                mapping.channel_id, other_agent_id
            ));
        }

        if let Some(agent) = config
            .agents
            .iter()
            .find(|agent| agent.id == mapping.role_id)
        {
            if rerun_policy == OnboardingRerunPolicy::ReuseExisting && agent.provider != provider {
                conflicts.push(format!(
                    "agent '{}' already uses provider '{}' in agentdesk.yaml; rerun_policy=reuse_existing refuses to replace it with '{}'",
                    mapping.role_id, agent.provider, provider
                ));
            }

            if rerun_policy == OnboardingRerunPolicy::ReuseExisting {
                if let Some(slot) = agent_channel_slot_ref(&agent.channels, provider) {
                    let channel = channel_config_from_existing(slot.clone());
                    let existing_channel_id = channel.channel_id();
                    let existing_names = channel.all_names();
                    let same_channel_id =
                        existing_channel_id.as_deref() == Some(mapping.channel_id.as_str());
                    let same_channel_name = existing_names.iter().any(|name| {
                        name == &mapping.channel_name || name == &mapping.requested_channel_name
                    });
                    let conflicts_with_existing = if existing_channel_id.is_some() {
                        !same_channel_id
                    } else {
                        !existing_names.is_empty() && !same_channel_name
                    };
                    if conflicts_with_existing {
                        conflicts.push(format!(
                            "agent '{}' already maps to a different channel in agentdesk.yaml; rerun_policy=reuse_existing refuses to replace it",
                            mapping.role_id
                        ));
                    }
                }
            }
        }

        for agent in &config.agents {
            if agent.id == mapping.role_id {
                continue;
            }
            let Some(slot) = agent_channel_slot_ref(&agent.channels, provider) else {
                continue;
            };
            let channel = channel_config_from_existing(slot.clone());
            let uses_same_target = channel.channel_id().as_deref()
                == Some(mapping.channel_id.as_str())
                || channel.all_names().iter().any(|name| {
                    name == &mapping.channel_name || name == &mapping.requested_channel_name
                });
            if uses_same_target {
                conflicts.push(format!(
                    "agent '{}' already owns channel '{}' in agentdesk.yaml",
                    agent.id, mapping.channel_id
                ));
            }
        }

        if let Some(entry) = by_channel_id.and_then(|entries| entries.get(&mapping.channel_id))
            && let Some(role_id) = role_map_entry_role_id(entry)
            && role_id != mapping.role_id
        {
            conflicts.push(format!(
                "role_map.json already binds channel '{}' to agent '{}'",
                mapping.channel_id, role_id
            ));
        }

        if let Some(entry) = by_channel_name.and_then(|entries| entries.get(&mapping.channel_name))
            && let Some(role_id) = role_map_entry_role_id(entry)
            && role_id != mapping.role_id
        {
            conflicts.push(format!(
                "role_map.json already binds channel name '{}' to agent '{}'",
                mapping.channel_name, role_id
            ));
        }

        if rerun_policy == OnboardingRerunPolicy::ReuseExisting {
            if let Some(entries) = by_channel_id {
                for (existing_channel_id, entry) in entries {
                    if role_map_entry_role_id(entry) == Some(mapping.role_id.as_str())
                        && existing_channel_id != &mapping.channel_id
                    {
                        conflicts.push(format!(
                            "role_map.json already binds agent '{}' to Discord channel '{}'; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            mapping.role_id, existing_channel_id, mapping.channel_id
                        ));
                    }
                }
            }

            if let Some(entries) = by_channel_name {
                for (existing_name, entry) in entries {
                    if role_map_entry_role_id(entry) != Some(mapping.role_id.as_str()) {
                        continue;
                    }

                    let same_name = existing_name == &mapping.channel_name
                        || existing_name == &mapping.requested_channel_name;
                    if !same_name {
                        conflicts.push(format!(
                            "role_map.json already binds agent '{}' to channel name '{}'; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            mapping.role_id, existing_name, mapping.channel_name
                        ));
                        continue;
                    }

                    if let Some(existing_channel_id) = role_map_entry_channel_id(entry)
                        && existing_channel_id != mapping.channel_id
                    {
                        conflicts.push(format!(
                            "role_map.json already binds channel name '{}' for agent '{}' to Discord channel '{}'; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            existing_name, mapping.role_id, existing_channel_id, mapping.channel_id
                        ));
                    }
                }
            }
        }
    }

    Ok(conflicts)
}

async fn collect_onboarding_conflicts_pg(
    pool: &sqlx::PgPool,
    runtime_root: &Path,
    provider: &str,
    resolved_channels: &[ResolvedChannelMapping],
    rerun_policy: OnboardingRerunPolicy,
) -> Result<Vec<String>, String> {
    validate_unique_resolved_channels(resolved_channels)?;

    let config = load_onboarding_config(runtime_root)?;
    let role_map = load_onboarding_role_map(runtime_root)?;
    let by_channel_id = role_map
        .get("byChannelId")
        .and_then(|value| value.as_object());
    let by_channel_name = role_map
        .get("byChannelName")
        .and_then(|value| value.as_object());

    let mut conflicts = Vec::new();

    for mapping in resolved_channels {
        let existing_agent = sqlx::query_as::<
            _,
            (
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            ),
        >(
            "SELECT provider, discord_channel_id, description, system_prompt \
             FROM agents WHERE id = $1",
        )
        .bind(&mapping.role_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("failed to query postgres agent {}: {e}", mapping.role_id))?;

        if let Some((
            existing_provider,
            existing_channel_id,
            existing_description,
            existing_prompt,
        )) = existing_agent
        {
            if rerun_policy == OnboardingRerunPolicy::ReuseExisting {
                if let Some(existing_channel_id) =
                    normalized_optional_text(existing_channel_id.as_deref())
                {
                    if existing_channel_id != mapping.channel_id {
                        conflicts.push(format!(
                            "agent '{}' already uses Discord channel '{}' in DB; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            mapping.role_id, existing_channel_id, mapping.channel_id
                        ));
                    }
                }

                if let Some(existing_provider) =
                    normalized_optional_text(existing_provider.as_deref())
                {
                    if existing_provider != provider {
                        conflicts.push(format!(
                            "agent '{}' already uses provider '{}' in config DB state; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            mapping.role_id, existing_provider, provider
                        ));
                    }
                }

                if let (Some(existing), Some(requested)) = (
                    normalized_optional_text(existing_description.as_deref()),
                    normalized_optional_text(mapping.description.as_deref()),
                ) {
                    if existing != requested {
                        conflicts.push(format!(
                            "agent '{}' already has a different description in DB; rerun_policy=reuse_existing refuses to overwrite it",
                            mapping.role_id
                        ));
                    }
                }

                if let (Some(existing), Some(requested)) = (
                    normalized_optional_text(existing_prompt.as_deref()),
                    normalized_optional_text(mapping.system_prompt.as_deref()),
                ) {
                    if existing != requested {
                        conflicts.push(format!(
                            "agent '{}' already has a different system prompt in DB; rerun_policy=reuse_existing refuses to overwrite it",
                            mapping.role_id
                        ));
                    }
                }
            }
        }

        let conflicting_db_channel_owner = sqlx::query_scalar::<_, String>(
            "SELECT id FROM agents WHERE discord_channel_id = $1 AND id != $2 LIMIT 1",
        )
        .bind(&mapping.channel_id)
        .bind(&mapping.role_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| {
            format!(
                "failed to check existing postgres DB channel owner {}: {e}",
                mapping.channel_id
            )
        })?;
        if let Some(other_agent_id) = conflicting_db_channel_owner {
            conflicts.push(format!(
                "Discord channel '{}' is already assigned to agent '{}' in DB",
                mapping.channel_id, other_agent_id
            ));
        }

        if let Some(agent) = config
            .agents
            .iter()
            .find(|agent| agent.id == mapping.role_id)
        {
            if rerun_policy == OnboardingRerunPolicy::ReuseExisting && agent.provider != provider {
                conflicts.push(format!(
                    "agent '{}' already uses provider '{}' in agentdesk.yaml; rerun_policy=reuse_existing refuses to replace it with '{}'",
                    mapping.role_id, agent.provider, provider
                ));
            }

            if rerun_policy == OnboardingRerunPolicy::ReuseExisting {
                if let Some(slot) = agent_channel_slot_ref(&agent.channels, provider) {
                    let channel = channel_config_from_existing(slot.clone());
                    let existing_channel_id = channel.channel_id();
                    let existing_names = channel.all_names();
                    let same_channel_id =
                        existing_channel_id.as_deref() == Some(mapping.channel_id.as_str());
                    let same_channel_name = existing_names.iter().any(|name| {
                        name == &mapping.channel_name || name == &mapping.requested_channel_name
                    });
                    let conflicts_with_existing = if existing_channel_id.is_some() {
                        !same_channel_id
                    } else {
                        !existing_names.is_empty() && !same_channel_name
                    };
                    if conflicts_with_existing {
                        conflicts.push(format!(
                            "agent '{}' already maps to a different channel in agentdesk.yaml; rerun_policy=reuse_existing refuses to replace it",
                            mapping.role_id
                        ));
                    }
                }
            }
        }

        for agent in &config.agents {
            if agent.id == mapping.role_id {
                continue;
            }
            let Some(slot) = agent_channel_slot_ref(&agent.channels, provider) else {
                continue;
            };
            let channel = channel_config_from_existing(slot.clone());
            let uses_same_target = channel.channel_id().as_deref()
                == Some(mapping.channel_id.as_str())
                || channel.all_names().iter().any(|name| {
                    name == &mapping.channel_name || name == &mapping.requested_channel_name
                });
            if uses_same_target {
                conflicts.push(format!(
                    "agent '{}' already owns channel '{}' in agentdesk.yaml",
                    agent.id, mapping.channel_id
                ));
            }
        }

        if let Some(entry) = by_channel_id.and_then(|entries| entries.get(&mapping.channel_id))
            && let Some(role_id) = role_map_entry_role_id(entry)
            && role_id != mapping.role_id
        {
            conflicts.push(format!(
                "role_map.json already binds channel '{}' to agent '{}'",
                mapping.channel_id, role_id
            ));
        }

        if let Some(entry) = by_channel_name.and_then(|entries| entries.get(&mapping.channel_name))
            && let Some(role_id) = role_map_entry_role_id(entry)
            && role_id != mapping.role_id
        {
            conflicts.push(format!(
                "role_map.json already binds channel name '{}' to agent '{}'",
                mapping.channel_name, role_id
            ));
        }

        if rerun_policy == OnboardingRerunPolicy::ReuseExisting {
            if let Some(entries) = by_channel_id {
                for (existing_channel_id, entry) in entries {
                    if role_map_entry_role_id(entry) == Some(mapping.role_id.as_str())
                        && existing_channel_id != &mapping.channel_id
                    {
                        conflicts.push(format!(
                            "role_map.json already binds agent '{}' to Discord channel '{}'; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            mapping.role_id, existing_channel_id, mapping.channel_id
                        ));
                    }
                }
            }

            if let Some(entries) = by_channel_name {
                for (existing_name, entry) in entries {
                    if role_map_entry_role_id(entry) != Some(mapping.role_id.as_str()) {
                        continue;
                    }

                    let same_name = existing_name == &mapping.channel_name
                        || existing_name == &mapping.requested_channel_name;
                    if !same_name {
                        conflicts.push(format!(
                            "role_map.json already binds agent '{}' to channel name '{}'; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            mapping.role_id, existing_name, mapping.channel_name
                        ));
                        continue;
                    }

                    if let Some(existing_channel_id) = role_map_entry_channel_id(entry)
                        && existing_channel_id != mapping.channel_id
                    {
                        conflicts.push(format!(
                            "role_map.json already binds channel name '{}' for agent '{}' to Discord channel '{}'; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            existing_name, mapping.role_id, existing_channel_id, mapping.channel_id
                        ));
                    }
                }
            }
        }
    }

    Ok(conflicts)
}

fn write_onboarding_role_map(
    runtime_root: &Path,
    provider: &str,
    resolved_channels: &[ResolvedChannelMapping],
) -> Result<(), String> {
    let mut role_map = load_onboarding_role_map(runtime_root)?;
    let root = role_map
        .as_object_mut()
        .ok_or_else(|| "role map root must be a JSON object".to_string())?;

    root.insert("version".to_string(), json!(1));
    root.entry("fallbackByChannelName".to_string())
        .or_insert_with(|| json!({ "enabled": true }));
    root.entry("byChannelId".to_string())
        .or_insert_with(|| json!({}));
    root.entry("byChannelName".to_string())
        .or_insert_with(|| json!({}));

    let resolved_role_ids = resolved_channels
        .iter()
        .map(|mapping| mapping.role_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    root.get_mut("byChannelId")
        .and_then(|value| value.as_object_mut())
        .ok_or_else(|| "role map byChannelId must be a JSON object".to_string())?
        .retain(|_, entry| {
            role_map_entry_role_id(entry)
                .map(|role_id| !resolved_role_ids.contains(role_id))
                .unwrap_or(true)
        });
    root.get_mut("byChannelName")
        .and_then(|value| value.as_object_mut())
        .ok_or_else(|| "role map byChannelName must be a JSON object".to_string())?
        .retain(|_, entry| {
            role_map_entry_role_id(entry)
                .map(|role_id| !resolved_role_ids.contains(role_id))
                .unwrap_or(true)
        });

    for mapping in resolved_channels {
        let workspace_tilde =
            tilde_display_path(&runtime_root.join("workspaces").join(&mapping.role_id));
        root.get_mut("byChannelId")
            .and_then(|value| value.as_object_mut())
            .ok_or_else(|| "role map byChannelId must be a JSON object".to_string())?
            .insert(
                mapping.channel_id.clone(),
                json!({
                    "roleId": mapping.role_id,
                    "provider": provider,
                    "workspace": workspace_tilde.clone(),
                }),
            );
        root.get_mut("byChannelName")
            .and_then(|value| value.as_object_mut())
            .ok_or_else(|| "role map byChannelName must be a JSON object".to_string())?
            .insert(
                mapping.channel_name.clone(),
                json!({
                    "roleId": mapping.role_id,
                    "channelId": mapping.channel_id,
                    "workspace": workspace_tilde,
                }),
            );
    }

    let path = crate::runtime_layout::role_map_path(runtime_root);
    let content = serde_json::to_string_pretty(&role_map)
        .map_err(|e| format!("failed to serialize role map: {e}"))?;
    std::fs::write(&path, content)
        .map_err(|e| format!("failed to write role map {}: {e}", path.display()))
}

fn write_agentdesk_channel_bindings(
    runtime_root: &Path,
    provider: &str,
    resolved_channels: &[ResolvedChannelMapping],
) -> Result<(), String> {
    let config_path = onboarding_config_path(runtime_root);
    let mut config = load_onboarding_config(runtime_root)?;

    for mapping in resolved_channels {
        let workspace = tilde_display_path(&runtime_root.join("workspaces").join(&mapping.role_id));
        let agent_index = if let Some(index) = config
            .agents
            .iter()
            .position(|agent| agent.id == mapping.role_id)
        {
            index
        } else {
            config.agents.push(crate::config::AgentDef {
                id: mapping.role_id.clone(),
                name: mapping.role_id.clone(),
                name_ko: None,
                aliases: Vec::new(),
                wake_word: None,
                voice_enabled: true,
                sensitivity_mode: None,
                provider: provider.to_string(),
                channels: crate::config::AgentChannels::default(),
                keywords: Vec::new(),
                department: None,
                avatar_emoji: None,
            });
            config.agents.len() - 1
        };

        let agent = &mut config.agents[agent_index];
        agent.provider = provider.to_string();

        let Some(slot) = agent_channel_slot_mut(&mut agent.channels, provider) else {
            return Err(format!(
                "unsupported provider for onboarding yaml sync: {provider}"
            ));
        };

        let mut channel = channel_config_from_existing(slot.clone());
        if let Some(existing_name) = channel
            .name
            .clone()
            .filter(|existing| existing != &mapping.channel_name)
        {
            push_channel_alias(&mut channel, existing_name);
        }
        channel.id = Some(mapping.channel_id.clone());
        channel.name = Some(mapping.channel_name.clone());
        channel.workspace = Some(workspace);
        channel.provider = Some(provider.to_string());
        *slot = Some(crate::config::AgentChannel::Detailed(channel));
    }

    crate::config::save_to_path(&config_path, &config)
        .map_err(|e| format!("Failed to write config {}: {e}", config_path.display()))
}

fn agent_channel_slot_ref<'a>(
    channels: &'a crate::config::AgentChannels,
    provider: &str,
) -> Option<&'a Option<crate::config::AgentChannel>> {
    match provider {
        "claude" => Some(&channels.claude),
        "codex" => Some(&channels.codex),
        "gemini" => Some(&channels.gemini),
        "opencode" => Some(&channels.opencode),
        "qwen" => Some(&channels.qwen),
        _ => None,
    }
}

fn verify_onboarding_settings_artifacts(
    runtime_root: &Path,
    primary_token: &str,
    primary_provider: &str,
    secondary_token: Option<&str>,
    secondary_provider: Option<&str>,
    guild_id: &str,
    owner_id: Option<&str>,
    announce_token: Option<&str>,
    notify_token: Option<&str>,
    resolved_channels: &[ResolvedChannelMapping],
) -> Result<serde_json::Value, String> {
    let config_path = onboarding_config_path(runtime_root);
    if !config_path.is_file() {
        return Err(format!(
            "onboarding config was not written at {}",
            config_path.display()
        ));
    }
    let config = crate::config::load_from_path(&config_path).map_err(|e| {
        format!(
            "failed to reload onboarding config {}: {e}",
            config_path.display()
        )
    })?;

    if config.discord.guild_id.as_deref() != Some(guild_id.trim()) {
        return Err(format!(
            "discord guild mismatch after onboarding: expected '{}' got {:?}",
            guild_id.trim(),
            config.discord.guild_id
        ));
    }
    let expected_owner_id = parse_owner_id(owner_id)?;
    if config.discord.owner_id != expected_owner_id {
        return Err(format!(
            "discord owner mismatch after onboarding: expected {:?} got {:?}",
            expected_owner_id, config.discord.owner_id
        ));
    }

    let command_bot = config
        .discord
        .bots
        .get("command")
        .ok_or_else(|| "missing command bot config after onboarding".to_string())?;
    if command_bot.token.as_deref() != Some(primary_token.trim()) {
        return Err("primary command token was not persisted".to_string());
    }
    if command_bot.provider.as_deref() != Some(primary_provider.trim()) {
        return Err("primary command provider was not persisted".to_string());
    }

    match secondary_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(expected_token) => {
            let command2 = config
                .discord
                .bots
                .get("command_2")
                .ok_or_else(|| "missing command_2 bot config after onboarding".to_string())?;
            let expected_provider = secondary_provider
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(default_secondary_command_provider(primary_provider));
            if command2.token.as_deref() != Some(expected_token) {
                return Err("secondary command token was not persisted".to_string());
            }
            if command2.provider.as_deref() != Some(expected_provider) {
                return Err("secondary command provider was not persisted".to_string());
            }
        }
        None => {
            if config.discord.bots.contains_key("command_2") {
                return Err("unexpected command_2 bot config remained after onboarding".to_string());
            }
        }
    }

    for mapping in resolved_channels {
        let agent = config
            .agents
            .iter()
            .find(|agent| agent.id == mapping.role_id)
            .ok_or_else(|| format!("agent '{}' missing from onboarding config", mapping.role_id))?;
        if agent.provider != primary_provider {
            return Err(format!(
                "agent '{}' provider mismatch after onboarding: expected '{}' got '{}'",
                mapping.role_id, primary_provider, agent.provider
            ));
        }
        let slot = agent_channel_slot_ref(&agent.channels, primary_provider).ok_or_else(|| {
            format!(
                "unsupported provider '{}' in onboarding verification",
                primary_provider
            )
        })?;
        let channel = channel_config_from_existing(slot.clone());
        if channel.id.as_deref() != Some(mapping.channel_id.as_str()) {
            return Err(format!(
                "agent '{}' channel id mismatch after onboarding",
                mapping.role_id
            ));
        }
        if channel.name.as_deref() != Some(mapping.channel_name.as_str()) {
            return Err(format!(
                "agent '{}' channel name mismatch after onboarding",
                mapping.role_id
            ));
        }
    }

    let role_map_path = crate::runtime_layout::role_map_path(runtime_root);
    if !role_map_path.is_file() {
        return Err(format!(
            "role map was not written at {}",
            role_map_path.display()
        ));
    }

    let workspace_root = runtime_root.join("workspaces");
    for mapping in resolved_channels {
        let workspace = workspace_root.join(&mapping.role_id);
        if !workspace.is_dir() {
            return Err(format!(
                "workspace for agent '{}' missing at {}",
                mapping.role_id,
                workspace.display()
            ));
        }
    }

    let announce_path = crate::runtime_layout::credential_token_path(runtime_root, "announce");
    match announce_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(_) if !announce_path.is_file() => {
            return Err(format!(
                "announce credential missing at {}",
                announce_path.display()
            ));
        }
        None if announce_path.exists() => {
            return Err(format!(
                "announce credential should have been removed at {}",
                announce_path.display()
            ));
        }
        _ => {}
    }

    let notify_path = crate::runtime_layout::credential_token_path(runtime_root, "notify");
    match notify_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(_) if !notify_path.is_file() => {
            return Err(format!(
                "notify credential missing at {}",
                notify_path.display()
            ));
        }
        None if notify_path.exists() => {
            return Err(format!(
                "notify credential should have been removed at {}",
                notify_path.display()
            ));
        }
        _ => {}
    }

    Ok(json!({
        "config_path": config_path.display().to_string(),
        "role_map_path": role_map_path.display().to_string(),
        "workspace_root": workspace_root.display().to_string(),
        "workspace_count": resolved_channels.len(),
        "announce_credential_path": announce_path.display().to_string(),
        "notify_credential_path": notify_path.display().to_string(),
    }))
}

fn verify_onboarding_pipeline_artifact(runtime_root: &Path) -> Result<serde_json::Value, String> {
    let config_path = onboarding_config_path(runtime_root);
    let config = if config_path.is_file() {
        crate::config::load_from_path(&config_path).map_err(|e| {
            format!(
                "failed to reload onboarding config {}: {e}",
                config_path.display()
            )
        })?
    } else {
        crate::config::Config::default()
    };

    let mut candidates = Vec::new();
    candidates.push(config.policies.dir.join("default-pipeline.yaml"));
    if !config.policies.dir.is_absolute() {
        candidates.push(
            runtime_root
                .join(&config.policies.dir)
                .join("default-pipeline.yaml"),
        );
    }

    let pipeline_path = candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            format!(
                "default pipeline not found for onboarding under '{}' or runtime root '{}'",
                config.policies.dir.display(),
                runtime_root.display()
            )
        })?;

    let content = std::fs::read_to_string(&pipeline_path)
        .map_err(|e| format!("failed to read pipeline {}: {e}", pipeline_path.display()))?;
    let pipeline: crate::pipeline::PipelineConfig = serde_yaml::from_str(&content)
        .map_err(|e| format!("failed to parse pipeline {}: {e}", pipeline_path.display()))?;
    if pipeline.states.is_empty() || pipeline.transitions.is_empty() {
        return Err(format!(
            "pipeline {} is missing states or transitions",
            pipeline_path.display()
        ));
    }

    Ok(json!({
        "path": pipeline_path.display().to_string(),
        "states": pipeline.states.len(),
        "transitions": pipeline.transitions.len(),
    }))
}
