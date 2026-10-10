//! Which process in a launched Herdr pane is the provider, from reads alone: the one
//! foreground child of the root shell whose exec-time environment names the launch's nonce.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use super::contract::{ForegroundProcesses, ServerWitness, Witnessed};
use super::observe::RestoreUnverified;
use super::provenance::{self, ProcessStart, StartIdentity};
use crate::db::dispatched_sessions::hosted_execution::{ExpectedExecution, ProcessStamp};
use crate::services::tui_prompt_dedupe::binding_context::context_names_execution;

/// How long a new pane is watched for its provider process, and how often.
pub(crate) const PROVIDER_WINDOW: Duration = Duration::from_secs(15);
pub(crate) const PROVIDER_POLL: Duration = Duration::from_millis(200);
/// A Linux start reads up to a second early.
const START_SLACK: Duration = Duration::from_secs(1);
const CONTEXT_ENV: &str = "AGENTDESK_BINDING_CONTEXT=";

/// Why a pane reading is not launch evidence; nothing is stored for any of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EvidenceGap {
    UnknownEndpoint,
    /// The pane read failed or answered off-contract.
    ReadFailed(String),
    NoRoot,
    Unreported,
    /// A reading came from another server than the one the launch was sent to.
    ServerChanged,
    /// No foreground child of the root shell when the window closed.
    NoneYet,
    /// Several foreground children of the root shell; read once, never waited out.
    Ambiguous(Vec<u32>),
    /// A process's parent or start could not be read.
    ProcessUnreadable(u32),
    StartedBeforeLaunch,
    EnvUnreadable,
    NonceMissing,
    /// The environment names another execution's binding context, or one that does not check.
    OtherNonce(String),
    /// The provider sees a Herdr pane variable, so it may report to Herdr.
    HerdrEnvPresent(String),
    /// The root shell or provider changed between the readings.
    Replaced,
    /// The evidence was read but not stored on this nonce's Pending row.
    NotRecorded(String),
}

/// One `pane.process_info` reading: root shell and foreground pids, and who answered.
pub(crate) type PaneProcesses = (
    Result<(Option<u32>, ForegroundProcesses), String>,
    Witnessed,
);

/// OS reads of another process; tests inject a fake.
pub(crate) trait ProcessOs: Send + Sync {
    fn exists(&self, _pid: u32) -> Result<bool, RestoreUnverified> {
        Err(RestoreUnverified::ProcessUnreadable)
    }
    fn parent(&self, pid: u32) -> Result<u32, RestoreUnverified>;
    fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified>;
    fn environ(&self, pid: u32) -> Result<Vec<String>, RestoreUnverified>;
    /// Only for the provenance text; never compared.
    fn exec_path(&self, pid: u32) -> Option<String>;
}

/// Absence is separate evidence from failure to read a process start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordedProcess {
    ExactAlive,
    ProvenAbsent,
    Replaced,
    Unreadable,
}

pub(crate) fn recorded_process(os: &dyn ProcessOs, stamp: &ProcessStamp) -> RecordedProcess {
    match os.exists(stamp.pid) {
        Ok(false) => RecordedProcess::ProvenAbsent,
        Ok(true) => match os.start(stamp.pid) {
            Ok(start) if start_text(start.identity) == stamp.start => RecordedProcess::ExactAlive,
            Ok(_) => RecordedProcess::Replaced,
            Err(_) => RecordedProcess::Unreadable,
        },
        Err(_) => RecordedProcess::Unreadable,
    }
}

pub(crate) struct HostOs;

impl ProcessOs for HostOs {
    fn exists(&self, pid: u32) -> Result<bool, RestoreUnverified> {
        #[cfg(unix)]
        {
            let pid = i32::try_from(pid)
                .ok()
                .filter(|pid| *pid > 0)
                .ok_or(RestoreUnverified::ProcessUnreadable)?;
            // Signal zero tests existence and permissions; it never sends a termination signal.
            let result = unsafe { libc::kill(pid, 0) };
            if result == 0 {
                return Ok(true);
            }
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ESRCH) => Ok(false),
                _ => Err(RestoreUnverified::ProcessUnreadable),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            Err(RestoreUnverified::PlatformUnsupported)
        }
    }

    fn parent(&self, pid: u32) -> Result<u32, RestoreUnverified> {
        provenance::process_parent(pid)
    }

    fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        provenance::process_start(pid)
    }

    fn environ(&self, pid: u32) -> Result<Vec<String>, RestoreUnverified> {
        provenance::process_environ(pid)
    }

    fn exec_path(&self, pid: u32) -> Option<String> {
        provenance::process_exec_path(pid)
    }
}

