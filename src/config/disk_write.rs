//! Whole-`Config` disk rendering: `Serialize` omits `server.auth_token` and
//! `discord.bots.*.token`, so writes to the YAML file that holds them put them back.

use super::{Config, McpServerConfig};
use anyhow::{Context, Result, bail};
use serde_yaml::Value;
use std::path::{Path, PathBuf};

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
        onboarding,
        memory,
        mcp,
        prompt_manifest_retention,
        tui_o,
        session_hosts,
        config_hot_reload
    )
}

fn has_secret(token: &Option<String>) -> bool {
    token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty())
}

/// The file's own `Config`, before `load` adds runtime defaults; `None` when absent.
fn read_config_on_disk(path: &Path) -> Result<Option<Config>> {
    let existing = match std::fs::read_to_string(path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read config {}", path.display()));
        }
    };
    serde_yaml::from_str(&existing)
        .map(Some)
        .with_context(|| format!("Failed to parse existing config {}", path.display()))
}

/// Drops the MCP server `load` injected: the file never named it and it is unchanged.
fn drop_injected_mcp_server(
    config: &mut Config,
    file: &Config,
    injected: Option<(String, McpServerConfig)>,
) {
    if let Some((name, server)) = injected
        && !file.mcp_servers.contains_key(&name)
        && config.mcp_servers.get(&name) == Some(&server)
    {
        config.mcp_servers.remove(&name);
    }
}

/// `config` without what `load` derived at runtime, so a save writes the file's own form
/// back and later default changes still apply.
fn without_runtime_derivations(path: &Path, config: &Config, on_disk: Option<&Config>) -> Config {
    let mut config = config.clone();
    let file = on_disk.cloned().unwrap_or_default();
    drop_injected_mcp_server(&mut config, &file, super::default_memento_mcp_server());
    let Some(root) = super::runtime_root_for_config_path(path) else {
        return config;
    };
    let resolves_to =
        |raw: &PathBuf, value: &PathBuf| super::resolve_runtime_path(&root, raw) == *value;
    for (value, raw) in [
        (&mut config.policies.dir, &file.policies.dir),
        (&mut config.data.dir, &file.data.dir),
        (&mut config.routines.dir, &file.routines.dir),
    ] {
        if resolves_to(raw, value) {
            *value = raw.clone();
        }
    }
    let (raw_dirs, dirs) = (
        &file.routines.additional_dirs,
        &config.routines.additional_dirs,
    );
    if raw_dirs.len() == dirs.len()
        && raw_dirs
            .iter()
            .zip(dirs)
            .all(|(raw, value)| resolves_to(raw, value))
    {
        config.routines.additional_dirs = raw_dirs.clone();
    }
    config
}

/// Refuse to drop a secret the file holds while its owner (the server, or the same
/// bot name) is still written: that `Config` came from a source that lost it.
fn check_disk_secrets_kept(path: &Path, config: &Config, on_disk: Option<&Config>) -> Result<()> {
    let Some(on_disk) = on_disk else {
        return Ok(());
    };
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
    let on_disk = read_config_on_disk(path)?;
    let config = &without_runtime_derivations(path, config, on_disk.as_ref());
    let rendered = serde_yaml::to_string(&disk_document(config)?)
        .with_context(|| format!("Failed to serialize config for {}", path.display()))?;
    check_reads_back(config, &rendered)
        .map_err(|error| anyhow::anyhow!("Refusing to write config {}: {error}", path.display()))?;
    check_disk_secrets_kept(path, config, on_disk.as_ref())?;
    Ok(rendered)
}

pub fn save_to_path(path: &Path, config: &Config) -> Result<()> {
    let rendered = render_config_for_path(path, config)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if super::config_contains_file_secrets(config) {
        crate::utils::secret_file::replace_secret_file(path, rendered)
            .with_context(|| format!("Failed to write config {}", path.display()))?;
    } else {
        replace_file(path, &rendered)
            .with_context(|| format!("Failed to write config {}", path.display()))?;
    }
    Ok(())
}

