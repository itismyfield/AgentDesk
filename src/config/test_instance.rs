use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

const MARKER: &str = "AGENTDESK_TEST_MANIFEST";
static INSTANCE: OnceLock<Result<TestInstance, String>> = OnceLock::new();

#[derive(Debug)]
pub(crate) struct TestInstance {
    pub(crate) root: PathBuf,
    pub(crate) instance_id: String,
    pub(crate) tmux: TmuxHost,
    pub(crate) herdr: HerdrHost,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TmuxHost {
    pub(crate) socket: PathBuf,
    pub(crate) tmpdir: PathBuf,
    pub(crate) prefix: String,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct HerdrHost {
    pub(crate) socket: PathBuf,
    pub(crate) home: PathBuf,
    pub(crate) session: String,
    pub(crate) endpoint_key: String,
}

pub(crate) fn requested() -> bool {
    INSTANCE.get().is_some() || std::env::var_os(MARKER).is_some()
}

pub(crate) fn initialize() -> Result<(), String> {
    if !requested() {
        return Ok(());
    }
    INSTANCE
        .get_or_init(|| {
            let env_path = |key| {
                std::env::var_os(key)
                    .map(PathBuf::from)
                    .ok_or_else(rejected)
            };
            let instance = load(
                &env_path(MARKER)?,
                &env_path("AGENTDESK_ROOT_DIR")?,
                &env_path("AGENTDESK_CONFIG")?,
            )?;
            crate::domain::provider::naming::install_test_prefix(&instance.tmux.prefix)?;
            Ok(instance)
        })
        .as_ref()
        .map(|_| ())
        .map_err(Clone::clone)
}

pub(crate) fn current() -> Result<Option<&'static TestInstance>, String> {
    match INSTANCE.get() {
        Some(result) => result.as_ref().map(Some).map_err(Clone::clone),
        None if requested() => Err(rejected()),
        None => Ok(None),
    }
}

fn rejected() -> String {
    "test instance host declaration rejected".to_string()
}

fn checked_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path.components().any(|part| {
            !matches!(
                part,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
    {
        return Err(rejected());
    }
    let parts: Vec<_> = path.components().collect();
    if parts.windows(2).any(|pair| {
        pair[0]
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(".adk")
            && pair[1]
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case("release")
    }) {
        return Err(rejected());
    }
    for ancestor in path.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(rejected()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(rejected()),
        }
    }
    Ok(())
}

fn read_json(path: &Path) -> Result<Value, String> {
    checked_path(path)?;
    if !std::fs::metadata(path).map_err(|_| rejected())?.is_file() {
        return Err(rejected());
    }
    serde_json::from_slice(&std::fs::read(path).map_err(|_| rejected())?).map_err(|_| rejected())
}

fn load(path: &Path, root: &Path, config_path: &Path) -> Result<TestInstance, String> {
    checked_path(root)?;
    if std::fs::canonicalize(root).map_err(|_| rejected())? != root
        || path != root.join("manifest.json")
        || config_path != root.join("config/agentdesk.yaml")
    {
        return Err(rejected());
    }
    let manifest = read_json(path)?;
    let keys = [
        "schema_version",
        "status",
        "live_ready",
        "instance_id",
        "root",
        "worktree",
        "head",
        "profile",
        "applied_env",
        "api",
        "postgres",
        "config_path",
        "execution_node_id",
        "hosts",
        "preferred_node_seed_required",
    ];
    if manifest.as_object().is_none_or(|object| {
        object.len() != keys.len() || keys.iter().any(|key| !object.contains_key(*key))
    }) || manifest["hosts"]
        .as_object()
        .is_none_or(|hosts| hosts.len() != 2)
    {
        return Err(rejected());
    }
    let id = manifest["instance_id"].as_str().ok_or_else(rejected)?;
    if !uuid::Uuid::parse_str(id).is_ok_and(|id_value| id_value.hyphenated().to_string() == id)
        || root.file_name() != Some(std::ffi::OsStr::new(id))
        || manifest["root"] != root.to_string_lossy().as_ref()
        || manifest["config_path"] != config_path.to_string_lossy().as_ref()
        || manifest["schema_version"] != 1
        || manifest["execution_node_id"] != id
        || manifest["live_ready"] != false
        || manifest["status"] != "DECLARED"
    {
        return Err(rejected());
    }
    let tmux: TmuxHost =
        serde_json::from_value(manifest["hosts"]["tmux"].clone()).map_err(|_| rejected())?;
    let herdr: HerdrHost =
        serde_json::from_value(manifest["hosts"]["herdr"].clone()).map_err(|_| rejected())?;
    if tmux.socket != root.join("runtime/tmux/socket")
        || tmux.tmpdir != root.join("runtime/tmux")
        || tmux.prefix != format!("AdkTest-{id}")
        || herdr.socket != root.join("runtime/herdr/socket")
        || herdr.home != root.join("herdr-home")
        || herdr.session != format!("adk-test-{id}")
        || herdr.endpoint_key != format!("test-{id}")
    {
        return Err(rejected());
    }
    for target in [&tmux.socket, &tmux.tmpdir, &herdr.socket, &herdr.home] {
        checked_path(target)?;
    }
    let mut receipt = serde_json::Map::new();
    for key in [
        "schema_version",
        "status",
        "live_ready",
        "instance_id",
        "root",
        "worktree",
        "head",
        "api",
        "postgres",
        "applied_env",
        "execution_node_id",
        "preferred_node_seed_required",
        "hosts",
    ] {
        receipt.insert(key.into(), manifest.get(key).ok_or_else(rejected)?.clone());
    }
    for key in ["features", "bots", "channels"] {
        receipt.insert(
            key.into(),
            manifest["profile"].get(key).ok_or_else(rejected)?.clone(),
        );
    }
    let config = read_json(config_path)?;
    if read_json(&root.join("receipt.json"))? != Value::Object(receipt)
        || config["cluster"]["instance_id"] != id
        || config["cluster"]["gateway_preferred_instance_id"] != id
    {
        return Err(rejected());
    }
    Ok(TestInstance {
        root: root.into(),
        instance_id: id.into(),
        tmux,
        herdr,
    })
}

pub(crate) fn configure_tmux_command(command: &mut Command) -> io::Result<()> {
    if let Some(instance) = current().map_err(io::Error::other)? {
        command
            .env(
                "AGENTDESK_TEST_MANIFEST",
                instance.root.join("manifest.json"),
            )
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env("TMUX_TMPDIR", &instance.tmux.tmpdir)
            .arg("-S")
            .arg(&instance.tmux.socket);
    }
    Ok(())
}

pub(crate) fn validate_tmux_target(target: &str) -> io::Result<()> {
    if let Some(instance) = current().map_err(io::Error::other)? {
        if target != instance.tmux.prefix
            && !target.starts_with(&format!("{}-", instance.tmux.prefix))
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, rejected()));
        }
    }
    Ok(())
}

pub(crate) fn validate_session_hosts(config: &super::Config) -> Result<(), String> {
    let Some(instance) = current()? else {
        return Ok(());
    };
    validate_hosts(config, &instance.instance_id, &instance.herdr)
}

fn validate_hosts(
    config: &super::Config,
    instance: &str,
    expected: &HerdrHost,
) -> Result<(), String> {
    let hosts = &config.session_hosts.herdr;
    if config.cluster.instance_id.as_deref() != Some(instance)
        || hosts.endpoints.iter().any(|(key, endpoint)| {
            endpoint.mode != super::session_hosts::EndpointMode::LegacyDedicated
                || key != &expected.endpoint_key
                || endpoint.execution_node != instance
                || endpoint.socket_path != expected.socket
                || endpoint.herdr_home != expected.home
                || endpoint.herdr_session != expected.session
        })
        || hosts
            .channels
            .values()
            .any(|key| key != &expected.endpoint_key)
    {
        return Err("test Herdr configuration differs from its receipt".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "test_instance/host_scope_tests.rs"]
pub(crate) mod tests;
