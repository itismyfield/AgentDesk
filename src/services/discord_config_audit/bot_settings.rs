//! Role-map and legacy bot-settings audit: migrates what `config.yaml` lacks.

use super::*;

pub(super) fn audit_role_map(
    config: &Config,
    legacy_scan: &LegacySourceScan,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) {
    let Some(role_map_json) = legacy_scan.role_map_json.as_ref() else {
        return;
    };

    let mut preview = Config::default();
    let changed = runtime_layout::preview_role_map_merge(&mut preview, role_map_json);

    if dry_run {
        if let Some(path) = legacy_scan.role_map_path.as_ref() {
            let verb = if changed {
                "would migrate"
            } else {
                "would retire"
            };
            report.actions.push(format!(
                "{} legacy role map '{}' with agentdesk.yaml as the winning source-of-truth",
                verb,
                path.display()
            ));
        }
    }

    for legacy_agent in &preview.agents {
        let Some(yaml_agent) = config
            .agents
            .iter()
            .find(|agent| agent.id == legacy_agent.id)
        else {
            report.warnings.push(format!(
                "Legacy role_map defines agent '{}' but agentdesk.yaml does not contain it after startup migration",
                legacy_agent.id
            ));
            continue;
        };

        let differing_fields = compare_agents(yaml_agent, legacy_agent);
        if differing_fields.is_empty() {
            continue;
        }

        report.warnings.push(format!(
            "Legacy role_map defines agent '{}' differently on {}; agentdesk.yaml wins",
            legacy_agent.id,
            differing_fields.join(", ")
        ));
    }
}

