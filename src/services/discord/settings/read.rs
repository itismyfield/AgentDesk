use super::*;

#[derive(Clone, Debug, Default)]
struct LegacyBotSettingsEntry {
    agent: Option<String>,
    provider: Option<ProviderKind>,
    allowed_tools: Option<Vec<String>>,
    allowed_channel_ids: Vec<u64>,
    require_mention_channel_ids: Vec<u64>,
    channel_model_overrides: std::collections::HashMap<String, String>,
    owner_user_id: Option<u64>,
    allowed_user_ids: Vec<u64>,
    allow_all_users: Option<bool>,
    allowed_bot_ids: Vec<u64>,
}

fn load_legacy_bot_settings_json() -> Option<serde_json::Value> {
    let path = bot_settings_path()?;
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str::<serde_json::Value>(&content).ok()
}

fn load_legacy_bot_settings_entry(token: &str) -> LegacyBotSettingsEntry {
    let Some(json) = load_legacy_bot_settings_json() else {
        return LegacyBotSettingsEntry::default();
    };
    let Some(obj) = json.as_object() else {
        return LegacyBotSettingsEntry::default();
    };
    let Some((_, entry)) = find_bot_settings_entry(obj, token) else {
        return LegacyBotSettingsEntry::default();
    };

    let agent = entry
        .get("agent")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let provider = entry
        .get("provider")
        .and_then(|v| v.as_str())
        .map(ProviderKind::from_str_or_unsupported);
    let allowed_channel_ids = entry
        .get("allowed_channel_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(json_u64).collect())
        .unwrap_or_default();
    let require_mention_channel_ids = entry
        .get("require_mention_channel_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(json_u64).collect())
        .unwrap_or_default();
    let channel_model_overrides = entry
        .get("channel_model_overrides")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(channel_id, model)| {
                    model
                        .as_str()
                        .map(|model| (channel_id.clone(), model.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let owner_user_id = entry.get("owner_user_id").and_then(json_u64);
    let allowed_user_ids = entry
        .get("allowed_user_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(json_u64).collect())
        .unwrap_or_default();
    let allow_all_users = entry.get("allow_all_users").and_then(|v| v.as_bool());
    let allowed_bot_ids = entry
        .get("allowed_bot_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(json_u64).collect())
        .unwrap_or_default();

    let allowed_tools = match entry.get("allowed_tools") {
        None => None,
        Some(value) => value
            .as_array()
            .map(|tools_arr| normalize_allowed_tools(tools_arr.iter().filter_map(|v| v.as_str()))),
    };

    LegacyBotSettingsEntry {
        agent,
        provider,
        allowed_tools,
        allowed_channel_ids,
        require_mention_channel_ids,
        channel_model_overrides,
        owner_user_id,
        allowed_user_ids,
        allow_all_users,
        allowed_bot_ids,
    }
}

fn warn_legacy_bot_setting_fallback(field_name: &str) {
    tracing::warn!(
        field = %field_name,
        "bot setting missing from YAML, falling back to legacy bot_settings.json"
    );
}

fn fallback_legacy_option<T>(
    configured: Option<T>,
    legacy: Option<T>,
    field_name: &'static str,
) -> Option<T> {
    configured.or_else(|| {
        legacy.map(|value| {
            warn_legacy_bot_setting_fallback(field_name);
            value
        })
    })
}

fn fallback_legacy_vec<T>(
    configured: Option<Vec<T>>,
    legacy: Vec<T>,
    field_name: &'static str,
) -> Vec<T> {
    match configured {
        Some(values) => values,
        None if !legacy.is_empty() => {
            warn_legacy_bot_setting_fallback(field_name);
            legacy
        }
        None => Vec::new(),
    }
}

fn load_kv_meta_value(db: Option<&crate::db::Db>, key: &str) -> Option<String> {
    let db = db?;
    let conn = db.read_conn().ok()?;
    conn.query_row("SELECT value FROM kv_meta WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .ok()
    .filter(|value| !value.trim().is_empty())
}

pub(crate) fn load_last_session_path(
    db: Option<&crate::db::Db>,
    token_hash: &str,
    channel_id: u64,
) -> Option<String> {
    load_kv_meta_value(db, &last_session_path_key(token_hash, channel_id))
}

pub(crate) fn load_last_remote_profile(
    db: Option<&crate::db::Db>,
    token_hash: &str,
    channel_id: u64,
) -> Option<String> {
    load_kv_meta_value(db, &last_remote_profile_key(token_hash, channel_id))
}

pub(crate) fn save_last_session_runtime(
    db: Option<&crate::db::Db>,
    token_hash: &str,
    channel_id: u64,
    current_path: &str,
    remote_profile_name: Option<&str>,
) {
    let Some(db) = db else {
        return;
    };
    let Ok(conn) = db.lock() else {
        return;
    };

    let _ = conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
        [
            last_session_path_key(token_hash, channel_id),
            current_path.to_string(),
        ],
    );

    let remote_key = last_remote_profile_key(token_hash, channel_id);
    match remote_profile_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(remote) => {
            let _ = conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                [remote_key, remote.to_string()],
            );
        }
        None => {
            let _ = conn.execute("DELETE FROM kv_meta WHERE key = ?1", [remote_key]);
        }
    }
}

pub(crate) fn load_bot_settings(token: &str) -> DiscordBotSettings {
    let configured = agentdesk_config::find_discord_bot_by_token(token);
    let legacy = load_legacy_bot_settings_entry(token);
    let provider = fallback_legacy_option(
        configured.as_ref().and_then(|bot| bot.provider.clone()),
        legacy.provider.clone(),
        "provider",
    )
    .unwrap_or(ProviderKind::Claude);
    let allowed_tools = configured
        .as_ref()
        .and_then(|bot| bot.auth.allowed_tools.as_ref().cloned())
        .map(|tools| normalize_allowed_tools(&tools))
        .or(legacy.allowed_tools.clone())
        .unwrap_or_else(|| default_allowed_tools_for_provider(&provider));

    DiscordBotSettings {
        agent: fallback_legacy_option(
            configured.as_ref().and_then(|bot| bot.agent.clone()),
            legacy.agent,
            "agent",
        ),
        provider,
        allowed_tools,
        allowed_channel_ids: fallback_legacy_vec(
            configured
                .as_ref()
                .and_then(|bot| bot.auth.allowed_channel_ids.clone()),
            legacy.allowed_channel_ids,
            "allowed_channel_ids",
        ),
        require_mention_channel_ids: fallback_legacy_vec(
            configured
                .as_ref()
                .and_then(|bot| bot.auth.require_mention_channel_ids.clone()),
            legacy.require_mention_channel_ids,
            "require_mention_channel_ids",
        ),
        channel_model_overrides: legacy.channel_model_overrides,
        owner_user_id: fallback_legacy_option(
            configured.as_ref().and_then(|bot| bot.owner_id),
            legacy.owner_user_id,
            "owner_user_id",
        ),
        allowed_user_ids: fallback_legacy_vec(
            configured
                .as_ref()
                .and_then(|bot| bot.auth.allowed_user_ids.clone()),
            legacy.allowed_user_ids,
            "allowed_user_ids",
        ),
        allow_all_users: fallback_legacy_option(
            configured.as_ref().and_then(|bot| bot.auth.allow_all_users),
            legacy.allow_all_users,
            "allow_all_users",
        )
        .unwrap_or(false),
        allowed_bot_ids: fallback_legacy_vec(
            configured
                .as_ref()
                .and_then(|bot| bot.auth.allowed_bot_ids.clone()),
            legacy.allowed_bot_ids,
            "allowed_bot_ids",
        ),
    }
}

pub fn load_discord_bot_launch_configs() -> Vec<DiscordBotLaunchConfig> {
    let configured = agentdesk_config::load_discord_bot_configs();
    if !configured.is_empty() {
        let agent_bot_names = agentdesk_config::collect_agent_bot_names();
        let mut configs = configured
            .into_iter()
            .filter(|bot| agent_bot_names.contains(&bot.name))
            .map(|bot| {
                let legacy = load_legacy_bot_settings_entry(&bot.token);
                DiscordBotLaunchConfig {
                    hash_key: discord_token_hash(&bot.token),
                    token: bot.token,
                    provider: bot
                        .provider
                        .or(legacy.provider)
                        .unwrap_or(ProviderKind::Claude),
                }
            })
            .collect::<Vec<_>>();
        configs.sort_by(|left, right| left.hash_key.cmp(&right.hash_key));
        configs.dedup_by(|left, right| left.token == right.token);
        return configs;
    }

    let Some(json) = load_legacy_bot_settings_json() else {
        return Vec::new();
    };
    let Some(obj) = json.as_object() else {
        return Vec::new();
    };

    let mut configs_by_token: std::collections::BTreeMap<String, DiscordBotLaunchConfig> =
        std::collections::BTreeMap::new();
    for (hash_key, entry) in obj {
        let Some(token) = entry.get("token").and_then(|v| v.as_str()) else {
            continue;
        };
        let provider = entry
            .get("provider")
            .and_then(|v| v.as_str())
            .map(ProviderKind::from_str_or_unsupported)
            .unwrap_or(ProviderKind::Claude);
        let config = DiscordBotLaunchConfig {
            hash_key: hash_key.clone(),
            token: token.to_string(),
            provider,
        };
        let canonical_key = discord_token_hash(token);
        match configs_by_token.get(token) {
            Some(existing) if existing.hash_key == canonical_key => {}
            _ if hash_key == &canonical_key => {
                configs_by_token.insert(token.to_string(), config);
            }
            None => {
                configs_by_token.insert(token.to_string(), config);
            }
            Some(_) => {}
        }
    }
    configs_by_token.into_values().collect()
}

pub fn resolve_discord_token_by_hash(hash: &str) -> Option<String> {
    if let Some(token) = agentdesk_config::load_discord_bot_configs()
        .into_iter()
        .find(|bot| discord_token_hash(&bot.token) == hash)
        .map(|bot| bot.token)
    {
        return Some(token);
    }

    let json = load_legacy_bot_settings_json()?;
    let obj = json.as_object()?;
    let entry = obj.get(hash)?;
    entry
        .get("token")
        .and_then(|v| v.as_str())
        .map(String::from)
}

pub fn resolve_discord_bot_provider(token: &str) -> ProviderKind {
    load_bot_settings(token).provider
}
