//! Typed peer observations are verified without consulting AgentDesk's own HOME or PATH.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use super::super::contract::{HerdrTransport, ServerHello, ServerWitness};
use super::super::model::{HERDR_PROTOCOL, VERIFIED_HERDR_VERSIONS};
use super::super::provenance::{self, ProcessStart, StartIdentity};
use super::PeerSource;
use super::Unverified;
use super::file::{self, FileToken};

#[path = "loaded_image.rs"]
mod loaded_image;
pub(crate) use loaded_image::LoadedImage;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PeerEnvironment {
    pub home: Vec<String>,
    pub xdg_config_home: Vec<String>,
    pub herdr_config_path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecutableImage {
    pub checker: PathBuf,
    pub checker_token: FileToken,
    /// This must identify the loaded image, rather than only statting its pathname.
    pub loaded: Option<LoadedImage>,
}

pub(crate) struct PeerObservation {
    pub expected_socket: PathBuf,
    pub socket_dir: (u64, u64),
    pub hello: ServerHello,
    /// Reread after the environment and loaded-image observations.
    pub current: ServerWitness,
    pub process: ProcessStart,
    pub protocol: u32,
    pub environment: PeerEnvironment,
    pub executable: ExecutableImage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerSnapshot {
    pub witness: ServerWitness,
    pub socket_dir: (u64, u64),
    pub started: SystemTime,
    pub home: PathBuf,
    pub config: PathBuf,
    pub checker: PathBuf,
    pub image: FileToken,
}

pub(crate) fn validate(observed: PeerObservation) -> Result<PeerSnapshot, Unverified> {
    let PeerObservation {
        expected_socket,
        socket_dir,
        hello,
        current,
        process,
        protocol,
        environment,
        executable,
    } = observed;
    if protocol != HERDR_PROTOCOL || !VERIFIED_HERDR_VERSIONS.contains(&hello.version.as_str()) {
        return Err(Unverified::Unsupported);
    }
    if hello.witness != current
        || hello.witness.socket != expected_socket
        || process.identity != hello.witness.start
        || process.wall_clock != hello.started
        || hello.started > hello.connected_at
        || hello.witness.pid == 0
        || !hello.witness.socket.is_absolute()
    {
        return Err(Unverified::Peer);
    }
    if !match hello.witness.start {
        StartIdentity::Darwin { seconds, micros } => seconds != 0 && micros < 1_000_000,
        StartIdentity::LinuxTicks(ticks) => ticks != 0,
    } {
        return Err(Unverified::Peer);
    }
    let home = absolute(&environment.home)?.ok_or(Unverified::Peer)?;
    let default = home.join(".config/herdr/config.toml");
    let xdg = absolute(&environment.xdg_config_home)?;
    let explicit = absolute(&environment.herdr_config_path)?;
    if xdg
        .as_ref()
        .is_some_and(|path| path.join("herdr/config.toml") != default)
        || explicit.as_ref().is_some_and(|path| path != &default)
    {
        return Err(Unverified::Ambiguous);
    }
    let image = executable.checker_token;
    let loaded = executable.loaded.ok_or(Unverified::Identity)?;
    if !executable.checker.is_absolute()
        || executable.checker.as_path() != image.logical_path()
        || !loaded.matches(&image)
    {
        return Err(Unverified::Identity);
    }
    image.fast_unchanged()?;
    Ok(PeerSnapshot {
        witness: hello.witness,
        socket_dir,
        started: hello.started,
        home,
        config: default,
        checker: executable.checker,
        image,
    })
}

/// Synchronous OS reads belong to the async owner's blocking task, never the gate.
pub(crate) struct SocketPeerSource {
    transport: Arc<dyn HerdrTransport>,
    expected_socket: PathBuf,
    checker: Mutex<Option<FileToken>>,
}

impl SocketPeerSource {
    pub(crate) fn new(transport: Arc<dyn HerdrTransport>, expected_socket: PathBuf) -> Self {
        Self {
            transport,
            expected_socket,
            checker: Mutex::new(None),
        }
    }
}

impl PeerSource for SocketPeerSource {
    fn snapshot(&self) -> Result<PeerSnapshot, Unverified> {
        let parent = self.expected_socket.parent().ok_or(Unverified::Peer)?;
        let socket_dir = file::directory_identity(parent)?;
        let hello = self.transport.hello().map_err(|_| Unverified::Peer)?;
        let pid = hello.witness.pid;
        let process = provenance::process_start(pid).map_err(|_| Unverified::Peer)?;
        let entries = provenance::process_environ(pid).map_err(|_| Unverified::Peer)?;
        let values = |key: &str| {
            let prefix = format!("{key}=");
            entries
                .iter()
                .filter_map(|entry| entry.strip_prefix(prefix.as_str()).map(str::to_owned))
                .collect()
        };
        let checker: PathBuf = provenance::process_exec_path(pid)
            .ok_or(Unverified::Identity)?
            .into();
        let checker_token = {
            let mut cached = self.checker.lock().map_err(|_| Unverified::Cache)?;
            if cached.as_ref().is_none_or(|token| {
                token.logical_path() != checker || token.fast_unchanged().is_err()
            }) {
                *cached = Some(file::image(&checker)?);
            }
            cached.as_ref().ok_or(Unverified::Identity)?.clone()
        };
        let loaded = loaded_image::for_process(pid).ok();
        let current = self
            .transport
            .server_witness()
            .map_err(|_| Unverified::Peer)?;
        if socket_dir != file::directory_identity(parent)? {
            return Err(Unverified::Changed);
        }
        validate(PeerObservation {
            expected_socket: self.expected_socket.clone(),
            socket_dir,
            hello,
            current,
            process,
            protocol: HERDR_PROTOCOL,
            environment: PeerEnvironment {
                home: values("HOME"),
                xdg_config_home: values("XDG_CONFIG_HOME"),
                herdr_config_path: values("HERDR_CONFIG_PATH"),
            },
            executable: ExecutableImage {
                checker,
                checker_token,
                loaded,
            },
        })
    }
}

fn absolute(values: &[String]) -> Result<Option<PathBuf>, Unverified> {
    match values {
        [] => Ok(None),
        [value] if !value.trim().is_empty() && Path::new(value).is_absolute() => {
            Ok(Some(value.into()))
        }
        _ => Err(Unverified::Peer),
    }
}

#[cfg(all(test, unix))]
#[path = "peer_tests.rs"]
mod tests;