/// Renames a fully written sibling over `path` (through a symlink), keeping its mode,
/// so a crash leaves the old file or the new one, never a truncated one.
fn replace_file(path: &Path, contents: &str) -> std::io::Result<()> {
    let target = crate::utils::secret_file::replacement_target(path)?;
    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::Builder::new()
        .prefix(".agentdesk-config-")
        .tempfile_in(dir)?;
    match std::fs::metadata(&target) {
        Ok(existing) => temp.as_file().set_permissions(existing.permissions())?,
        #[cfg(unix)]
        Err(_) => {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o644))?;
        }
        #[cfg(not(unix))]
        Err(_) => {}
    }
    std::io::Write::write_all(&mut temp, contents.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(&target).map(drop).map_err(|error| error.error)
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

    #[cfg(unix)]
    #[test]
    fn save_replaces_the_file_instead_of_rewriting_it_in_place() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        write_secret_config(&path, "");
        let before = std::fs::metadata(&path).unwrap().ino();
        let mut config = crate::config::load_from_path(&path).unwrap();
        config.shared_prompt = Some("/prompts/shared.md".to_string());

        save_to_path(&path, &config).unwrap();

        assert_ne!(std::fs::metadata(&path).unwrap().ino(), before);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["agentdesk.yaml"], "no temp file may be left behind");
        assert_secrets_on_disk(&path);
    }

    #[test]
    fn save_writes_back_the_relative_dirs_the_file_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        write_secret_config(&path, "policies:\n  dir: policies\ndata:\n  dir: data\n");
        let mut config = crate::config::load_from_path(&path).unwrap();
        assert!(
            config.data.dir.is_absolute(),
            "load resolves the dir against the runtime root"
        );
        config.shared_prompt = Some("/prompts/shared.md".to_string());

        save_to_path(&path, &config).unwrap();

        let raw: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["policies"]["dir"].as_str(), Some("policies"));
        assert_eq!(raw["data"]["dir"].as_str(), Some("data"));
        assert_eq!(
            crate::config::load_from_path(&path).unwrap().data.dir,
            config.data.dir
        );
    }

    #[test]
    fn an_unchanged_injected_mcp_server_is_not_written() {
        let server = McpServerConfig {
            url: "https://memento.example/mcp".to_string(),
            auth: None,
        };
        let mut edited = server.clone();
        edited.url = "https://elsewhere.example/mcp".to_string();
        let injected = || Some(("memento".to_string(), server.clone()));
        let (unnamed, mut named) = (Config::default(), Config::default());
        named
            .mcp_servers
            .insert("memento".to_string(), server.clone());
        let mut config = Config::default();

        config
            .mcp_servers
            .insert("memento".to_string(), server.clone());
        drop_injected_mcp_server(&mut config, &named, injected());
        assert_eq!(config.mcp_servers.get("memento"), Some(&server));
        drop_injected_mcp_server(&mut config, &unnamed, injected());
        assert!(config.mcp_servers.is_empty());

        config
            .mcp_servers
            .insert("memento".to_string(), edited.clone());
        drop_injected_mcp_server(&mut config, &unnamed, injected());
        assert_eq!(config.mcp_servers.get("memento"), Some(&edited));
    }

    #[test]
    fn a_save_writes_back_only_the_memento_server_the_file_names() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _key = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "MEMENTO_ACCESS_KEY",
            std::ffi::OsStr::new("memento-test-key"),
        );
        let (_, default) = crate::config::default_memento_mcp_server().expect("key is set");
        let dir = tempfile::tempdir().unwrap();
        let saved_url = |case: &str, extra: &str| {
            let path = dir.path().join(case).join("agentdesk.yaml");
            write_secret_config(&path, extra);
            let mut config = crate::config::load_from_path(&path).unwrap();
            config.shared_prompt = Some("/prompts/shared.md".to_string());
            save_to_path(&path, &config).unwrap();
            let raw: serde_yaml::Value =
                serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            raw["mcp_servers"]["memento"]["url"]
                .as_str()
                .map(str::to_string)
        };

        assert_eq!(saved_url("injected", ""), None);
        let written = format!(
            "mcp_servers:\n  memento:\n    url: {}\n    auth:\n      type: bearer\n      token_env_var: MEMENTO_ACCESS_KEY\n",
            default.url
        );
        assert_eq!(saved_url("written", &written), Some(default.url.clone()));
        let edited = "mcp_servers:\n  memento:\n    url: https://elsewhere.example/mcp\n";
        assert_eq!(
            saved_url("edited", edited).as_deref(),
            Some("https://elsewhere.example/mcp")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_save_through_a_dangling_symlink_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (path, missing) = (
            dir.path().join("agentdesk.yaml"),
            dir.path().join("gone.yaml"),
        );
        std::os::unix::fs::symlink(&missing, &path).unwrap();

        save_to_path(&path, &Config::default()).expect_err("a dangling symlink is not replaced");

        let link = std::fs::symlink_metadata(&path).unwrap();
        assert!(link.file_type().is_symlink());
        assert!(!missing.exists());
    }
}
