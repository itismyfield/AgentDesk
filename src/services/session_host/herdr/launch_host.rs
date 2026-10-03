//! Production `HerdrLaunchHost` over the Unix socket. Nothing constructs it yet: the
//! launch selector, `host_for(Herdr)` and Claude input keep refusing Herdr.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::contract::{self, CreatedPane, ForegroundProcesses, HerdrOutcome, HerdrTransport};
use super::model::{HerdrCall, HerdrEndpoint, HerdrRequest};
use super::observe::{self, RESTORE_RESUME_NOT_OFF, RestoreResume, RestoreUnverified};
use super::transport::{HerdrSocketConfig, HerdrSocketTransport};
use super::wire::LineJsonFraming;
use crate::db::dispatched_sessions::hosted_execution::{HostedLocation, ProcessStamp};
use crate::services::herdr_launch::{
    HerdrCreateOutcome, HerdrCreateRequest, HerdrLaunchEndpoint, HerdrLaunchHost,
    launch_command_eligible,
};
use crate::services::session_host::model::HostMutation;

/// How long a new pane is watched for its provider process, and how often.
const PROVIDER_WINDOW: Duration = Duration::from_secs(5);
const PROVIDER_POLL: Duration = Duration::from_millis(200);

/// What the pane's process list says about the provider. Only `One` names a candidate,
/// and a candidate is not evidence until its nonce is checked against the launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderCandidate {
    One {
        root: u32,
        provider: u32,
    },
    /// Still only the shell in the foreground when the window closed.
    NoneYet,
    Multiple(Vec<u32>),
    NoRoot,
    Unreported,
    ReadFailed(String),
    UnknownEndpoint,
}

pub(crate) struct SocketHerdrLaunchHost {
    endpoints: Vec<(HerdrEndpoint, HerdrSocketTransport)>,
    next_id: AtomicU64,
    read_restore: fn(&HerdrSocketTransport, &HerdrEndpoint) -> RestoreResume,
}

fn same_endpoint(
    endpoint: &HerdrEndpoint,
    node: &str,
    key: &str,
    socket: &str,
    session: &str,
) -> bool {
    endpoint.execution_node() == node
        && endpoint.config_key() == key
        && endpoint.socket_path() == Path::new(socket)
        && endpoint.herdr_session() == session
}

/// The pane opened where asked; Herdr silently falls back to HOME for a missing cwd.
fn same_dir(requested: &Path, reported: Option<&str>) -> bool {
    let Some(reported) = reported.map(Path::new) else {
        return false;
    };
    reported == requested || std::fs::canonicalize(requested).is_ok_and(|real| real == reported)
}

