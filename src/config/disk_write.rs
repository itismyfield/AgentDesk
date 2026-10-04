//! Whole-`Config` disk rendering: `Serialize` omits `server.auth_token` and
//! `discord.bots.*.token`, so writes to the YAML file that holds them put them back.

use super::Config;
use anyhow::{Context, Result, bail};
use serde_yaml::Value;
use std::path::Path;

/// Mutable entry for `key` under `parent`, created as null when absent.
/// A `parent` that is not a mapping is replaced with an empty one.
pub(crate) fn yaml_child<'a>(parent: &'a mut Value, key: &str) -> &'a mut Value {
    if !parent.is_mapping() {
        *parent = Value::Mapping(serde_yaml::Mapping::new());
    }
    let key = Value::String(key.to_string());
    let map = parent
        .as_mapping_mut()
        .expect("value was just normalized to a mapping");
    if !map.contains_key(&key) {
        map.insert(key.clone(), Value::Null);
    }
    map.get_mut(&key).expect("entry exists after the insert")
}

pub(crate) fn yaml_set(parent: &mut Value, key: &str, value: Value) {
    *yaml_child(parent, key) = value;
}

// Local comparison only: include the two secrets deliberately omitted by Serialize.
// Never log this projection or differences containing its values.
pub(crate) fn typed_document(config: &Config) -> Result<Value, serde_yaml::Error> {
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

/// The serialized config plus the secrets it holds; absent secrets add no keys.
fn disk_document(config: &Config) -> Result<Value, serde_yaml::Error> {
    let mut value = serde_yaml::to_value(config)?;
    if let Some(token) = &config.server.auth_token {
        yaml_set(
            yaml_child(&mut value, "server"),
            "auth_token",
            Value::String(token.clone()),
        );
    }
    for (name, bot) in &config.discord.bots {
        if let Some(token) = &bot.token {
            let bots = yaml_child(yaml_child(&mut value, "discord"), "bots");
            yaml_set(
                yaml_child(bots, name),
                "token",
                Value::String(token.clone()),
            );
        }
    }
    Ok(value)
}

/// Settings write-back gate: the patched document must serialize like `expected`,
/// secrets included. The error names the section only, never a value.
pub(crate) fn check_round_trip(expected: &Config, rendered: &str) -> Result<()> {
    let actual: Config = serde_yaml::from_str(rendered)?;
    let expected = typed_document(expected)?;
    let actual = typed_document(&actual)?;
    if actual == expected {
        return Ok(());
    }
    let section = expected
        .as_mapping()
        .and_then(|sections| {
            sections
                .iter()
                .find(|(key, value)| actual.get(*key) != Some(*value))
        })
        .and_then(|(key, _)| key.as_str())
        .unwrap_or("unknown");
    bail!("config write would change typed section {section}")
}

/// Refuse a render that would not read back as exactly `expected`, including values
/// a `skip_serializing_if` omits. The error names the top-level field only.
fn check_reads_back(expected: &Config, rendered: &str) -> Result<()> {
    let actual: Config = serde_yaml::from_str(rendered)?;
    if actual == *expected {
        return Ok(());
    }
    let section = differing_section(expected, &actual);
    bail!("config write would change typed section {section}")
}

/// First differing top-level field; the destructure stops compiling when one is added.
fn differing_section(expected: &Config, actual: &Config) -> &'static str {
    macro_rules! first_difference {
        ($($field:ident),+) => {{
            let Config { $($field: _),+ } = expected;
            $(if expected.$field != actual.$field {
                return stringify!($field);
            })+
            "unknown"
        }};
    }
    first_difference!(
        server,
        discord,
        providers,
        voice,
        shared_prompt,
        mcp_servers,
        review_mcp_allowlist,
        agents,
        meeting,
        github,
        policies,
        data,
        database,
        cluster,
        kanban,
        review,
        placeholder,
        runtime,
        routines,
        escalation,
        onboarding,
        memory,
        mcp,
        prompt_manifest_retention,
        tui_o,
        config_hot_reload
    )
}

fn has_secret(token: &Option<String>) -> bool {
    token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty())
}

