//! Read-side policy: the ping/pong handshake check, bounded read-only retries, the rule
//! that one observation never spans two connections, and the E7 restore-resume reading.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use super::contract::{HerdrOutcome, HerdrTransport};
use super::model::{
    ExecutionState, HERDR_PROTOCOL, HerdrCall, HerdrEndpoint, HerdrObservation, HerdrRequest,
    HerdrResult, VERIFIED_HERDR_VERSIONS,
};
use crate::services::session_host::model::HostError;

pub(crate) const RESTORE_RESUME_NOT_OFF: &str = "restore_resume_not_off";
/// The only config a dedicated server may run with: any other byte, a missing file or a
/// parse error leaves Herdr on its default, which resumes agents on restore.
pub(crate) const CANONICAL_CONFIG: &[u8] = b"[session]\nresume_agents_on_restore = false\n";
/// A config written this close to the server's start may have been read in its old form.
const START_MARGIN: Duration = Duration::from_secs(1);

/// The running server's effective `[session] resume_agents_on_restore`. `Off` needs the
/// connected server's provenance: started from the canonical config, unchanged since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestoreResume {
    Off { generation: u64 },
    On,
    Unverified(RestoreUnverified),
}

/// Why a reading is not `Off`; every failed or unsupported observation lands here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestoreUnverified {
    /// No connection, or its peer process could not be read.
    NoPeer,
    VersionNotVerified,
    PlatformUnsupported,
    /// No herdr home, an unknown endpoint, or a server env naming another config.
    NotBootstrapped,
    ProcessUnreadable,
    /// The peer pid's process changed while it was read, or started after the connection.
    ProcessChanged,
    ConfigMissing,
    ConfigUnreadable,
    ConfigNotCanonical,
    /// The file changed while it was read.
    ConfigChanged,
    ConfigChangedSinceStart,
    /// The connection or its peer changed between the first and last check.
    Reconnected,
}

impl RestoreResume {
    /// The connection a create or input may use; anything but `Off` admits none.
    pub(crate) fn admitted_generation(self) -> Option<u64> {
        match self {
            Self::Off { generation } => Some(generation),
            Self::On | Self::Unverified(_) => None,
        }
    }
}

/// One read of the config file: bytes and modification time from the same open file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigRead {
    pub bytes: Vec<u8>,
    pub modified: SystemTime,
}

/// OS reads E7 needs about the server process and its config file.
pub(crate) trait ServerProvenance {
    /// Wall-clock start of `pid`.
    fn process_start(&self, pid: u32) -> Result<SystemTime, RestoreUnverified>;
    /// Every value `pid`'s environment holds for `key`.
    fn process_env(&self, pid: u32, key: &str) -> Result<Vec<String>, RestoreUnverified>;
    fn read_config(&self, path: &Path) -> Result<ConfigRead, RestoreUnverified>;
}

/// E7 over the production OS reads.
pub(crate) fn read_restore_resume<T: HerdrTransport + ?Sized>(
    transport: &T,
    endpoint: &HerdrEndpoint,
) -> RestoreResume {
    read_restore_resume_with(transport, endpoint, &super::provenance::OsProvenance)
}

/// No API reads effective settings, so `Off` is proven from provenance: a verified server
/// started with this endpoint's config and XDG home, the file canonical and older than it.
pub(crate) fn read_restore_resume_with<T: HerdrTransport + ?Sized>(
    transport: &T,
    endpoint: &HerdrEndpoint,
    provenance: &dyn ServerProvenance,
) -> RestoreResume {
    match prove_restore_off(transport, endpoint, provenance) {
        Ok(generation) => RestoreResume::Off { generation },
        Err(reason) => RestoreResume::Unverified(reason),
    }
}

fn prove_restore_off<T: HerdrTransport + ?Sized>(
    transport: &T,
    endpoint: &HerdrEndpoint,
    provenance: &dyn ServerProvenance,
) -> Result<u64, RestoreUnverified> {
    use RestoreUnverified as Why;
    let peer = transport.server_peer()?;
    if !VERIFIED_HERDR_VERSIONS.contains(&peer.version.as_str()) {
        return Err(Why::VersionNotVerified);
    }
    let home = endpoint.herdr_home().ok_or(Why::NotBootstrapped)?;
    let config = home.join("config.toml");
    let xdg = home.join("xdg");
    let started = provenance.process_start(peer.pid)?;
    let env_matches = |key, expected: &Path| -> Result<bool, Why> {
        let values = provenance.process_env(peer.pid, key)?;
        Ok(values.len() == 1 && Path::new(&values[0]) == expected)
    };
    let bootstrapped =
        env_matches("HERDR_CONFIG_PATH", &config)? && env_matches("XDG_CONFIG_HOME", &xdg)?;
    if provenance.process_start(peer.pid)? != started || started > peer.connected_at {
        return Err(Why::ProcessChanged);
    }
    if !bootstrapped {
        return Err(Why::NotBootstrapped);
    }
    let read = provenance.read_config(&config)?;
    if read.bytes != CANONICAL_CONFIG {
        return Err(Why::ConfigNotCanonical);
    }
    if read.modified + START_MARGIN >= started {
        return Err(Why::ConfigChangedSinceStart);
    }
    match transport.server_peer() {
        Ok(last) if last == peer => Ok(peer.generation),
        _ => Err(Why::Reconnected),
    }
}

/// What a verified pong reported. Only E7 reads the version; the handshake does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrHello {
    pub version: String,
    pub protocol: u32,
}

/// A pong must echo the ping id and speak our protocol; the version may differ.
pub(crate) fn hello_result(
    call: &HerdrCall,
    outcome: HerdrOutcome,
) -> Result<HerdrHello, HostError> {
    if call.request != (HerdrRequest::Ping {}) {
        return Err(HostError::Protocol("handshake without ping".to_string()));
    }
    let reply = outcome.map_err(|error| HostError::Transport(format!("{error:?}")))?;
    if reply.id != call.id {
        return Err(HostError::Protocol(format!(
            "pong id {} for ping {}",
            reply.id, call.id
        )));
    }
    match reply.body {
        Ok(HerdrResult::Pong { version, protocol }) if protocol == HERDR_PROTOCOL => {
            Ok(HerdrHello { version, protocol })
        }
        Ok(HerdrResult::Pong { protocol, .. }) => Err(HostError::Protocol(format!(
            "herdr protocol {protocol}, expected {HERDR_PROTOCOL}"
        ))),
        Ok(other) => Err(HostError::Protocol(format!("ping answered with {other:?}"))),
        Err(body) => Err(HostError::Remote {
            code: body.code,
            message: body.message,
        }),
    }
}

/// Repeats a read-only attempt until it gets any reply or `deadline` passes.
pub(crate) fn retry_read(
    deadline: Instant,
    backoff: Duration,
    mut attempt: impl FnMut() -> (HerdrOutcome, u64),
) -> (HerdrOutcome, u64) {
    loop {
        let (outcome, generation) = attempt();
        if outcome.is_ok() || Instant::now() + backoff >= deadline {
            return (outcome, generation);
        }
        thread::sleep(backoff);
    }
}

/// Execution evidence read on another connection than the snapshot is dropped.
pub(crate) fn fence_generation(
    observation: HerdrObservation,
    snapshot_generation: u64,
    process_generation: u64,
) -> HerdrObservation {
    if snapshot_generation == process_generation {
        return observation;
    }
    HerdrObservation {
        execution: ExecutionState::Unknown,
        shell_pid: None,
        ..observation
    }
}