impl SocketHerdrLaunchHost {
    /// No I/O: each endpoint's socket is opened by its first E7 reading.
    pub(crate) fn new(endpoints: Vec<HerdrEndpoint>, config: HerdrSocketConfig) -> Self {
        let endpoints = endpoints
            .into_iter()
            .map(|endpoint| {
                let transport = HerdrSocketTransport::new(&endpoint, config, LineJsonFraming);
                (endpoint, transport)
            })
            .collect();
        Self {
            endpoints,
            next_id: AtomicU64::new(1),
            read_restore: observe::read_restore_resume,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_restore_reader(
        self,
        read_restore: fn(&HerdrSocketTransport, &HerdrEndpoint) -> RestoreResume,
    ) -> Self {
        Self {
            read_restore,
            ..self
        }
    }

    fn endpoint(
        &self,
        launch: &HerdrLaunchEndpoint,
    ) -> Option<&(HerdrEndpoint, HerdrSocketTransport)> {
        self.endpoints.iter().find(|(endpoint, _)| {
            same_endpoint(
                endpoint,
                &launch.execution_node,
                &launch.config_key,
                &launch.socket_addr,
                &launch.herdr_session,
            )
        })
    }

    fn call(&self, request: HerdrRequest) -> HerdrCall {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        HerdrCall {
            id: format!("adk-launch-{id}"),
            request,
        }
    }

    /// E7 read again on the connection the caller's reading named, never another one.
    fn still_off(
        &self,
        (endpoint, transport): &(HerdrEndpoint, HerdrSocketTransport),
        generation: u64,
    ) -> Result<(), String> {
        match (self.read_restore)(transport, endpoint) {
            RestoreResume::Off { generation: now } if now == generation => Ok(()),
            other => Err(format!(
                "{RESTORE_RESUME_NOT_OFF}: {other:?} after connection {generation}"
            )),
        }
    }

    /// One read of the pane's root shell and foreground processes.
    fn read_candidate(&self, transport: &HerdrSocketTransport, pane_id: &str) -> ProviderCandidate {
        let call = self.call(HerdrRequest::PaneProcessInfo {
            pane_id: pane_id.to_string(),
        });
        let (outcome, _): (HerdrOutcome, u64) = transport.call(&call);
        let (root, foreground) = match contract::foreground_result(&call, outcome, pane_id) {
            Ok(read) => read,
            Err(error) => return ProviderCandidate::ReadFailed(format!("{error:?}")),
        };
        let Some(root) = root else {
            return ProviderCandidate::NoRoot;
        };
        let ForegroundProcesses::Listed(pids) = foreground else {
            return ProviderCandidate::Unreported;
        };
        let others: Vec<u32> = pids.into_iter().filter(|pid| *pid != root).collect();
        match others.as_slice() {
            [] => ProviderCandidate::NoneYet,
            [provider] => ProviderCandidate::One {
                root,
                provider: *provider,
            },
            _ => ProviderCandidate::Multiple(others),
        }
    }

    /// Polls the new pane until one non-shell foreground process shows up or the window
    /// closes; any other answer is returned with the reason it was not a candidate.
    pub(crate) fn provider_candidate(&self, location: &HostedLocation) -> ProviderCandidate {
        let found = self.endpoints.iter().find(|(endpoint, _)| {
            same_endpoint(
                endpoint,
                &location.execution_node,
                &location.endpoint_config_key,
                &location.socket_addr,
                &location.named_session,
            )
        });
        let Some((_, transport)) = found else {
            return ProviderCandidate::UnknownEndpoint;
        };
        let deadline = Instant::now() + PROVIDER_WINDOW;
        loop {
            let candidate = self.read_candidate(transport, &location.pane_id);
            if candidate != ProviderCandidate::NoneYet || Instant::now() + PROVIDER_POLL >= deadline
            {
                return candidate;
            }
            thread::sleep(PROVIDER_POLL);
        }
    }
}

impl HerdrLaunchHost for SocketHerdrLaunchHost {
    fn restore_resume(&self, launch: &HerdrLaunchEndpoint) -> RestoreResume {
        match self.endpoint(launch) {
            Some((endpoint, transport)) => (self.read_restore)(transport, endpoint),
            None => RestoreResume::Unverified(RestoreUnverified::NotBootstrapped),
        }
    }

    /// Create, check the pane's cwd, read E7 again, then type the command with Enter, all
    /// on the connection E7 read. After the create, no failure removes or retries the pane.
    fn create(&self, request: &HerdrCreateRequest) -> HerdrCreateOutcome {
        if let Err(detail) = launch_command_eligible(&request.cwd, &request.command) {
            return HerdrCreateOutcome::NotSent(detail);
        }
        let Some(configured) = self.endpoint(&request.endpoint) else {
            return HerdrCreateOutcome::NotSent("endpoint is not configured".into());
        };
        let generation = request.restore_off_generation;
        if let Err(detail) = self.still_off(configured, generation) {
            return HerdrCreateOutcome::NotSent(detail);
        }
        let transport = &configured.1;
        let call = self.call(HerdrRequest::WorkspaceCreate {
            cwd: request.cwd.to_string_lossy().into_owned(),
            label: request.label.clone(),
            focus: false,
        });
        let (outcome, _) = transport.call_on(&call, generation);
        let pane = match contract::created_result(&call, outcome) {
            CreatedPane::Root(pane) => pane,
            CreatedPane::NotSent(detail) => return HerdrCreateOutcome::NotSent(detail),
            CreatedPane::Indeterminate(detail) => return HerdrCreateOutcome::Indeterminate(detail),
        };
        let unconfirmed = |detail: String| HerdrCreateOutcome::CreatedUnconfirmed {
            pane_id: pane.pane_id.clone(),
            detail,
        };
        if !same_dir(&request.cwd, pane.cwd.as_deref()) {
            return unconfirmed(format!("pane opened in {:?}", pane.cwd));
        }
        if let Err(detail) = self.still_off(configured, generation) {
            return unconfirmed(detail);
        }
        let call = self.call(HerdrRequest::PaneSendInput {
            pane_id: pane.pane_id.clone(),
            text: request.command.clone(),
            keys: vec!["enter".to_string()],
        });
        let (outcome, _) = transport.call_on(&call, generation);
        match contract::mutation_result(&call, outcome, &pane.pane_id) {
            Ok(HostMutation::Confirmed) => HerdrCreateOutcome::Created {
                pane_id: pane.pane_id.clone(),
            },
            other => unconfirmed(format!("command input: {other:?}")),
        }
    }

    /// No stamps until the candidate's nonce is checked; the reading is logged, not stored.
    fn launch_evidence(&self, location: &HostedLocation) -> Option<(ProcessStamp, ProcessStamp)> {
        let candidate = self.provider_candidate(location);
        tracing::info!(pane = %location.pane_id, ?candidate, "herdr provider candidate (not evidence)");
        None
    }
}

#[cfg(test)]
#[path = "launch_host_tests.rs"]
mod tests;
