//! Typed peer observations are verified without consulting AgentDesk's own HOME or PATH.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::super::super::contract::{ServerHello, ServerWitness};
use super::super::super::model::{HERDR_PROTOCOL, VERIFIED_HERDR_VERSIONS};
use super::super::super::provenance::{ProcessStart, StartIdentity};
use super::Unverified;
use super::file::FileToken;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PeerEnvironment {
    pub home: Vec<String>,
    pub xdg_config_home: Vec<String>,
    pub herdr_config_path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedImage {
    pub witness: ServerWitness,
    pub token: FileToken,
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
    pub started: SystemTime,
    pub home: PathBuf,
    pub config: PathBuf,
    pub checker: PathBuf,
    pub image: FileToken,
}

pub(crate) fn validate(observed: PeerObservation) -> Result<PeerSnapshot, Unverified> {
    let PeerObservation {
        expected_socket,
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
        || loaded.witness != hello.witness
        || !loaded.token.same_image(&image)
    {
        return Err(Unverified::Identity);
    }
    image.unchanged()?;
    loaded.token.unchanged()?;
    Ok(PeerSnapshot {
        witness: hello.witness,
        started: hello.started,
        home,
        config: default,
        checker: executable.checker,
        image,
    })
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
