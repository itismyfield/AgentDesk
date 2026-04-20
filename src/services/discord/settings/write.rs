use super::*;

#[derive(Debug, Clone)]
enum FileSnapshot {
    Missing,
    Bytes(Vec<u8>),
}

fn capture_file_snapshot(path: &Path) -> std::io::Result<FileSnapshot> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => fs::read(path).map(FileSnapshot::Bytes),
        Ok(_) => Ok(FileSnapshot::Missing),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(FileSnapshot::Missing),
        Err(err) => Err(err),
    }
}

fn temp_write_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.tmp".to_string());
    path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()))
}

fn write_bytes_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp_path = temp_write_path(path);
    fs::write(&temp_path, bytes)?;
    match fs::rename(&temp_path, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&temp_path);
            Err(err)
        }
    }
}

fn restore_file_snapshot(path: &Path, snapshot: &FileSnapshot) -> std::io::Result<()> {
    match snapshot {
        FileSnapshot::Missing => match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => fs::remove_file(path),
            Ok(_) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        },
        FileSnapshot::Bytes(bytes) => write_bytes_atomically(path, bytes),
    }
}

fn config_io_error(path: &Path, err: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(format!("{}: {}", path.display(), err))
}

fn persist_bot_auth_to_yaml_checked(
    token: &str,
    settings: &DiscordBotSettings,
) -> std::io::Result<()> {
    let Some(path) = super::config_path_for_write() else {
        return Ok(());
    };

    let mut config = if path.is_file() {
        crate::config::load_from_path(&path).map_err(|err| config_io_error(&path, err))?
    } else {
        crate::config::Config::default()
    };

    let Some(bot_name) = super::resolved_config_bot_name(&config, token) else {
        // Do not mutate YAML for tokens that are not managed by agentdesk.yaml.
        // This prevents owner imprinting from an unconfigured bot from overwriting
        // the shared discord.owner_id used by configured bots.
        return Ok(());
    };

    // Keep the onboarding-configured owner stable; runtime settings should only
    // fill the owner when the YAML is still unset.
    if config.discord.owner_id.is_none() {
        config.discord.owner_id = settings.owner_user_id;
    }

    if let Some(bot) = config.discord.bots.get_mut(&bot_name) {
        bot.provider = Some(settings.provider.as_str().to_string());
        bot.agent = settings.agent.clone();
        bot.auth.allowed_channel_ids = Some(settings.allowed_channel_ids.clone());
        bot.auth.require_mention_channel_ids = Some(settings.require_mention_channel_ids.clone());
        bot.auth.allowed_user_ids = Some(settings.allowed_user_ids.clone());
        bot.auth.allowed_tools = Some(normalize_allowed_tools(&settings.allowed_tools));
        bot.auth.allow_all_users = Some(settings.allow_all_users);
        bot.auth.allowed_bot_ids = Some(settings.allowed_bot_ids.clone());
    }

    let rendered = serde_yaml::to_string(&config).map_err(|err| config_io_error(&path, err))?;
    write_bytes_atomically(&path, rendered.as_bytes())
}