pub(super) fn audit_bot_settings(
    root: &Path,
    config: &mut Config,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> bool {
    let path = runtime_layout::config_dir(root).join("bot_settings.json");
    if !path.is_file() {
        return false;
    }

    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            report.warnings.push(format!(
                "Failed to read legacy bot settings '{}': {err}",
                path.display()
            ));
            return false;
        }
    };

    let json = match serde_json::from_str::<Value>(&content) {
        Ok(json) => json,
        Err(err) => {
            report.warnings.push(format!(
                "Failed to parse legacy bot settings '{}': {err}",
                path.display()
            ));
            return false;
        }
    };

    let Some(obj) = json.as_object() else {
        report.warnings.push(format!(
            "Legacy bot settings '{}' is not a JSON object",
            path.display()
        ));
        return false;
    };

    let mut changed = false;
    let mut runtime_only_entries = Map::new();
    let mut owner_candidates = BTreeSet::new();
    let mut saw_migratable_data = false;
    let mut saw_runtime_rewrite = false;

    for (entry_key, entry_value) in obj {
        let legacy = parse_legacy_bot_entry(entry_key, entry_value);
        if !legacy.channel_model_overrides.is_empty()
            || !legacy.channel_fast_modes.is_empty()
            || !legacy.channel_fast_mode_reset_pending.is_empty()
            || !legacy.channel_codex_goals.is_empty()
            || !legacy.channel_codex_goals_reset_pending.is_empty()
        {
            let mut runtime_entry = Map::new();
            if !legacy.channel_model_overrides.is_empty() {
                runtime_entry.insert(
                    "channel_model_overrides".to_string(),
                    serde_json::json!(legacy.channel_model_overrides),
                );
            }
            if !legacy.channel_fast_modes.is_empty() {
                runtime_entry.insert(
                    "channel_fast_modes".to_string(),
                    serde_json::json!(legacy.channel_fast_modes),
                );
            }
            if !legacy.channel_fast_mode_reset_pending.is_empty() {
                runtime_entry.insert(
                    "channel_fast_mode_reset_pending".to_string(),
                    serde_json::json!(legacy.channel_fast_mode_reset_pending),
                );
            }
            if !legacy.channel_codex_goals.is_empty() {
                runtime_entry.insert(
                    "channel_codex_goals".to_string(),
                    serde_json::json!(legacy.channel_codex_goals),
                );
            }
            if !legacy.channel_codex_goals_reset_pending.is_empty() {
                runtime_entry.insert(
                    "channel_codex_goals_reset_pending".to_string(),
                    serde_json::json!(legacy.channel_codex_goals_reset_pending),
                );
            }
            runtime_only_entries.insert(legacy.hash_key.clone(), Value::Object(runtime_entry));
        }

        if !legacy.has_migratable_fields() {
            continue;
        }

        saw_migratable_data = true;

        let Some(token) = legacy.token.as_deref() else {
            report.warnings.push(format!(
                "Legacy bot_settings entry '{}' has auth data but no token; skipping migration",
                legacy.hash_key
            ));
            continue;
        };

        let Some(bot_name) = resolved_config_bot_name(config, token) else {
            report.warnings.push(format!(
                "Legacy bot_settings entry '{}' does not match any bot token in agentdesk.yaml; skipping migration",
                legacy.hash_key
            ));
            continue;
        };

        if let Some(owner_user_id) = legacy.owner_user_id {
            owner_candidates.insert(owner_user_id);
        }

        let Some(bot) = config.discord.bots.get_mut(&bot_name) else {
            continue;
        };

        changed |= migrate_provider(&legacy, bot, dry_run, report);
        changed |= migrate_agent(&legacy, bot, dry_run, report);
        changed |= migrate_u64_list(
            &legacy.hash_key,
            "allowed_channel_ids",
            &legacy.allowed_channel_ids,
            &mut bot.auth.allowed_channel_ids,
            dry_run,
            report,
        );
        changed |= migrate_u64_list(
            &legacy.hash_key,
            "allowed_user_ids",
            &legacy.allowed_user_ids,
            &mut bot.auth.allowed_user_ids,
            dry_run,
            report,
        );
        changed |= migrate_tool_list(
            &legacy.hash_key,
            &legacy.allowed_tools,
            &mut bot.auth.allowed_tools,
            dry_run,
            report,
        );
        changed |= migrate_bool_field(
            &legacy.hash_key,
            "allow_all_users",
            legacy.allow_all_users,
            &mut bot.auth.allow_all_users,
            dry_run,
            report,
        );
        changed |= migrate_u64_list(
            &legacy.hash_key,
            "allowed_bot_ids",
            &legacy.allowed_bot_ids,
            &mut bot.auth.allowed_bot_ids,
            dry_run,
            report,
        );
    }

    if config.discord.owner_id.is_none() {
        match owner_candidates.len() {
            0 => {}
            1 => {
                let owner_user_id = *owner_candidates.iter().next().unwrap();
                report.actions.push(format!(
                    "{} discord.owner_id={} from legacy bot_settings.json",
                    dry_run_action_prefix(dry_run),
                    owner_user_id
                ));
                if !dry_run {
                    config.discord.owner_id = Some(owner_user_id);
                }
                changed = true;
            }
            _ => report.warnings.push(format!(
                "Legacy bot_settings.json contains multiple owner_user_id candidates ({}) while agentdesk.yaml is unset; refusing automatic migration",
                owner_candidates
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    } else if let Some(owner_user_id) = config.discord.owner_id {
        for candidate in owner_candidates {
            if candidate != owner_user_id {
                report.warnings.push(format!(
                    "Legacy bot_settings.json owner_user_id={} conflicts with agentdesk.yaml owner_id={}; agentdesk.yaml wins",
                    candidate, owner_user_id
                ));
            }
        }
    }

    let desired_runtime_value = Value::Object(runtime_only_entries.clone());
    let desired_runtime_rendered =
        serde_json::to_string_pretty(&desired_runtime_value).unwrap_or_else(|_| "{}".to_string());
    if Value::Object(obj.clone()) != desired_runtime_value {
        saw_runtime_rewrite = true;
    }

    if runtime_only_entries.is_empty() {
        if saw_migratable_data || saw_runtime_rewrite {
            let migrated = path.with_extension("json.migrated");
            report.actions.push(format!(
                "{} legacy bot_settings '{}' after migrating runtime auth into agentdesk.yaml",
                if dry_run { "would retire" } else { "retired" },
                path.display()
            ));
            if !dry_run {
                match fs::rename(&path, &migrated) {
                    Ok(()) => {}
                    Err(err) => report.warnings.push(format!(
                        "Failed to retire legacy bot settings '{}' → '{}': {err}",
                        path.display(),
                        migrated.display()
                    )),
                }
            }
        }
    } else if saw_migratable_data || saw_runtime_rewrite {
        report.actions.push(format!(
            "{} '{}' to runtime-only channel settings entries",
            if dry_run { "would rewrite" } else { "rewrote" },
            path.display()
        ));
        if !dry_run {
            if let Err(err) = atomic_write(&path, &desired_runtime_rendered) {
                report.warnings.push(format!(
                    "Failed to rewrite legacy bot settings '{}': {err}",
                    path.display()
                ));
            }
        }
    }

    changed
}
