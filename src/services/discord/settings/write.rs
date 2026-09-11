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
    let result =
        crate::utils::secret_file::write_secret_file_preserving_parent_mode(&temp_path, bytes)
            .and_then(|()| fs::rename(&temp_path, path));
    match result {
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

/// Mutable entry for `key` under `parent`, created as null when absent.
///
/// A `parent` that is not a mapping is replaced with an empty one, matching what
/// the whole-`Config` round-trip this replaced did to a malformed section.
fn yaml_child<'a>(parent: &'a mut serde_yaml::Value, key: &str) -> &'a mut serde_yaml::Value {
    if !parent.is_mapping() {
        *parent = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let key = serde_yaml::Value::String(key.to_string());
    let map = parent
        .as_mapping_mut()
        .expect("value was just normalized to a mapping");
    if !map.contains_key(&key) {
        map.insert(key.clone(), serde_yaml::Value::Null);
    }
    map.get_mut(&key).expect("entry exists after the insert")
}

fn yaml_set(parent: &mut serde_yaml::Value, key: &str, value: serde_yaml::Value) {
    *yaml_child(parent, key) = value;
}

fn yaml_remove(parent: &mut serde_yaml::Value, key: &str) {
    if let Some(map) = parent.as_mapping_mut() {
        map.remove(&serde_yaml::Value::String(key.to_string()));
    }
}

// Local comparison only: include the two secrets deliberately omitted by Serialize.
// Never log this projection or differences containing its values.
fn typed_document(config: &crate::config::Config) -> Result<serde_yaml::Value, serde_yaml::Error> {
    let mut value = serde_yaml::to_value(config)?;
    yaml_set(
        yaml_child(&mut value, "server"),
        "auth_token",
        serde_yaml::to_value(&config.server.auth_token)?,
    );
    let bots = yaml_child(yaml_child(&mut value, "discord"), "bots");
    for (name, bot) in &config.discord.bots {
        yaml_set(
            yaml_child(bots, name),
            "token",
            serde_yaml::to_value(&bot.token)?,
        );
    }
    Ok(value)
}

// Promote existing scalar values/keys using the typed interpretation, without
// inserting defaults or removing unknown document fields. Unsupported shapes
// are caught by the typed equivalence gate before any write.
fn preserve_string_scalars(raw: &mut serde_yaml::Value, typed: &serde_yaml::Value) {
    use serde_yaml::Value;
    match (raw, typed) {
        (raw, Value::String(_)) => *raw = typed.clone(),
        (Value::Sequence(raw), Value::Sequence(typed)) => {
            for (raw, typed) in raw.iter_mut().zip(typed) {
                preserve_string_scalars(raw, typed);
            }
        }
        (Value::Mapping(raw), Value::Mapping(typed)) => {
            let original = std::mem::take(raw);
            let mut available = typed.clone();
            *raw = original
                .iter()
                .map(|(key, value)| {
                    let mut value = value.clone();
                    let matched = available.remove_entry(key).or_else(|| {
                        if key.is_string() {
                            return None;
                        }
                        let candidate = available
                            .keys()
                            .find(|candidate| {
                                !original.contains_key(*candidate)
                                    && candidate
                                        .as_str()
                                        .and_then(|s| serde_yaml::from_str::<Value>(s).ok())
                                        .as_ref()
                                        == Some(key)
                            })
                            .cloned()?;
                        available.remove_entry(&candidate)
                    });
                    if let Some((key, typed)) = matched {
                        preserve_string_scalars(&mut value, &typed);
                        (key, value)
                    } else {
                        (key.clone(), value)
                    }
                })
                .collect();
        }
        _ => {}
    }
}

fn check_typed_equivalence(expected: &crate::config::Config, rendered: &str) -> anyhow::Result<()> {
    let actual: crate::config::Config = serde_yaml::from_str(rendered)?;
    let expected = typed_document(expected)?;
    let actual = typed_document(&actual)?;
    if actual != expected {
        for (section, value) in expected.as_mapping().expect("Config is a mapping") {
            if actual.get(section) == Some(value) {
                continue;
            }
            anyhow::bail!(
                "settings write-back changed typed section {}",
                section.as_str().unwrap_or("unknown")
            );
        }
        anyhow::bail!("settings write-back changed typed Config sections");
    }
    Ok(())
}