fn save_runtime_bot_settings_checked(
    token: &str,
    settings: &DiscordBotSettings,
) -> std::io::Result<()> {
    #[cfg(test)]
    if force_runtime_settings_write_failure_for_tests() {
        return Err(std::io::Error::other(
            "forced runtime bot settings write failure for tests",
        ));
    }

    let Some(path) = bot_settings_path() else {
        return Ok(());
    };

    let mut json: serde_json::Value = match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({})),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(err) => return Err(err),
    };
    let Some(obj) = json.as_object_mut() else {
        return Ok(());
    };

    let yaml_manages_bot = super::config_path_for_write()
        .map(|config_path| {
            let config = if config_path.is_file() {
                crate::config::load_from_path(&config_path).unwrap_or_default()
            } else {
                crate::config::Config::default()
            };
            super::resolved_config_bot_name(&config, token).is_some()
        })
        .unwrap_or(false);
    let legacy_metadata = super::find_bot_settings_entry(obj, token)
        .and_then(|(_, entry)| entry.as_object().cloned());
    let key = super::discord_token_hash(token);
    obj.retain(|existing_key, existing_entry| {
        if existing_key == &key {
            return false;
        }
        existing_entry
            .get("token")
            .and_then(|value| value.as_str())
            .map(|existing_token| existing_token != token)
            .unwrap_or(true)
    });
    let mut sorted_fast_mode_reset_pending: Vec<_> = settings
        .channel_fast_mode_reset_pending
        .iter()
        .cloned()
        .collect();
    sorted_fast_mode_reset_pending.sort();

    if yaml_manages_bot {
        if !settings.channel_model_overrides.is_empty()
            || !settings.channel_fast_modes.is_empty()
            || !sorted_fast_mode_reset_pending.is_empty()
        {
            let mut runtime_entry = serde_json::Map::new();
            if !settings.channel_model_overrides.is_empty() {
                runtime_entry.insert(
                    "channel_model_overrides".to_string(),
                    serde_json::json!(settings.channel_model_overrides),
                );
            }
            if !settings.channel_fast_modes.is_empty() {
                runtime_entry.insert(
                    "channel_fast_modes".to_string(),
                    serde_json::json!(settings.channel_fast_modes),
                );
            }
            if !sorted_fast_mode_reset_pending.is_empty() {
                runtime_entry.insert(
                    "channel_fast_mode_reset_pending".to_string(),
                    serde_json::json!(sorted_fast_mode_reset_pending),
                );
            }
            obj.insert(key, serde_json::Value::Object(runtime_entry));
        }
    } else {
        let mut entry = legacy_metadata.unwrap_or_default();
        entry.insert("token".to_string(), serde_json::json!(token));
        entry.insert(
            "provider".to_string(),
            serde_json::json!(settings.provider.as_str()),
        );
        match settings
            .agent
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            Some(agent) => {
                entry.insert("agent".to_string(), serde_json::json!(agent));
            }
            None => {
                entry.remove("agent");
            }
        }
        if settings.allowed_channel_ids.is_empty() {
            entry.remove("allowed_channel_ids");
        } else {
            entry.insert(
                "allowed_channel_ids".to_string(),
                serde_json::json!(settings.allowed_channel_ids),
            );
        }
        if settings.require_mention_channel_ids.is_empty() {
            entry.remove("require_mention_channel_ids");
        } else {
            entry.insert(
                "require_mention_channel_ids".to_string(),
                serde_json::json!(settings.require_mention_channel_ids),
            );
        }
        if settings.allowed_user_ids.is_empty() {
            entry.remove("allowed_user_ids");
        } else {
            entry.insert(
                "allowed_user_ids".to_string(),
                serde_json::json!(settings.allowed_user_ids),
            );
        }
        if settings.allowed_bot_ids.is_empty() {
            entry.remove("allowed_bot_ids");
        } else {
            entry.insert(
                "allowed_bot_ids".to_string(),
                serde_json::json!(settings.allowed_bot_ids),
            );
        }
        if settings.allowed_tools.is_empty() {
            entry.remove("allowed_tools");
        } else {
            entry.insert(
                "allowed_tools".to_string(),
                serde_json::json!(normalize_allowed_tools(&settings.allowed_tools)),
            );
        }
        if settings.allow_all_users {
            entry.insert("allow_all_users".to_string(), serde_json::json!(true));
        } else {
            entry.remove("allow_all_users");
        }
        match settings.owner_user_id {
            Some(owner_user_id) => {
                entry.insert(
                    "owner_user_id".to_string(),
                    serde_json::json!(owner_user_id),
                );
            }
            None => {
                entry.remove("owner_user_id");
            }
        }
        if settings.channel_model_overrides.is_empty() {
            entry.remove("channel_model_overrides");
        } else {
            entry.insert(
                "channel_model_overrides".to_string(),
                serde_json::json!(settings.channel_model_overrides),
            );
        }
        if settings.channel_fast_modes.is_empty() {
            entry.remove("channel_fast_modes");
        } else {
            entry.insert(
                "channel_fast_modes".to_string(),
                serde_json::json!(settings.channel_fast_modes),
            );
        }
        if sorted_fast_mode_reset_pending.is_empty() {
            entry.remove("channel_fast_mode_reset_pending");
        } else {
            entry.insert(
                "channel_fast_mode_reset_pending".to_string(),
                serde_json::json!(sorted_fast_mode_reset_pending),
            );
        }
        obj.insert(key, serde_json::Value::Object(entry));
    }

    if obj.is_empty() {
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => fs::remove_file(&path)?,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        return Ok(());
    }

    let rendered = serde_json::to_vec_pretty(&json).map_err(|err| config_io_error(&path, err))?;
    write_bytes_atomically(&path, &rendered)
}

pub(crate) fn save_bot_settings(token: &str, settings: &DiscordBotSettings) {
    let yaml_path = super::config_path_for_write();
    let yaml_snapshot = yaml_path
        .as_ref()
        .and_then(|path| capture_file_snapshot(path).ok());
    let json_path = bot_settings_path();
    let json_snapshot = json_path
        .as_ref()
        .and_then(|path| capture_file_snapshot(path).ok());

    if let Err(err) = persist_bot_auth_to_yaml_checked(token, settings) {
        tracing::warn!("failed to persist bot settings yaml: {err}");
        return;
    }

    if let Err(err) = save_runtime_bot_settings_checked(token, settings) {
        let mut rollback_failed = false;

        if let (Some(path), Some(snapshot)) = (yaml_path.as_ref(), yaml_snapshot.as_ref()) {
            if let Err(rollback_err) = restore_file_snapshot(path, snapshot) {
                rollback_failed = true;
                tracing::warn!(
                    "failed to roll back yaml after runtime bot settings write failed: {rollback_err}"
                );
            }
        }

        if let (Some(path), Some(snapshot)) = (json_path.as_ref(), json_snapshot.as_ref()) {
            if let Err(rollback_err) = restore_file_snapshot(path, snapshot) {
                rollback_failed = true;
                tracing::warn!(
                    "failed to roll back runtime bot settings after write failure: {rollback_err}"
                );
            }
        }

        if rollback_failed {
            tracing::warn!(
                "failed to persist runtime bot settings and at least one rollback step failed: {err}"
            );
        } else {
            tracing::warn!(
                "failed to persist runtime bot settings; yaml/json changes rolled back: {err}"
            );
        }
    }
}

#[cfg(test)]
static FORCE_RUNTIME_SETTINGS_WRITE_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
fn force_runtime_settings_write_failure_for_tests() -> bool {
    FORCE_RUNTIME_SETTINGS_WRITE_FAILURE.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) fn set_force_runtime_settings_write_failure_for_tests(enabled: bool) {
    FORCE_RUNTIME_SETTINGS_WRITE_FAILURE.store(enabled, std::sync::atomic::Ordering::SeqCst);
}
