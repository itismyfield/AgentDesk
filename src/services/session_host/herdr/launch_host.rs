//! Socket launch adapter; legacy E7 and user-default permits remain separate.

use super::policy::grant_store::GrantRestoration;
use super::policy::permit::{
    CreatePermit, ExistingMutationEvidence, ExistingMutationPermit, LaunchAdmissionGrant,
};
use super::policy::{LocalChecker, PeerSource, ProofCache, Unverified, peer::SocketPeerSource};
use crate::config::session_hosts::EndpointMode;
use crate::services::herdr_launch::policy::{PermitFuture, UserDefaultCreateRequest};
use std::collections::{HashMap, HashSet};
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
use crate::services::herdr_launch::unplaced::{ChannelPane, Residue, candidate_label};
use crate::services::herdr_launch::{
    EvidenceProbe, HerdrCreateOutcome, HerdrCreateRequest, HerdrLaunchEndpoint, HerdrLaunchHost,
    launch_command_eligible,
};
use crate::services::session_host::herdr_gate::residue;
use crate::services::session_host::model::HostMutation;

/// What serves an endpoint now, against the server a nonce's grant admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GrantedServer {
    /// The granted server answers, or no grant names one.
    Kept,
    /// No server answered.
    Unanswered,
    Replaced(ReplacedServer),
}

/// Another server than the granted one answers; `startup` says `observe_policy` proved its policy
/// from its start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplacedServer {
    pub(crate) current: ServerWitness,
    pub(crate) startup: bool,
}

pub(crate) struct SocketHerdrLaunchHost {
    endpoints: Vec<(HerdrEndpoint, Arc<HerdrSocketTransport>)>,
    policy: ProofCache,
    sources: Vec<Arc<dyn PeerSource>>,
    witnessed: Mutex<HashMap<HerdrLaunchEndpoint, ServerWitness>>,
    grants: Mutex<GrantCache>,
    grant_store: super::policy::grant_store::FileGrantStore,
    next_id: AtomicU64,
    read_restore: fn(&HerdrSocketTransport, &HerdrEndpoint) -> RestoreResume,
    os: Box<dyn ProcessOs>,
    provider_window: Duration,
}

struct CachedGrant {
    grant: LaunchAdmissionGrant,
    remembered: bool,
}

#[derive(Default)]
struct GrantCache {
    grants: HashMap<(HerdrLaunchEndpoint, String), CachedGrant>,
    failed: HashSet<HerdrLaunchEndpoint>,
    unknown_failure: bool,
}