/// Replace the owned bot provider/agent/auth block, preserving other typed values
/// and unknown fields outside auth. Comments and scalar presentation may change.
fn patch_bot_settings_yaml(
    original: &str,
    mut expected: crate::config::Config,
    bot_name: &str,
    owner_id_to_set: Option<u64>,
    settings: &DiscordBotSettings,
) -> anyhow::Result<String> {
    let mut document: serde_yaml::Value = serde_yaml::from_str(original)?;
    preserve_string_scalars(&mut document, &typed_document(&expected)?);

    let discord = yaml_child(&mut document, "discord");
    if let Some(owner_id) = owner_id_to_set {
        yaml_set(discord, "owner_id", serde_yaml::to_value(owner_id)?);
    }

    let bot = yaml_child(yaml_child(discord, "bots"), bot_name);
    yaml_set(
        bot,
        "provider",
        serde_yaml::to_value(settings.provider.as_str())?,
    );
    match settings.agent.as_deref() {
        Some(agent) => yaml_set(bot, "agent", serde_yaml::to_value(agent)?),
        None => yaml_remove(bot, "agent"),
    }

    let auth = crate::config::DiscordBotAuthConfig {
        allowed_channel_ids: Some(settings.allowed_channel_ids.clone()),
        require_mention_channel_ids: Some(settings.require_mention_channel_ids.clone()),
        allowed_user_ids: Some(settings.allowed_user_ids.clone()),
        allowed_tools: Some(normalize_allowed_tools(&settings.allowed_tools)),
        allow_all_users: Some(settings.allow_all_users),
        allowed_bot_ids: Some(settings.allowed_bot_ids.clone()),
    };
    yaml_set(bot, "auth", serde_yaml::to_value(&auth)?);

    if let Some(owner_id) = owner_id_to_set {
        expected.discord.owner_id = Some(owner_id);
    }
    let bot = expected
        .discord
        .bots
        .get_mut(bot_name)
        .ok_or_else(|| anyhow::anyhow!("configured bot missing"))?;
    bot.provider = Some(settings.provider.as_str().to_string());
    bot.agent = settings.agent.clone();
    bot.auth = auth;
    let rendered = serde_yaml::to_string(&document)?;
    check_typed_equivalence(&expected, &rendered)?;
    Ok(rendered)
}

fn persist_bot_auth_to_yaml_checked(
    token: &str,
    settings: &DiscordBotSettings,
) -> std::io::Result<()> {
    let Some(path) = super::config_path_for_write() else {
        return Ok(());
    };
    if !path.is_file() {
        // No file means no configured bot this write-back could own; the old
        // `Config::default()` branch reached the same early return below.
        return Ok(());
    }

    let original = fs::read_to_string(&path).map_err(|err| config_io_error(&path, err))?;
    let config: crate::config::Config =
        serde_yaml::from_str(&original).map_err(|err| config_io_error(&path, err))?;
    crate::config::validate_config(&config).map_err(|err| config_io_error(&path, err))?;

    let Some(bot_name) = super::resolved_config_bot_name(&config, token) else {
        // Do not mutate YAML for tokens that are not managed by agentdesk.yaml.
        // This prevents owner imprinting from an unconfigured bot from overwriting
        // the shared discord.owner_id used by configured bots.
        return Ok(());
    };

    // Keep the onboarding-configured owner stable; runtime settings should only
    // fill the owner when the YAML is still unset.
    let owner_id_to_set = match config.discord.owner_id {
        Some(_) => None,
        None => settings.owner_user_id,
    };

    let rendered = patch_bot_settings_yaml(&original, config, &bot_name, owner_id_to_set, settings)
        .map_err(|err| config_io_error(&path, err))?;
    write_bytes_atomically(&path, rendered.as_bytes())
}

