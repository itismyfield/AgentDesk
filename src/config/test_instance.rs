use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

pub(crate) mod environment;
pub(crate) mod lifecycle;

const MARKER: &str = "AGENTDESK_TEST_MANIFEST";
static INSTANCE: OnceLock<Result<TestInstance, String>> = OnceLock::new();

#[derive(Debug)]
pub(crate) struct TestInstance {
    pub(crate) root: PathBuf,
    config_path: PathBuf,
    pub(crate) instance_id: String,
    #[cfg(unix)]
    pub(crate) head: String,
    #[cfg(unix)]
    pub(crate) worktree: PathBuf,
    pub(crate) tmux: TmuxHost,
    pub(crate) herdr: HerdrHost,
    environment: environment::Environment,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TmuxHost {
    pub(crate) socket: PathBuf,
    pub(crate) tmpdir: PathBuf,
    pub(crate) prefix: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
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
            let instance = load_instance_declaration(
                &env_path(MARKER)?,
                &env_path("AGENTDESK_ROOT_DIR")?,
                &env_path("AGENTDESK_CONFIG")?,
            )?;
            instance.environment.verify()?;
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

pub(crate) fn configure_reentry_command(command: &mut Command) -> Result<(), String> {
    environment::configure_child_command(command)?;
    if let Some(instance) = current()? {
        command
            .env(MARKER, instance.root.join("manifest.json"))
            .env("AGENTDESK_ROOT_DIR", &instance.root)
            .env("AGENTDESK_CONFIG", &instance.config_path);
    } else {
        command.env_remove("AGENTDESK_ROOT_DIR");
    }
    Ok(())
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

fn load_instance_declaration(
    path: &Path,
    root: &Path,
    config_path: &Path,
) -> Result<TestInstance, String> {
    checked_path(root)?;
    if std::fs::canonicalize(root).map_err(|_| rejected())? != root
        || path != root.join("manifest.json")
        || config_path.parent() != Some(crate::runtime_layout::config_dir(root).as_path())
        || config_path.file_name() != Some(std::ffi::OsStr::new("agentdesk.yaml"))
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
    let head = manifest["head"].as_str().ok_or_else(rejected)?;
    let worktree = Path::new(manifest["worktree"].as_str().ok_or_else(rejected)?);
    checked_path(worktree)?;
    if head.len() != 40
        || !head
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || std::fs::canonicalize(worktree).map_err(|_| rejected())? != worktree
    {
        return Err(rejected());
    }
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
        config_path: config_path.into(),
        instance_id: id.into(),
        #[cfg(unix)]
        head: head.into(),
        #[cfg(unix)]
        worktree: worktree.into(),
        tmux,
        herdr,
        environment: environment::Environment::load(root, &manifest, &config)?,
    })
}

#[cfg(unix)]
impl TestInstance {
    fn verify_run_declaration(&self) -> Result<(), String> {
        self.environment.verify()?;
        for path in [
            self.root.join("manifest.json"),
            self.root.join("receipt.json"),
        ] {
            let declaration = read_json(&path)?;
            if declaration["head"] != self.head
                || declaration["worktree"] != self.worktree.to_string_lossy().as_ref()
                || declaration["instance_id"] != self.instance_id
                || declaration["root"] != self.root.to_string_lossy().as_ref()
            {
                return Err(rejected());
            }
        }
        Ok(())
    }
}

pub(crate) fn configure_tmux_command(command: &mut Command) -> io::Result<()> {
    if let Some(instance) = current().map_err(io::Error::other)? {
        environment::validate_path(&instance.root).map_err(io::Error::other)?;
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
    if let Some(instance) = current().map_err(io::Error::other)?
        && ((target != instance.tmux.prefix
            && !target.starts_with(&format!("{}-", instance.tmux.prefix)))
            || !target
                .chars()
                .all(|ch| ch.is_alphanumeric() || ch == '-' || ch == '_'))
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, rejected()));
    }
    Ok(())
}

#[cfg(unix)]
mod owner {
    use super::*;
    use crate::services::herdr_launch::HerdrLaunchEndpoint;
    use crate::services::session_host::{ServerWitness, StartIdentity};
    use crate::utils::secret_file::private_directory::PrivateDirectory;
    use serde::Serialize;

    const FILE: &str = "herdr-owner.json";