/// Refuse to drop a secret the file holds while its owner (the server, or the same
/// bot name) is still written: that `Config` came from a source that lost it.
fn check_disk_secrets_kept(path: &Path, config: &Config) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read config {}", path.display()));
        }
    };
    let on_disk: Config = serde_yaml::from_str(&existing)
        .with_context(|| format!("Failed to parse existing config {}", path.display()))?;
    if has_secret(&on_disk.server.auth_token) && !has_secret(&config.server.auth_token) {
        bail!(
            "config write would drop server.auth_token from {}",
            path.display()
        );
    }
    for (name, bot) in &on_disk.discord.bots {
        if has_secret(&bot.token)
            && config
                .discord
                .bots
                .get(name)
                .is_some_and(|next| !has_secret(&next.token))
        {
            bail!(
                "config write would drop discord.bots.{name}.token from {}",
                path.display()
            );
        }
    }
    Ok(())
}

/// YAML text for writing `config` to `path`, checked before anything is written.
pub(crate) fn render_config_for_path(path: &Path, config: &Config) -> Result<String> {
    let rendered = serde_yaml::to_string(&disk_document(config)?)
        .with_context(|| format!("Failed to serialize config for {}", path.display()))?;
    check_reads_back(config, &rendered)
        .map_err(|error| anyhow::anyhow!("Refusing to write config {}: {error}", path.display()))?;
    check_disk_secrets_kept(path, config)?;
    Ok(rendered)
}

pub fn save_to_path(path: &Path, config: &Config) -> Result<()> {
    let rendered = render_config_for_path(path, config)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if super::config_contains_file_secrets(config) {
        crate::utils::secret_file::write_secret_file(path, rendered)
            .with_context(|| format!("Failed to write config {}", path.display()))?;
    } else {
        std::fs::write(path, rendered)
            .with_context(|| format!("Failed to write config {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    pub(crate) const SERVER_TOKEN: &str = "server-secret-token";
    pub(crate) const BOT_TOKEN: &str = "keeper-bot-secret-token";

    /// Writes a config holding both secrets, followed by `extra` top-level YAML.
    pub(crate) fn write_secret_config(path: &Path, extra: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let yaml = format!(
            "server:\n  auth_token: {SERVER_TOKEN}\ndiscord:\n  bots:\n    keeper:\n      token: {BOT_TOKEN}\n      provider: claude\n{extra}"
        );
        std::fs::write(path, yaml).unwrap();
    }

    pub(crate) fn assert_secrets_on_disk(path: &Path) {
        let raw: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(raw["server"]["auth_token"].as_str(), Some(SERVER_TOKEN));
        assert_eq!(
            raw["discord"]["bots"]["keeper"]["token"].as_str(),
            Some(BOT_TOKEN)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn round_trip_gate_refuses_a_render_that_drops_secrets_without_echoing_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        write_secret_config(&path, "");
        let expected = crate::config::load_from_path(&path).unwrap();
        let stripped = serde_yaml::to_string(&expected).unwrap();

        let error = check_reads_back(&expected, &stripped)
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("would change typed section server"),
            "{error}"
        );
        assert!(!error.contains(SERVER_TOKEN) && !error.contains(BOT_TOKEN));
    }

    #[test]
    fn save_refuses_to_drop_channel_settings_the_yaml_form_omits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        write_secret_config(
            &path,
            "agents:\n  - id: retained\n    name: Retained\n    channels:\n      future:\n        model: keep-model\n        workspace: keep-workspace\n",
        );
        let original = std::fs::read(&path).unwrap();
        let mut config = crate::config::load_from_path(&path).unwrap();
        config.shared_prompt = Some("/prompts/shared.md".to_string());

        let error = format!("{:#}", save_to_path(&path, &config).unwrap_err());

        assert!(error.contains("typed section agents"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn save_refuses_a_config_that_lost_a_secret_the_file_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        write_secret_config(&path, "");
        let original = std::fs::read(&path).unwrap();
        let loaded = crate::config::load_from_path(&path).unwrap();

        let mut no_server_token = loaded.clone();
        no_server_token.server.auth_token = None;
        let mut no_bot_token = loaded.clone();
        no_bot_token.discord.bots.get_mut("keeper").unwrap().token = None;
        for config in [no_server_token, no_bot_token] {
            let error = format!("{:#}", save_to_path(&path, &config).unwrap_err());
            assert!(error.contains("would drop"), "{error}");
            assert!(!error.contains(SERVER_TOKEN) && !error.contains(BOT_TOKEN));
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }

        let mut bot_removed = loaded;
        bot_removed.discord.bots.remove("keeper");
        save_to_path(&path, &bot_removed).unwrap();
        let saved = crate::config::load_from_path(&path).unwrap();
        assert_eq!(saved.server.auth_token.as_deref(), Some(SERVER_TOKEN));
        assert!(saved.discord.bots.is_empty());
    }
}
