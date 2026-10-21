//! Socket launch adapter; legacy E7 and user-default permits remain separate.
#![cfg_attr(not(test), allow(dead_code))]

use super::policy::permit::{CreatePermit, ExistingMutationEvidence, LaunchAdmissionGrant};
use super::policy::{LocalChecker, PeerSource, ProofCache, Unverified, peer::SocketPeerSource};
use crate::config::session_hosts::EndpointMode;
use crate::services::herdr_launch::policy::{PermitFuture, UserDefaultCreateRequest};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
    endpoints: Vec<(HerdrEndpoint, Arc<HerdrSocketTransport>)>,
    policy: ProofCache,
    sources: Vec<Arc<dyn PeerSource>>,
    witnessed: Mutex<HashMap<HerdrLaunchEndpoint, ServerWitness>>,
    grants: Mutex<HashMap<(HerdrLaunchEndpoint, String), LaunchAdmissionGrant>>,
    next_id: AtomicU64,
    read_restore: fn(&HerdrSocketTransport, &HerdrEndpoint) -> RestoreResume,
    os: Box<dyn ProcessOs>,
    provider_window: Duration,
}

struct PaneCommand<'a> {
    endpoint: &'a HerdrLaunchEndpoint,
    label: &'a str,
    cwd: &'a Path,
    command: &'a str,
    witness: &'a ServerWitness,
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
                (endpoint, Arc::new(transport))
            })
            .collect();
        let endpoints: Vec<(HerdrEndpoint, Arc<HerdrSocketTransport>)> = endpoints;
        let sources = endpoints
            .iter()
            .map(|(endpoint, transport)| {
                let hello = transport.clone();
                let witness = transport.clone();
                Arc::new(SocketPeerSource::new(
                    endpoint.socket_path().into(),
                    move || hello.hello().map_err(|_| Unverified::Peer),
                    move || witness.server_witness().map_err(|_| Unverified::Peer),
                )) as Arc<dyn PeerSource>
            })
            .collect();
        Self {
            endpoints,
            policy: ProofCache::default(),
            sources,
            witnessed: Mutex::default(),
            grants: Mutex::default(),
            next_id: AtomicU64::new(1),
            read_restore: observe::read_restore_resume,
            os: Box::new(HostOs),
            provider_window: PROVIDER_WINDOW,
        }
    }

    fn endpoint(
        &self,
        launch: &HerdrLaunchEndpoint,
    ) -> Option<&(HerdrEndpoint, Arc<HerdrSocketTransport>)> {
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

    fn source(&self, launch: &HerdrLaunchEndpoint) -> Option<Arc<dyn PeerSource>> {
        self.endpoints
            .iter()
            .position(|(e, _)| {
                same_endpoint(
                    e,
                    &launch.execution_node,
                    &launch.config_key,
                    &launch.socket_addr,
                    &launch.herdr_session,
                )
            })
            .and_then(|index| self.sources.get(index))
            .cloned()
    }

    pub(crate) fn mutation_evidence(
        &self,
        endpoint: &HerdrEndpoint,
        nonce: &str,
    ) -> Result<ExistingMutationEvidence, Unverified> {
        let key = HerdrLaunchEndpoint {
            execution_node: endpoint.execution_node().into(),
            config_key: endpoint.config_key().into(),
            socket_addr: endpoint.socket_path().to_string_lossy().into_owned(),
            herdr_session: endpoint.herdr_session().into(),
        };
        let witness = self
            .witnessed
            .lock()
            .map_err(|_| Unverified::Cache)?
            .get(&key)
            .cloned();
        let startup = witness
            .map(|w| self.policy.startup_for(&w))
            .transpose()?
            .flatten();
        let grant = self
            .grants
            .lock()
            .map_err(|_| Unverified::Cache)?
            .get(&(key, nonce.into()))
            .cloned();
        Ok(ExistingMutationEvidence { startup, grant })
    }

    #[cfg(test)]
    pub(crate) fn poison_grants_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = self.grants.lock().unwrap();
            panic!("injected grant cache failure");
        }));
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
        (endpoint, transport): &(HerdrEndpoint, Arc<HerdrSocketTransport>),
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

    /// Both policies share the two send sites; each supplies its own authorization fence.
    fn create_checked(
        &self,
        request: PaneCommand<'_>,
        fence: impl Fn(
            &(HerdrEndpoint, Arc<HerdrSocketTransport>),
            &ServerWitness,
        ) -> Result<(), String>,
    ) -> HerdrCreateOutcome {
        let Some(configured) = self.endpoint(request.endpoint) else {
            return HerdrCreateOutcome::NotSent("endpoint is not configured".into());
        };
        let witness = request.witness;
        if let Err(detail) = fence(configured, witness) {
            return HerdrCreateOutcome::NotSent(detail);
        }
        let transport = &configured.1;
        let call = self.call(HerdrRequest::WorkspaceCreate {
            cwd: request.cwd.to_string_lossy().into_owned(),
            label: request.label.to_string(),
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
        if !same_dir(request.cwd, pane.cwd.as_deref()) {
            return unconfirmed(format!("pane opened in {:?}", pane.cwd));
        }
        if let Err(detail) = fence(configured, witness) {
            return unconfirmed(detail);
        }
        let call = self.call(HerdrRequest::PaneSendInput {
            pane_id: pane.pane_id.clone(),
            text: request.command.to_string(),
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
    fn create_eligible(&self, request: &HerdrCreateRequest) -> HerdrCreateOutcome {
        self.create_checked(
            PaneCommand {
                endpoint: &request.endpoint,
                label: &request.label,
                cwd: &request.cwd,
                command: &request.command,
                witness: &request.restore_off_witness,
            },
            |configured, witness| self.still_off(configured, witness),
        )
    }
}

impl HerdrLaunchHost for SocketHerdrLaunchHost {
    fn remember_grant(&self, grant: LaunchAdmissionGrant) {
        if let Ok(mut grants) = self.grants.lock() {
            grants.insert((grant.endpoint().clone(), grant.nonce().into()), grant);
        }
    }

    fn mode(&self, launch: &HerdrLaunchEndpoint) -> EndpointMode {
        self.endpoint(launch)
            .map_or(EndpointMode::LegacyDedicated, |(e, _)| e.mode())
    }

    fn create_permit<'a>(&'a self, launch: &'a HerdrLaunchEndpoint) -> PermitFuture<'a> {
        Box::pin(async move {
            if self.mode(launch) != EndpointMode::UserDefault {
                return Err("endpoint mode is not user_default".into());
            }
            let source = self.source(launch).ok_or("endpoint is not configured")?;
            let proof = self
                .policy
                .next_restart(source, &LocalChecker)
                .await
                .map_err(|why| format!("next_restart_policy_unverified: {why:?}"))?;
            self.witnessed
                .lock()
                .map_err(|_| "policy cache unavailable")?
                .insert(launch.clone(), proof.witness().clone());
            CreatePermit::new(launch.clone(), proof).map_err(|why| format!("{why:?}"))
        })
    }

    fn current_witness(&self, launch: &HerdrLaunchEndpoint) -> Result<ServerWitness, String> {
        self.endpoint(launch)
            .ok_or("endpoint is not configured")?
            .1
            .server_witness()
            .map_err(|why| format!("{why:?}"))
    }

    fn create_user_default(&self, request: &UserDefaultCreateRequest) -> HerdrCreateOutcome {
        if let Err(detail) = launch_command_eligible(&request.cwd, &request.command) {
            return HerdrCreateOutcome::NotSent(detail);
        }
        if self.endpoint(&request.endpoint).is_none() {
            return HerdrCreateOutcome::NotSent("endpoint is not configured".into());
        }
        let Some(source) = self.source(&request.endpoint) else {
            return HerdrCreateOutcome::NotSent("policy source unavailable".into());
        };
        self.create_checked(
            PaneCommand {
                endpoint: &request.endpoint,
                label: &request.label,
                cwd: &request.cwd,
                command: &request.command,
                witness: request.permit.witness(),
            },
            |_, _| {
                request
                    .permit
                    .fence(&request.endpoint, source.as_ref())
                    .map_err(|why| format!("policy fence: {why:?}"))
            },
        )
    }

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