impl GrantCache {
    fn restored(prepared: Result<GrantRestoration, Unverified>) -> Self {
        let restored = match prepared {
            Ok(restored) => restored,
            Err(_) => {
                return Self {
                    unknown_failure: true,
                    ..Self::default()
                };
            }
        };
        let mut cache = Self {
            unknown_failure: restored.unknown_failure,
            ..Self::default()
        };
        for (endpoint, grants) in restored.by_endpoint {
            match grants {
                Ok(grants) => {
                    for grant in grants {
                        cache.grants.insert(
                            (endpoint.clone(), grant.nonce().into()),
                            CachedGrant {
                                grant,
                                remembered: false,
                            },
                        );
                    }
                }
                Err(_) => {
                    cache.failed.insert(endpoint);
                }
            }
        }
        cache
    }
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

/// The policy's view of the server behind `socket`, read through `transport`; no I/O until used.
fn peer_source(socket: &Path, transport: &Arc<HerdrSocketTransport>) -> Arc<dyn PeerSource> {
    let (hello, witness) = (transport.clone(), transport.clone());
    let source = SocketPeerSource::new(
        socket.into(),
        move || {
            hello.hello().map_err(|why| match why {
                RestoreUnverified::VersionNotVerified => Unverified::Unsupported,
                _ => Unverified::Peer,
            })
        },
        move || witness.server_witness().map_err(|_| Unverified::Peer),
    );
    #[cfg(target_os = "macos")]
    let source = {
        let generation = transport.clone();
        source.with_generation(move || {
            generation
                .peer_generation()
                .map_err(|_| Unverified::Changed)
        })
    };
    Arc::new(source)
}

/// The same peer source a launch host gives consent, for a one-off read-only probe of `socket`.
pub(crate) fn probe_source(socket: &Path) -> Result<Arc<dyn PeerSource>, String> {
    let endpoint = HerdrEndpoint::new("probe", "probe", socket, "probe")
        .map_err(|error| format!("herdr probe socket {}: {error:?}", socket.display()))?;
    let transport =
        HerdrSocketTransport::new(&endpoint, HerdrSocketConfig::default(), LineJsonFraming);
    Ok(peer_source(socket, &Arc::new(transport)))
}

/// The pid of the server on `socket` that answered a hello within `wait`; a socket that only
/// accepts is no server yet.
pub(crate) fn accepting_pid(socket: &Path, wait: Duration) -> Option<u32> {
    let endpoint = HerdrEndpoint::new("autostart", "autostart", socket, "autostart").ok()?;
    let config = HerdrSocketConfig {
        io_timeout: wait,
        read_deadline: wait,
        ..HerdrSocketConfig::default()
    };
    let transport = HerdrSocketTransport::new(&endpoint, config, LineJsonFraming);
    transport.hello().ok().map(|hello| hello.witness.pid)
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
            .map(|(endpoint, transport)| peer_source(endpoint.socket_path(), transport))
            .collect();
        let grants = GrantCache::restored(super::policy::boot_grants::prepared());
        Self {
            endpoints,
            policy: ProofCache::default(),
            sources,
            witnessed: Mutex::default(),
            grants: Mutex::new(grants),
            grant_store: super::policy::boot_grants::store(),
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

    /// The current next-restart policy of `launch`'s server, recorded for health and remembered
    /// as the witness later mutations of this endpoint compare against.
    pub(crate) async fn observe_policy(
        &self,
        launch: &HerdrLaunchEndpoint,
    ) -> Result<super::policy::NextRestartProof, String> {
        let source = self.source(launch).ok_or("endpoint is not configured")?;
        let (checked, _) = self.observe_policy_from(launch, source).await;
        checked.map_err(|why| format!("next_restart_policy_unverified: {why:?}"))
    }

    /// `observe_policy` through `source`, with the health observation it recorded.
    pub(crate) async fn observe_policy_from(
        &self,
        launch: &HerdrLaunchEndpoint,
        source: Arc<dyn PeerSource>,
    ) -> (
        Result<super::policy::NextRestartProof, Unverified>,
        serde_json::Value,
    ) {
        let mut checked = self.policy.next_restart(source, &LocalChecker).await;
        if let Ok(proof) = &checked {
            match self.witnessed.lock() {
                Ok(mut witnessed) => _ = witnessed.insert(launch.clone(), proof.witness().clone()),
                Err(_) => checked = Err(Unverified::Cache),
            }
        }
        let socket = Path::new(&launch.socket_addr);
        let observation = super::policy::diagnostics::record(socket, &self.policy, &checked);
        (checked, observation)
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

    pub(crate) fn consent_source(
        &self,
        endpoint: &HerdrLaunchEndpoint,
    ) -> Option<Arc<dyn PeerSource>> {
        self.source(endpoint)
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
        let cache = self.grants.lock().map_err(|_| Unverified::Cache)?;
        let grant = cache.grants.get(&(key.clone(), nonce.into()));
        // Async owners remember only grants whose durable save/load completed this process.
        if (cache.unknown_failure || cache.failed.contains(&key))
            && !grant.is_some_and(|grant| grant.remembered)
        {
            return Err(Unverified::Cache);
        }
        let grant = grant.map(|grant| grant.grant.clone());
        Ok(ExistingMutationEvidence { startup, grant })
    }

    /// The server on `launch` against the one `nonce`'s grant admitted; reads no policy.
    pub(crate) fn granted_server(
        &self,
        launch: &HerdrLaunchEndpoint,
        nonce: &str,
    ) -> GrantedServer {
        let Some((endpoint, transport)) = self.endpoint(launch) else {
            return GrantedServer::Kept;
        };
        let Ok(current) = transport.server_witness() else {
            return GrantedServer::Unanswered;
        };
        let Ok(evidence) = self.mutation_evidence(endpoint, nonce) else {
            return GrantedServer::Kept;
        };
        let granted = evidence
            .grant
            .as_ref()
            .map(|g| g.witness_for(launch, nonce));
        match granted {
            Some(Ok(granted)) if *granted != current => {
                let startup = evidence.startup.is_some_and(|p| p.witness() == &current);
                GrantedServer::Replaced(ReplacedServer { current, startup })
            }
            _ => GrantedServer::Kept,
        }
    }

    #[cfg(test)]
    pub(crate) fn poison_grants_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = self.grants.lock().unwrap();
            panic!("injected grant cache failure");
        }));
    }

    #[cfg(test)]
    pub(crate) fn seed_witness_for_test(&self, endpoint: &HerdrEndpoint, witness: ServerWitness) {
        self.witnessed.lock().unwrap().insert(
            HerdrLaunchEndpoint {
                execution_node: endpoint.execution_node().into(),
                config_key: endpoint.config_key().into(),
                socket_addr: endpoint.socket_path().to_string_lossy().into_owned(),
                herdr_session: endpoint.herdr_session().into(),
            },
            witness,
        );
    }
    #[cfg(test)]
    pub(crate) fn poison_witnessed_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = self.witnessed.lock().unwrap();
            panic!("injected witnessed cache failure");
        }));
    }
    #[cfg(test)]
    pub(crate) fn poison_proof_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.policy.poison_for_test()
        }));
    }
    #[cfg(test)]
    pub(crate) fn with_prepared_for_test(
        mut self,
        prepared: Result<Vec<LaunchAdmissionGrant>, Unverified>,
    ) -> Self {
        self.grants = Mutex::new(GrantCache::restored(
            prepared.map(GrantRestoration::from_grants),
        ));
        self
    }

    #[cfg(test)]
    pub(crate) fn with_restored_for_test(mut self, restored: GrantRestoration) -> Self {
        self.grants = Mutex::new(GrantCache::restored(Ok(restored)));
        self
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
    fn grants(&self) -> &dyn crate::services::herdr_launch::policy::LaunchGrantStore {
        &self.grant_store
    }

    fn remember_grant(&self, grant: LaunchAdmissionGrant) {
        if let Ok(mut grants) = self.grants.lock() {
            grants.grants.insert(
                (grant.endpoint().clone(), grant.nonce().into()),
                CachedGrant {
                    grant,
                    remembered: true,
                },
            );
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
            // A start only makes a server answer; the policy below still decides the permit.
            super::autostart::spawn::ensure(launch).await;
            let proof = self.observe_policy(launch).await?;
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
    /// sent only to the server E7 named; the launch itself never removes or retries the pane.
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

    /// LegacyDedicated: a fresh E7 `Off` reading. UserDefault: the server execution `nonce`'s grant
    /// or the startup proof admits, as for any write to an existing pane.
    fn unplaced_witness(
        &self,
        launch: &HerdrLaunchEndpoint,
        nonce: &str,
    ) -> Result<ServerWitness, String> {
        let configured = self.endpoint(launch).ok_or("endpoint is not configured")?;
        let (endpoint, transport) = configured;
        if endpoint.mode() != EndpointMode::UserDefault {
            return match (self.read_restore)(transport, endpoint) {
                RestoreResume::Off { witness } => Ok(witness),
                other => Err(format!("{RESTORE_RESUME_NOT_OFF}: {other:?}")),
            };
        }
        let evidence = self.mutation_evidence(endpoint, nonce);
        let evidence = evidence.map_err(|why| format!("mutation evidence: {why:?}"))?;
        let current = transport
            .server_witness()
            .map_err(|why| format!("{why:?}"))?;
        ExistingMutationPermit::issue(launch, nonce, current, &evidence)
            .map(|permit| permit.witness().clone())
            .map_err(|why| format!("existing mutation permit: {why:?}"))
    }

    fn channel_panes(
        &self,
        launch: &HerdrLaunchEndpoint,
        logical_key: &str,
        witness: &ServerWitness,
    ) -> Result<Vec<ChannelPane>, String> {
        let (_, transport) = self.endpoint(launch).ok_or("endpoint is not configured")?;
        let snapshot = residue::snapshot_on(transport.as_ref(), witness, &self.next_id)?;
        let workspaces = snapshot.workspaces.as_deref().unwrap_or_default();
        let mut panes = Vec::new();
        for workspace in workspaces
            .iter()
            .filter(|workspace| candidate_label(logical_key, &workspace.label))
        {
            let pristine = workspace.pane_count == 1 && workspace.tab_count == 1;
            let held = snapshot
                .panes
                .iter()
                .filter(|pane| pane.workspace_id == workspace.workspace_id);
            for pane in held {
                let reading = self.read_processes(transport, &pane.pane_id);
                panes.push(ChannelPane {
                    pane_id: pane.pane_id.clone(),
                    label: workspace.label.clone(),
                    pristine,
                    occupant: pane_probe::occupant(reading, self.os.as_ref(), witness),
                });
            }
        }
        Ok(panes)
    }

    /// Removes the residue only while the endpoint's write policy still names its server.
    fn dispose_residue(
        &self,
        launch: &HerdrLaunchEndpoint,
        nonce: &str,
        residue: &Residue,
    ) -> Result<(), String> {
        let (_, transport) = self.endpoint(launch).ok_or("endpoint is not configured")?;
        if self.unplaced_witness(launch, nonce)? != residue.witness {
            return Err("the endpoint's server changed".into());
        }
        let shell = residue::CreateResidue {
            pane_id: residue.pane_id.clone(),
            label: residue.label.clone(),
            root: residue.root.clone(),
        };
        let os = self.os.as_ref();
        residue::dispose_create_residue(
            transport.as_ref(),
            os,
            &residue.witness,
            &self.next_id,
            &shell,
        )
    }
}

#[cfg(test)]
#[path = "launch_host_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "launch_host_real_tests.rs"]
mod real_tests;

#[cfg(all(test, unix))]
#[path = "launch_host_t19_real_tests.rs"]
mod t19_real_tests;