    #[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    pub(crate) struct OwnerReceipt {
        schema_version: u8,
        instance_id: String,
        endpoint: HerdrHost,
        binary_sha256: String,
        daemon: Daemon,
        workspaces: Vec<OwnedPane>,
    }

    #[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Daemon {
        pid: u32,
        start_identity: RecordedStart,
    }

    #[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
    #[serde(tag = "platform", rename_all = "lowercase", deny_unknown_fields)]
    enum RecordedStart {
        Darwin { seconds: u64, micros: u64 },
        Linux { ticks: u64 },
    }

    #[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct OwnedPane {
        workspace_id: String,
        pane_id: String,
    }

    fn identifier(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= 256
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_:.".contains(&b))
    }

    impl OwnerReceipt {
        pub(crate) fn pid(&self) -> u32 {
            self.daemon.pid
        }

        pub(crate) fn matches_peer(&self, pid: u32, start: &StartIdentity) -> bool {
            self.pid() == pid
                && match (&self.daemon.start_identity, start) {
                    (
                        RecordedStart::Darwin { seconds, micros },
                        StartIdentity::Darwin {
                            seconds: actual_seconds,
                            micros: actual_micros,
                        },
                    ) => seconds == actual_seconds && micros == actual_micros,
                    (RecordedStart::Linux { ticks }, StartIdentity::LinuxTicks(actual)) => {
                        ticks == actual
                    }
                    _ => false,
                }
        }

        pub(crate) fn matches_witness(&self, witness: &ServerWitness) -> bool {
            self.endpoint.socket == witness.socket && self.matches_peer(witness.pid, &witness.start)
        }

        pub(crate) fn contains_pane(&self, pane: &str) -> bool {
            self.workspaces.iter().any(|owned| owned.pane_id == pane)
        }

        fn valid(&self, instance: &TestInstance) -> bool {
            let start = match self.daemon.start_identity {
                RecordedStart::Darwin { seconds, micros } => seconds > 0 && micros < 1_000_000,
                RecordedStart::Linux { ticks } => ticks > 0,
            };
            let mut workspaces = std::collections::HashSet::new();
            let mut panes = std::collections::HashSet::new();
            self.schema_version == 1
                && self.instance_id == instance.instance_id
                && self.endpoint == instance.herdr
                && self.pid() > 0
                && start
                && self.binary_sha256.len() == 64
                && self
                    .binary_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && self.workspaces.iter().all(|pane| {
                    identifier(&pane.workspace_id)
                        && identifier(&pane.pane_id)
                        && workspaces.insert(&pane.workspace_id)
                        && panes.insert(&pane.pane_id)
                })
        }
    }

    fn read(instance: &TestInstance, directory: &PrivateDirectory) -> Result<OwnerReceipt, String> {
        let bytes = directory
            .read(FILE)
            .map_err(|_| rejected())?
            .ok_or_else(rejected)?;
        let receipt: OwnerReceipt = serde_json::from_slice(&bytes).map_err(|_| rejected())?;
        if !receipt.valid(instance) {
            return Err(rejected());
        }
        Ok(receipt)
    }

    pub(crate) fn owned_herdr(socket: &Path) -> Result<Option<OwnerReceipt>, String> {
        let Some(instance) = current()? else {
            return Ok(None);
        };
        if socket != instance.herdr.socket {
            return Err(rejected());
        }
        checked_path(socket)?;
        let directory =
            PrivateDirectory::open(&instance.root.join("runtime")).map_err(|_| rejected())?;
        read(instance, &directory).map(Some)
    }

    pub(crate) fn validate_herdr_create_target(cwd: &Path, label: &str) -> Result<(), String> {
        let Some(instance) = current()? else {
            return Ok(());
        };
        let (logical, nonce) = label
            .split_once('#')
            .map_or((label, None), |(l, n)| (l, Some(n)));
        validate_tmux_target(logical).map_err(|_| rejected())?;
        if nonce.is_some_and(|nonce| {
            nonce.len() != 32
                || !nonce
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) || !cwd.starts_with(&instance.root)
        {
            return Err(rejected());
        }
        checked_path(cwd)?;
        if !cwd.is_dir() || std::fs::canonicalize(cwd).map_err(|_| rejected())? != cwd {
            return Err(rejected());
        }
        Ok(())
    }

    pub(crate) struct OwnedCreateGuard {
        instance: &'static TestInstance,
        directory: PrivateDirectory,
        _lock: std::fs::File,
        baseline: OwnerReceipt,
    }

    pub(crate) fn begin_herdr_create(
        endpoint: &HerdrLaunchEndpoint,
        witness: &ServerWitness,
        label: &str,
        cwd: &Path,
    ) -> Result<Option<OwnedCreateGuard>, String> {
        let Some(instance) = current()? else {
            return Ok(None);
        };
        validate_herdr_create_target(cwd, label)?;
        if endpoint.execution_node != instance.instance_id
            || endpoint.config_key != instance.herdr.endpoint_key
            || endpoint.socket_addr != instance.herdr.socket.to_string_lossy()
            || endpoint.herdr_session != instance.herdr.session
        {
            return Err(rejected());
        }
        let directory =
            PrivateDirectory::open(&instance.root.join("runtime")).map_err(|_| rejected())?;
        let lock = directory.lock("herdr-owner.lock").map_err(|_| rejected())?;
        let baseline = read(instance, &directory)?;
        if !baseline.matches_witness(witness) {
            return Err(rejected());
        }
        Ok(Some(OwnedCreateGuard {
            instance,
            directory,
            _lock: lock,
            baseline,
        }))
    }

    impl OwnedCreateGuard {
        pub(crate) fn record_created(
            &mut self,
            workspace_id: &str,
            pane_id: &str,
        ) -> Result<(), String> {
            if !identifier(workspace_id)
                || !identifier(pane_id)
                || self
                    .baseline
                    .workspaces
                    .iter()
                    .any(|pane| pane.workspace_id == workspace_id || pane.pane_id == pane_id)
                || read(self.instance, &self.directory)? != self.baseline
            {
                return Err(rejected());
            }
            self.baseline.workspaces.push(OwnedPane {
                workspace_id: workspace_id.into(),
                pane_id: pane_id.into(),
            });
            let bytes = serde_json::to_vec(&self.baseline).map_err(|_| rejected())?;
            self.directory
                .write_atomic(FILE, &bytes)
                .map_err(|_| rejected())
        }
    }
}

#[cfg(unix)]
pub(crate) use owner::{begin_herdr_create, owned_herdr, validate_herdr_create_target};

#[cfg(all(test, unix))]
#[path = "test_instance/host_scope_tests.rs"]
pub(crate) mod tests;

#[cfg(all(test, unix))]
mod api_boundary_tests;
#[cfg(all(test, unix))]
mod bound_health_tests;
#[cfg(all(test, unix))]
mod environment_tests;
