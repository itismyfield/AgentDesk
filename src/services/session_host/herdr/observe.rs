//! Read-side policy: the ping/pong handshake check, bounded read-only retries, the rule
//! that one observation never spans two servers, and the E7 restore-resume reading.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use super::contract::{HerdrOutcome, HerdrTransport, ServerWitness, Witnessed};
use super::model::{
    ExecutionState, HERDR_PROTOCOL, HerdrCall, HerdrEndpoint, HerdrObservation, HerdrRequest,
    HerdrResult, VERIFIED_HERDR_VERSIONS,
};
use super::provenance::ProcessStart;
use crate::services::session_host::model::HostError;

pub(crate) const RESTORE_RESUME_NOT_OFF: &str = "restore_resume_not_off";
/// The only config a dedicated server may run with: any other byte, a missing file or a
/// parse error leaves Herdr on its default, which resumes agents on restore.
pub(crate) const CANONICAL_CONFIG: &[u8] = b"[session]\nresume_agents_on_restore = false\n";
/// A config written this close to the server's start may have been read in its old form.
const START_MARGIN: Duration = Duration::from_secs(1);

/// The running server's effective `[session] resume_agents_on_restore`. `Off` needs the
/// server's provenance, started from the canonical config, and names that server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RestoreResume {
    Off { witness: ServerWitness },
    On,
    Unverified(RestoreUnverified),
}

/// Why a reading is not `Off`; every failed or unsupported observation lands here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestoreUnverified {
    /// No connection, no handshake, or its peer process could not be read.
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
    /// A fresh connection reached another server than the one checked.
    ServerChanged,
}

impl RestoreResume {
    /// The only server a create or input may reach; anything but `Off` admits none.
    pub(crate) fn admitted_witness(self) -> Option<ServerWitness> {
        match self {
            Self::Off { witness } => Some(witness),
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
    fn process_start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified>;
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
        Ok(witness) => RestoreResume::Off { witness },
        Err(reason) => RestoreResume::Unverified(reason),
    }
}

/// The ping connection names the server; its version, env, config and start are checked;
/// a last connection that carries no request must still reach that same server.
fn prove_restore_off<T: HerdrTransport + ?Sized>(
    transport: &T,
    endpoint: &HerdrEndpoint,
    provenance: &dyn ServerProvenance,
) -> Result<ServerWitness, RestoreUnverified> {
    use RestoreUnverified as Why;
    let hello = transport.hello()?;
    let witness = hello.witness;
    if witness.socket != endpoint.socket_path() {
        return Err(Why::NotBootstrapped);
    }
    if hello.started > hello.connected_at {
        return Err(Why::ProcessChanged);
    }
    if !VERIFIED_HERDR_VERSIONS.contains(&hello.version.as_str()) {
        return Err(Why::VersionNotVerified);
    }
    let home = endpoint.herdr_home().ok_or(Why::NotBootstrapped)?;
    let config = home.join("config.toml");
    let xdg = home.join("xdg");
    let env_matches = |key, expected: &Path| -> Result<bool, Why> {
        let values = provenance.process_env(witness.pid, key)?;
        Ok(values.len() == 1 && Path::new(&values[0]) == expected)
    };
    if !(env_matches("HERDR_CONFIG_PATH", &config)? && env_matches("XDG_CONFIG_HOME", &xdg)?) {
        return Err(Why::NotBootstrapped);
    }
    let read = provenance.read_config(&config)?;
    if read.bytes != CANONICAL_CONFIG {
        return Err(Why::ConfigNotCanonical);
    }
    if read.modified + START_MARGIN >= hello.started {
        return Err(Why::ConfigChangedSinceStart);
    }
    if provenance.process_start(witness.pid)?.identity != witness.start {
        return Err(Why::ProcessChanged);
    }
    if transport.server_witness()? != witness {
        return Err(Why::ServerChanged);
    }
    Ok(witness)
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

/// Repeats a read-only attempt until it gets any reply or `deadline` passes; the result
/// keeps whatever the answering attempt returned alongside its outcome.
pub(crate) fn retry_read<W>(
    deadline: Instant,
    backoff: Duration,
    mut attempt: impl FnMut() -> (HerdrOutcome, W),
) -> (HerdrOutcome, W) {
    loop {
        let (outcome, seen) = attempt();
        if outcome.is_ok() || Instant::now() + backoff >= deadline {
            return (outcome, seen);
        }
        thread::sleep(backoff);
    }
}

/// Execution evidence joins a snapshot only when both replies name the same known server.
pub(crate) fn fence_witness(
    observation: HerdrObservation,
    snapshot: &Witnessed,
    process: &Witnessed,
) -> HerdrObservation {
    if matches!((snapshot, process), (Ok(a), Ok(b)) if a == b) {
        return observation;
    }
    HerdrObservation {
        execution: ExecutionState::Unknown,
        shell_pid: None,
        ..observation
    }
}