fn save_runtime_bot_settings_checked(
    token: &str,
    settings: &DiscordBotSettings,
) -> std::io::Result<()> {
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
    let mut sorted_codex_goals_reset_pending: Vec<_> = settings
        .channel_codex_goals_reset_pending
        .iter()
        .cloned()
        .collect();
    sorted_codex_goals_reset_pending.sort();

    if yaml_manages_bot {
        if !settings.channel_model_overrides.is_empty()
            || !settings.channel_fast_modes.is_empty()
            || !sorted_fast_mode_reset_pending.is_empty()
            || !settings.channel_codex_goals.is_empty()
            || !sorted_codex_goals_reset_pending.is_empty()
            || !settings.channel_node_overrides.is_empty()
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
            if !settings.channel_codex_goals.is_empty() {
                runtime_entry.insert(
                    "channel_codex_goals".to_string(),
                    serde_json::json!(settings.channel_codex_goals),
                );
            }
            if !sorted_codex_goals_reset_pending.is_empty() {
                runtime_entry.insert(
                    "channel_codex_goals_reset_pending".to_string(),
                    serde_json::json!(sorted_codex_goals_reset_pending),
                );
            }
            if !settings.channel_node_overrides.is_empty() {
                runtime_entry.insert(
                    "channel_node_overrides".to_string(),
                    serde_json::json!(settings.channel_node_overrides),
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
        if settings.channel_codex_goals.is_empty() {
            entry.remove("channel_codex_goals");
        } else {
            entry.insert(
                "channel_codex_goals".to_string(),
                serde_json::json!(settings.channel_codex_goals),
            );
        }
        if sorted_codex_goals_reset_pending.is_empty() {
            entry.remove("channel_codex_goals_reset_pending");
        } else {
            entry.insert(
                "channel_codex_goals_reset_pending".to_string(),
                serde_json::json!(sorted_codex_goals_reset_pending),
            );
        }
        if settings.channel_node_overrides.is_empty() {
            entry.remove("channel_node_overrides");
        } else {
            entry.insert(
                "channel_node_overrides".to_string(),
                serde_json::json!(settings.channel_node_overrides),
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
mod yaml_write_back_secret_tests {
    use super::*;

    /// A settings YAML that carries both `#[serde(skip_serializing)]` secrets in
    /// the tree: `server.auth_token` (`config.rs:125`) and `discord.bots.*.token`
    /// (`config.rs:242`).
    const FIXTURE: &str = concat!(
        "server:\n",
        "  host: 0.0.0.0\n",
        "  port: 8791\n",
        "  auth_token: dashboard-secret-token\n",
        "  allow_insecure_nonloopback_bind: true\n",
        "discord:\n",
        "  bots:\n",
        "    main:\n",
        "      token: discord-bot-secret\n",
        "      provider: claude\n",
    );

    const BOT_TOKEN: &str = "discord-bot-secret";

    fn settings_with_new_allowlist() -> DiscordBotSettings {
        DiscordBotSettings {
            allowed_channel_ids: vec![4242],
            ..DiscordBotSettings::default()
        }
    }

    /// #5750 — a Discord settings write-back must not delete secrets it never
    /// read back. The whole-`Config` re-serialization it replaced dropped every
    /// `skip_serializing` field, which silently disarmed the `/ws` auth gate
    /// (`src/server/ws.rs:33`) without a restart.
    #[test]
    fn bot_settings_write_back_keeps_skip_serializing_secrets_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agentdesk.yaml");
        fs::write(&path, FIXTURE).expect("write fixture");
        let _config_env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_CONFIG", &path);

        persist_bot_auth_to_yaml_checked(BOT_TOKEN, &settings_with_new_allowlist())
            .expect("write-back should succeed");

        let reloaded = crate::config::load_from_path(&path).expect("reload written yaml");
        let bot = reloaded
            .discord
            .bots
            .get("main")
            .expect("bot entry survives the write-back");

        assert_eq!(
            bot.auth.allowed_channel_ids,
            Some(vec![4242]),
            "the write-back must actually apply the new allowlist"
        );
        assert_eq!(
            reloaded.server.auth_token.as_deref(),
            Some("dashboard-secret-token"),
            "#5750: server.auth_token must survive a Discord settings write-back"
        );
        assert_eq!(
            bot.token.as_deref(),
            Some(BOT_TOKEN),
            "#5750: discord.bots.*.token must survive a Discord settings write-back"
        );
    }

    /// Unmodelled keys survive, though YAML presentation may change.
    #[test]
    fn bot_settings_write_back_preserves_unowned_document_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agentdesk.yaml");
        fs::write(
            &path,
            format!("{FIXTURE}unmodelled_section:\n  keep_me: true\n"),
        )
        .expect("write fixture");
        let _config_env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_CONFIG", &path);

        persist_bot_auth_to_yaml_checked(BOT_TOKEN, &settings_with_new_allowlist())
            .expect("write-back should succeed");

        let rendered = fs::read_to_string(&path).expect("read written yaml");
        assert!(
            rendered.contains("keep_me"),
            "unmodelled keys must survive the write-back, got:\n{rendered}"
        );
    }

    /// #5071 T5 A6 — the mixed-version half of the same contract, pinned at the
    /// shape that actually threatens the #5464 relay-authority rollout dial.
    ///
    /// The test above covers a whole top-level section this binary does not
    /// model. The downgrade case is narrower and is NOT implied by it: a key a
    /// newer binary wrote inside a section this one DOES model, which this
    /// binary's typed `Config` parses past and drops (`RuntimeSettingsConfig`
    /// carries no `deny_unknown_fields`). Were the write-back rendering the
    /// typed tree, that key would vanish on the first Discord settings command
    /// an older binary served — silently returning the dial to `Legacy/0` with
    /// no config edit and no restart to point at. That is the loss #5464 T5 S1
    /// review follow-up 2 booked as "mixed-version whole-config rewrite loses
    /// the new dial keys", and PR #5803 (#5750) closed it by patching the
    /// parsed document instead of re-rendering it. This asserts that closure at
    /// the dial's own shape rather than inferring it from the top-level case.
    #[test]
    fn bot_settings_write_back_preserves_unknown_keys_inside_modelled_sections() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agentdesk.yaml");
        fs::write(
            &path,
            format!("{FIXTURE}runtime:\n  relay_authority_dial_from_a_newer_binary: enforce\n"),
        )
        .expect("write fixture");
        let _config_env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_CONFIG", &path);

        persist_bot_auth_to_yaml_checked(BOT_TOKEN, &settings_with_new_allowlist())
            .expect("write-back should succeed");

        let rendered = fs::read_to_string(&path).expect("read written yaml");
        let document: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("the written yaml parses");
        assert_eq!(
            document["runtime"]["relay_authority_dial_from_a_newer_binary"].as_str(),
            Some("enforce"),
            "a newer binary's key inside a modelled section must survive an older \
             binary's write-back, got:\n{rendered}"
        );
    }
    fn round_trip(original: &str) -> crate::config::Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        fs::write(&path, original).unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_CONFIG", &path);
        persist_bot_auth_to_yaml_checked(BOT_TOKEN, &settings_with_new_allowlist()).unwrap();
        let rendered = fs::read_to_string(path).unwrap();
        let raw: serde_yaml::Value = serde_yaml::from_str(&rendered).unwrap();
        let before: serde_yaml::Value = serde_yaml::from_str(original).unwrap();
        assert_eq!(
            raw["discord"]["bots"].as_mapping().unwrap().len(),
            before["discord"]["bots"].as_mapping().unwrap().len()
        );
        serde_yaml::from_str(&rendered).unwrap()
    }

    #[test]
    fn scalar_database_password_is_preserved() {
        let actual = round_trip(&format!("{FIXTURE}database:\n  password: 0x1234\n"));
        assert_eq!(actual.database.password.as_deref(), Some("0x1234"));
    }

    #[test]
    fn scalar_numeric_bot_key_patches_existing_entry() {
        let actual = round_trip(&FIXTURE.replace("main:", "123:"));
        assert_eq!(actual.discord.bots.len(), 1);
        assert_eq!(actual.discord.bots["123"].token.as_deref(), Some(BOT_TOKEN));
        assert_eq!(
            actual.discord.bots["123"].auth.allowed_channel_ids,
            Some(vec![4242])
        );
    }

    #[test]
    fn scalar_colliding_mcp_keys_keep_distinct_entries() {
        for entries in [
            "  '0x7b': {url: 'https://example.invalid/hex'}\n  123: {url: 'https://example.invalid/decimal'}\n",
            "  123: {url: 'https://example.invalid/decimal'}\n  '0x7b': {url: 'https://example.invalid/hex'}\n",
        ] {
            let actual = round_trip(&format!("{FIXTURE}mcp_servers:\n{entries}"));
            assert_eq!(actual.mcp_servers.len(), 2);
            assert_eq!(
                actual.mcp_servers["0x7b"].url,
                "https://example.invalid/hex"
            );
            assert_eq!(
                actual.mcp_servers["123"].url,
                "https://example.invalid/decimal"
            );
            assert_eq!(
                actual.discord.bots["main"].auth.allowed_channel_ids,
                Some(vec![4242])
            );
        }
    }

    #[test]
    fn invalid_config_is_not_written_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        let original = format!("{FIXTURE}escalation:\n  schedule:\n    timezone: Invalid/Zone\n");
        fs::write(&path, &original).unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_CONFIG", &path);
        assert!(
            persist_bot_auth_to_yaml_checked(BOT_TOKEN, &settings_with_new_allowlist()).is_err()
        );
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn scalar_server_token_is_preserved() {
        for token in ["0777", "0x0777"] {
            let actual = round_trip(&FIXTURE.replace("dashboard-secret-token", token));
            assert_eq!(actual.server.auth_token.as_deref(), Some(token));
        }
    }

    #[test]
    fn scalar_all_unowned_typed_values_are_equal() {
        let original = format!("{FIXTURE}database:\n  password: 0x1234\nshared_prompt: 1e3\n");
        let mut expected: crate::config::Config = serde_yaml::from_str(&original).unwrap();
        let actual = round_trip(&original);
        let settings = settings_with_new_allowlist();
        let bot = expected.discord.bots.get_mut("main").unwrap();
        bot.provider = Some(settings.provider.as_str().to_string());
        bot.agent = settings.agent.clone();
        bot.auth = crate::config::DiscordBotAuthConfig {
            allowed_channel_ids: Some(settings.allowed_channel_ids),
            require_mention_channel_ids: Some(settings.require_mention_channel_ids),
            allowed_user_ids: Some(settings.allowed_user_ids),
            allowed_tools: Some(normalize_allowed_tools(&settings.allowed_tools)),
            allow_all_users: Some(settings.allow_all_users),
            allowed_bot_ids: Some(settings.allowed_bot_ids),
        };
        assert_eq!(expected.server.auth_token, actual.server.auth_token);
        assert_eq!(
            expected.discord.bots["main"].token,
            actual.discord.bots["main"].token
        );
        assert_eq!(
            serde_yaml::to_value(expected).unwrap(),
            serde_yaml::to_value(actual).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn secret_write_back_keeps_owner_only_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        fs::write(&path, FIXTURE).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_CONFIG", &path);
        persist_bot_auth_to_yaml_checked(BOT_TOKEN, &settings_with_new_allowlist()).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn typed_gate_rejects_scalar_corruption() {
        let original = format!("{FIXTURE}database:\n  password: 0x1234\n");
        let expected: crate::config::Config = serde_yaml::from_str(&original).unwrap();
        let raw: serde_yaml::Value = serde_yaml::from_str(&original).unwrap();
        let corrupted = serde_yaml::to_string(&raw).unwrap();
        let error = check_typed_equivalence(&expected, &corrupted)
            .unwrap_err()
            .to_string();
        assert!(error.contains("database"));
        assert!(!error.contains("4660") && !error.contains("0x1234"));
    }
    #[test]
    fn typed_gate_blocks_mismatched_snapshot_before_render_returns() {
        let original = format!("{FIXTURE}database:\n  password: 0x1234\n");
        let mut expected: crate::config::Config = serde_yaml::from_str(&original).unwrap();
        expected.server.port += 1;
        assert!(
            patch_bot_settings_yaml(
                &original,
                expected,
                "main",
                None,
                &settings_with_new_allowlist()
            )
            .is_err()
        );
    }
}