pub(crate) struct ProbeRequest<'a> {
    pub provider: &'a str,
    pub nonce: &'a str,
    /// No process that started before this can be the launch's provider.
    pub launched_at: SystemTime,
    /// The server the launch was sent to; every pane reading must come from it.
    pub witness: &'a ServerWitness,
    pub window: Duration,
}

/// The stored text of a process start; equal text is the same start.
pub(crate) fn start_text(start: StartIdentity) -> String {
    match start {
        StartIdentity::Darwin { seconds, micros } => format!("darwin:{seconds}.{micros:06}"),
        StartIdentity::LinuxTicks(ticks) => format!("linux-ticks:{ticks}"),
    }
}

/// Root shell and provider of the pane `read` reports, as launch evidence, or why not.
pub(crate) fn probe(
    read: &dyn Fn() -> PaneProcesses,
    os: &dyn ProcessOs,
    request: &ProbeRequest,
) -> Result<ExpectedExecution, EvidenceGap> {
    let (root, provider) = await_candidate(read, os, request)?;
    let (root_start, provider_start) = still_running(read, os, request.witness, root, provider)?;
    if provider_start.wall_clock + START_SLACK < request.launched_at {
        return Err(EvidenceGap::StartedBeforeLaunch);
    }
    check_environment(os, request, provider)?;
    let again = still_running(read, os, request.witness, root, provider)?;
    if (again.0.identity, again.1.identity) != (root_start.identity, provider_start.identity) {
        return Err(EvidenceGap::Replaced);
    }
    let exec = os.exec_path(provider).unwrap_or_else(|| "?".into());
    let stamp = |pid, start: ProcessStart| ProcessStamp {
        pid,
        start: start_text(start.identity),
    };
    Ok(ExpectedExecution {
        binding_provider: request.provider.to_string(),
        binding_nonce: request.nonce.to_string(),
        root: stamp(root, root_start),
        provider_process: stamp(provider, provider_start),
        provenance: format!("herdr_launch:ppid+env;exec={exec}"),
    })
}

enum Reading {
    One { root: u32, provider: u32 },
    Wait(EvidenceGap),
}

/// Reads again while no single child of the root shell shows up and the window is open.
fn await_candidate(
    read: &dyn Fn() -> PaneProcesses,
    os: &dyn ProcessOs,
    request: &ProbeRequest,
) -> Result<(u32, u32), EvidenceGap> {
    let deadline = Instant::now() + request.window;
    loop {
        let gap = match candidate(read(), os, request.witness)? {
            Reading::One { root, provider } => return Ok((root, provider)),
            Reading::Wait(gap) => gap,
        };
        if Instant::now() + PROVIDER_POLL >= deadline {
            return Err(gap);
        }
        thread::sleep(PROVIDER_POLL);
    }
}

/// The provider is the foreground process whose OS parent is the root shell; the provider's
/// own children share its process group, so "not the shell" alone would name several.
fn candidate(
    reading: PaneProcesses,
    os: &dyn ProcessOs,
    witness: &ServerWitness,
) -> Result<Reading, EvidenceGap> {
    let (root, listed) = from_server(reading, witness)?;
    let mut children = Vec::new();
    let mut unreadable = None;
    for pid in listed.into_iter().filter(|pid| *pid != root) {
        match os.parent(pid) {
            Ok(parent) if parent == root => children.push(pid),
            Ok(_) => {}
            Err(_) => unreadable = unreadable.or(Some(pid)),
        }
    }
    match (children.as_slice(), unreadable) {
        ([_, _, ..], _) => Err(EvidenceGap::Ambiguous(children)),
        ([provider], None) => Ok(Reading::One {
            root,
            provider: *provider,
        }),
        (_, Some(pid)) => Ok(Reading::Wait(EvidenceGap::ProcessUnreadable(pid))),
        ([], None) => Ok(Reading::Wait(EvidenceGap::NoneYet)),
    }
}

/// Root shell and foreground pids of a reading the launch's server gave.
fn from_server(
    (result, answered): PaneProcesses,
    witness: &ServerWitness,
) -> Result<(u32, Vec<u32>), EvidenceGap> {
    let (root, foreground) = result.map_err(EvidenceGap::ReadFailed)?;
    if answered.as_ref() != Ok(witness) {
        return Err(EvidenceGap::ServerChanged);
    }
    let root = root.ok_or(EvidenceGap::NoRoot)?;
    match foreground {
        ForegroundProcesses::Listed(pids) => Ok((root, pids)),
        ForegroundProcesses::Unreported => Err(EvidenceGap::Unreported),
    }
}

