//! Production `HerdrLaunchHost` over the Unix socket. Nothing constructs it yet: the
//! launch selector, `host_for(Herdr)` and Claude input keep refusing Herdr.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::contract::{self, CreatedPane, HerdrTransport, ServerWitness};
use super::model::{HerdrCall, HerdrEndpoint, HerdrRequest};
use super::observe::{self, RESTORE_RESUME_NOT_OFF, RestoreResume, RestoreUnverified};
use super::pane_probe::{
    self, EvidenceGap, HostOs, PROVIDER_WINDOW, PaneProcesses, ProbeRequest, ProcessOs,
};
use super::transport::{HerdrSocketConfig, HerdrSocketTransport};
use super::wire::LineJsonFraming;
use crate::db::dispatched_sessions::hosted_execution::ExpectedExecution;
use crate::services::herdr_launch::{
    EvidenceProbe, HerdrCreateOutcome, HerdrCreateRequest, HerdrLaunchEndpoint, HerdrLaunchHost,
    launch_command_eligible,
};
use crate::services::session_host::model::HostMutation;

pub(crate) struct SocketHerdrLaunchHost {
    endpoints: Vec<(HerdrEndpoint, HerdrSocketTransport)>,
    next_id: AtomicU64,
    read_restore: fn(&HerdrSocketTransport, &HerdrEndpoint) -> RestoreResume,
    os: Box<dyn ProcessOs>,
    provider_window: Duration,
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
    /// No I/O: every call dials its own connection.
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
            os: Box::new(HostOs),
            provider_window: PROVIDER_WINDOW,
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

    /// E7 read afresh, and it must name the server the caller's reading named: a new
    /// server that also reads `Off` is not the one this launch was checked against.
    fn still_off(
        &self,
        (endpoint, transport): &(HerdrEndpoint, HerdrSocketTransport),
        witness: &ServerWitness,
    ) -> Result<(), String> {
        match (self.read_restore)(transport, endpoint) {
            RestoreResume::Off { witness: now } if now == *witness => Ok(()),
            other => Err(format!(
                "{RESTORE_RESUME_NOT_OFF}: {other:?}, checked {witness:?}"
            )),
        }
    }

    /// One read of the pane's root shell and foreground processes on its own connection,
    /// with the server that answered it.
    fn read_processes(&self, transport: &HerdrSocketTransport, pane_id: &str) -> PaneProcesses {
        let call = self.call(HerdrRequest::PaneProcessInfo {
            pane_id: pane_id.to_string(),
        });
        let (outcome, witness) = transport.call(&call);
        let read = contract::foreground_result(&call, outcome, pane_id);
        (read.map_err(|error| format!("{error:?}")), witness)
    }

    /// The create after the local checks: everything from the first socket call on.
    fn create_eligible(&self, request: &HerdrCreateRequest) -> HerdrCreateOutcome {
        let Some(configured) = self.endpoint(&request.endpoint) else {
            return HerdrCreateOutcome::NotSent("endpoint is not configured".into());
        };
        let witness = &request.restore_off_witness;
        if let Err(detail) = self.still_off(configured, witness) {
            return HerdrCreateOutcome::NotSent(detail);
        }
        let transport = &configured.1;
        let call = self.call(HerdrRequest::WorkspaceCreate {
            cwd: request.cwd.to_string_lossy().into_owned(),
            label: request.label.clone(),
            focus: false,
        });
        let outcome = transport.call_with_witness(&call, witness);
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
        if let Err(detail) = self.still_off(configured, witness) {
            return unconfirmed(detail);
        }
        let call = self.call(HerdrRequest::PaneSendInput {
            pane_id: pane.pane_id.clone(),
            text: request.command.clone(),
            keys: vec!["enter".to_string()],
        });
        let outcome = transport.call_with_witness(&call, witness);
        match contract::mutation_result(&call, outcome, &pane.pane_id) {
            Ok(HostMutation::Confirmed) => HerdrCreateOutcome::Created {
                pane_id: pane.pane_id.clone(),
            },
            other => unconfirmed(format!("command input: {other:?}")),
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

    /// Create, check the pane's cwd, read E7 again, then type the command with Enter, each
    /// sent only to the server E7 named; after the create nothing removes or retries the pane.
    fn create(&self, request: &HerdrCreateRequest) -> HerdrCreateOutcome {
        if let Err(detail) = launch_command_eligible(&request.cwd, &request.command) {
            return HerdrCreateOutcome::NotSent(detail);
        }
        self.create_eligible(request)
    }

    /// The pane probe over the location's configured socket and this host's process table.
    fn launch_evidence(&self, probe: &EvidenceProbe) -> Result<ExpectedExecution, EvidenceGap> {
        let location = &probe.location;
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
            return Err(EvidenceGap::UnknownEndpoint);
        };
        let request = ProbeRequest {
            provider: &probe.provider,
            nonce: &probe.execution_nonce,
            launched_at: probe.launched_at,
            witness: &probe.witness,
            window: self.provider_window,
        };
        let read = || self.read_processes(transport, &location.pane_id);
        let probed = pane_probe::probe(&read, self.os.as_ref(), &request);
        tracing::info!(pane = %location.pane_id, ?probed, "herdr launch evidence");
        probed
    }
}

#[cfg(test)]
#[path = "launch_host_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "launch_host_real_tests.rs"]
mod real_tests;