/// A fresh reading still shows the pair in the pane; their starts, read after it.
fn still_running(
    read: &dyn Fn() -> PaneProcesses,
    os: &dyn ProcessOs,
    witness: &ServerWitness,
    root: u32,
    provider: u32,
) -> Result<(ProcessStart, ProcessStart), EvidenceGap> {
    let (shell, listed) = from_server(read(), witness)?;
    if shell != root || !listed.contains(&provider) {
        return Err(EvidenceGap::Replaced);
    }
    let start = |pid| {
        os.start(pid)
            .map_err(|_| EvidenceGap::ProcessUnreadable(pid))
    };
    Ok((start(root)?, start(provider)?))
}

/// The provider's exec-time environment names this execution's own binding context and
/// no Herdr pane variable.
fn check_environment(
    os: &dyn ProcessOs,
    request: &ProbeRequest,
    provider: u32,
) -> Result<(), EvidenceGap> {
    let environ = os
        .environ(provider)
        .ok()
        .filter(|entries| !entries.is_empty())
        .ok_or(EvidenceGap::EnvUnreadable)?;
    let paths: Vec<&str> = environ
        .iter()
        .filter_map(|entry| entry.strip_prefix(CONTEXT_ENV))
        .collect();
    let path = match paths.as_slice() {
        [] => return Err(EvidenceGap::NonceMissing),
        [path] => Path::new(path),
        _ => return Err(EvidenceGap::OtherNonce("several binding contexts".into())),
    };
    context_names_execution(request.provider, request.nonce, path)
        .map_err(EvidenceGap::OtherNonce)?;
    if let Some(entry) = environ.iter().find(|entry| entry.starts_with("HERDR_")) {
        let key = entry.split('=').next().unwrap_or(entry);
        return Err(EvidenceGap::HerdrEnvPresent(key.to_string()));
    }
    Ok(())
}

#[cfg(test)]
#[path = "pane_probe_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
mod recorded_process_tests {
    use super::*;
    struct Os {
        exists: Result<bool, RestoreUnverified>,
        start: Result<ProcessStart, RestoreUnverified>,
    }
    impl ProcessOs for Os {
        fn exists(&self, _: u32) -> Result<bool, RestoreUnverified> {
            self.exists
        }
        fn start(&self, _: u32) -> Result<ProcessStart, RestoreUnverified> {
            self.start.clone()
        }
        fn parent(&self, _: u32) -> Result<u32, RestoreUnverified> {
            unreachable!()
        }
        fn environ(&self, _: u32) -> Result<Vec<String>, RestoreUnverified> {
            unreachable!()
        }
        fn exec_path(&self, _: u32) -> Option<String> {
            unreachable!()
        }
    }
    #[test]
    fn m1_recorded_provider_four_states_are_distinct() {
        let start = ProcessStart {
            identity: StartIdentity::Darwin {
                seconds: 1000,
                micros: 0,
            },
            wall_clock: SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
        };
        let stamp = ProcessStamp {
            pid: 20,
            start: start_text(start.identity),
        };
        assert_eq!(
            recorded_process(
                &Os {
                    exists: Ok(true),
                    start: Ok(start.clone())
                },
                &stamp
            ),
            RecordedProcess::ExactAlive
        );
        assert_eq!(
            recorded_process(
                &Os {
                    exists: Ok(false),
                    start: Err(RestoreUnverified::ProcessUnreadable)
                },
                &stamp
            ),
            RecordedProcess::ProvenAbsent
        );
        let replacement = ProcessStamp {
            start: "another-start".into(),
            ..stamp.clone()
        };
        assert_eq!(
            recorded_process(
                &Os {
                    exists: Ok(true),
                    start: Ok(start)
                },
                &replacement
            ),
            RecordedProcess::Replaced
        );
        assert_eq!(
            recorded_process(
                &Os {
                    exists: Ok(true),
                    start: Err(RestoreUnverified::ProcessUnreadable)
                },
                &stamp
            ),
            RecordedProcess::Unreadable
        );
        assert_eq!(
            recorded_process(
                &Os {
                    exists: Err(RestoreUnverified::ProcessUnreadable),
                    start: Err(RestoreUnverified::ProcessUnreadable)
                },
                &stamp
            ),
            RecordedProcess::Unreadable
        );
    }
}
