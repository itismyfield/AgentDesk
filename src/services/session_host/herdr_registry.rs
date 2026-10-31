//! This node's Herdr endpoints from the boot `session_hosts` section: socket transports, the
//! read-only `host_for(Herdr)` host, the launch host and gated targets. Building dials nothing.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::PathBuf;
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::OnceLock;

use super::herdr::contract::{HerdrOutcome, HerdrTransport, ServerHello, ServerWitness, Witnessed};
#[cfg(unix)]
use super::herdr::launch_host::SocketHerdrLaunchHost;
use super::herdr::model::{HerdrCall, HerdrEndpoint};
use super::herdr::observe::RestoreUnverified;
use super::herdr::policy::permit::ExistingMutationEvidence;
use super::herdr_gate::{HerdrPaneView, HerdrTarget};
use super::herdr_host::HerdrHost;
use super::model::{
    HostCapabilities, HostError, HostKind, HostLiveness, HostMutation, HostPresence, HostRefusal,
    HostSessionRef,
};
use super::traits::InteractiveSessionHost;
use crate::config::session_hosts::BootSessionHosts;
use crate::db::dispatched_sessions::hosted_execution::HostedExecution;

/// One socket transport shared by an endpoint's host and its input targets, so their
/// mutations stay one at a time.
#[derive(Clone)]
struct Shared(Arc<dyn HerdrTransport>);

impl HerdrTransport for Shared {
    fn call(&self, call: &HerdrCall) -> (HerdrOutcome, Witnessed) {
        self.0.call(call)
    }

    fn call_with_witness(&self, call: &HerdrCall, expected: &ServerWitness) -> HerdrOutcome {
        self.0.call_with_witness(call, expected)
    }

    fn hello(&self) -> Result<ServerHello, RestoreUnverified> {
        self.0.hello()
    }

    fn server_witness(&self) -> Witnessed {
        self.0.server_witness()
    }
}

/// The endpoint host for reads only: input reaches a pane through its `HerdrTarget` gate.
pub(crate) struct ReadOnlyHerdrHost(HerdrHost<Shared>);

const INPUT_GATED: &str = "herdr_input_goes_through_its_mutation_gate";

fn gated() -> Result<HostMutation, HostError> {
    Ok(HostMutation::Refused(HostRefusal::Precondition(
        INPUT_GATED.to_string(),
    )))
}

impl InteractiveSessionHost for ReadOnlyHerdrHost {
    fn kind(&self) -> HostKind {
        self.0.kind()
    }

    fn capabilities(&self) -> HostCapabilities {
        HostCapabilities {
            send_text: false,
            send_keys: false,
            interrupt: false,
            ..self.0.capabilities()
        }
    }

    fn presence(&self, session: HostSessionRef<'_>) -> HostPresence {
        self.0.presence(session)
    }

    fn liveness(&self, session: HostSessionRef<'_>) -> HostLiveness {
        self.0.liveness(session)
    }

    fn send_text(&self, _: HostSessionRef<'_>, _: &str) -> Result<HostMutation, HostError> {
        gated()
    }

    fn send_keys(&self, _: HostSessionRef<'_>, _: &[&str]) -> Result<HostMutation, HostError> {
        gated()
    }

    fn interrupt(&self, _: HostSessionRef<'_>) -> Result<HostMutation, HostError> {
        gated()
    }

    fn capture_screen(
        &self,
        session: HostSessionRef<'_>,
        scroll_back: i32,
    ) -> Result<String, HostError> {
        self.0.capture_screen(session, scroll_back)
    }

    fn current_working_dir(
        &self,
        session: HostSessionRef<'_>,
    ) -> Result<Option<PathBuf>, HostError> {
        self.0.current_working_dir(session)
    }

    fn execution_pid(&self, session: HostSessionRef<'_>) -> Result<Option<u32>, HostError> {
        self.0.execution_pid(session)
    }
}

struct Registered {
    endpoint: HerdrEndpoint,
    transport: Shared,
    host: ReadOnlyHerdrHost,
}

#[cfg(test)]
type TestReads = (
    fn(&dyn HerdrTransport, &HerdrEndpoint) -> super::herdr::observe::RestoreResume,
    Arc<dyn super::herdr::pane_probe::ProcessOs>,
);

pub(crate) struct HerdrRegistry {
    endpoints: Vec<Registered>,
    #[cfg(unix)]
    launch: Option<SocketHerdrLaunchHost>,
    /// E7 and process reads every target gets instead of the OS ones.
    #[cfg(test)]
    reads: Option<TestReads>,
}

impl HerdrRegistry {
    const EMPTY: Self = Self {
        endpoints: Vec::new(),
        #[cfg(unix)]
        launch: None,
        #[cfg(test)]
        reads: None,
    };

    /// The endpoints whose `execution_node` is this node. No socket is dialled here.
    pub(crate) fn build(boot: &BootSessionHosts) -> Self {
        let Some(local) = boot.local_node() else {
            return Self::EMPTY;
        };
        let endpoints: Vec<HerdrEndpoint> = boot
            .config()
            .herdr
            .endpoints
            .iter()
            .filter(|(_, configured)| configured.execution_node.trim() == local)
            .filter_map(|(key, configured)| {
                HerdrEndpoint::new(
                    local,
                    key,
                    &configured.socket_path,
                    &configured.herdr_session,
                )
                .and_then(|endpoint| match configured.mode {
                    crate::config::session_hosts::EndpointMode::LegacyDedicated => {
                        endpoint.with_herdr_home(&configured.herdr_home)
                    }
                    crate::config::session_hosts::EndpointMode::UserDefault => Ok(endpoint),
                })
                .map(|endpoint| endpoint.with_mode(configured.mode))
                .ok()
            })
            .collect();
        Self::over(endpoints)
    }

    #[cfg(unix)]
    fn over(endpoints: Vec<HerdrEndpoint>) -> Self {
        use super::herdr::transport::{HerdrSocketConfig, HerdrSocketTransport};
        use super::herdr::wire::LineJsonFraming;
        if endpoints.is_empty() {
            return Self::EMPTY;
        }
        let config = HerdrSocketConfig::default();
        let launch = SocketHerdrLaunchHost::new(endpoints.clone(), config);
        let endpoints = endpoints
            .into_iter()
            .map(|endpoint| {
                let socket = HerdrSocketTransport::new(&endpoint, config, LineJsonFraming);
                let transport = Shared(Arc::new(socket));
                Registered::new(endpoint, transport)
            })
            .collect();
        Self {
            endpoints,
            launch: Some(launch),
            #[cfg(test)]
            reads: None,
        }
    }

    /// No Herdr transport exists off Unix, so no endpoint registers there.
    #[cfg(not(unix))]
    fn over(_endpoints: Vec<HerdrEndpoint>) -> Self {
        Self::EMPTY
    }

    #[cfg(test)]
    pub(crate) fn with_transport(
        endpoint: HerdrEndpoint,
        transport: Arc<dyn HerdrTransport>,
    ) -> Self {
        Self {
            endpoints: vec![Registered::new(endpoint, Shared(transport))],
            ..Self::EMPTY
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_launch_host(mut self, host: SocketHerdrLaunchHost) -> Self {
        self.launch = Some(host);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_reads(self, reads: TestReads) -> Self {
        Self {
            reads: Some(reads),
            ..self
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// Reads whole registered endpoints. A changed server or exceeded budget is not a census.
    pub(crate) fn read_pane_census(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, Vec<String>)>, String> {
        use super::herdr::model::{HERDR_PROTOCOL, HerdrRequest, HerdrResult};
        let mut census = Vec::new();
        let mut count = self.endpoints.len();
        for registered in &self.endpoints {
            if count > limit {
                return Err("Herdr census budget exceeded".into());
            }
            let before = registered
                .transport
                .server_witness()
                .map_err(|e| format!("{e:?}"))?;
            if before.socket != registered.endpoint.socket_path() {
                return Err("Herdr census socket witness mismatch".into());
            }
            let call = HerdrCall {
                id: uuid::Uuid::new_v4().to_string(),
                request: HerdrRequest::SessionSnapshot {},
            };
            let (outcome, answered) = registered.transport.call(&call);
            if answered.as_ref() != Ok(&before)
                || registered.transport.server_witness().as_ref() != Ok(&before)
            {
                return Err("Herdr census server changed".into());
            }
            let reply = outcome.map_err(|e| format!("{e:?}"))?;
            if reply.id != call.id {
                return Err("Herdr census reply mismatch".into());
            }
            let HerdrResult::SessionSnapshot { snapshot } =
                reply.body.map_err(|e| format!("{e:?}"))?
            else {
                return Err("Herdr census snapshot unavailable".into());
            };
            if snapshot.protocol != HERDR_PROTOCOL {
                return Err("Herdr census protocol mismatch".into());
            }
            count = count.saturating_add(snapshot.panes.len());
            let mut panes: Vec<_> = snapshot
                .panes
                .into_iter()
                .map(|pane| pane.pane_id)
                .collect();
            panes.sort();
            if panes.iter().any(|pane| pane.is_empty())
                || panes.windows(2).any(|pair| pair[0] == pair[1])
            {
                return Err("Herdr census invalid pane identity".into());
            }
            if count > limit {
                #[cfg(test)]
                if std::env::var("ADK_CODEX_OBSERVATION_MUTANT").as_deref() == Ok("herdr_budget") {
                    panes.truncate(limit.saturating_sub(self.endpoints.len()));
                    census.push((
                        format!("{}:{before:?}", registered.endpoint.config_key()),
                        panes,
                    ));
                    return Ok(census);
                }
                return Err("Herdr census budget exceeded".into());
            }
            census.push((
                format!("{}:{before:?}", registered.endpoint.config_key()),
                panes,
            ));
        }
        census.sort();
        Ok(census)
    }

    /// The one local endpoint's read-only host; none, or several, give no host.
    pub(crate) fn sole_host(&self) -> Option<&dyn InteractiveSessionHost> {
        match self.endpoints.as_slice() {
            [only] => Some(&only.host),
            _ => None,
        }
    }

    #[cfg(unix)]
    pub(crate) fn launch_host(&self) -> Option<&SocketHerdrLaunchHost> {
        self.launch.as_ref()
    }

    /// The gated input target of a stored execution located on a registered endpoint.
    pub(crate) fn target(&self, stored: &HostedExecution) -> Option<HerdrTarget> {
        #[cfg(unix)]
        let evidence = if let Some(location) = &stored.location {
            self.endpoints
                .iter()
                .find(|r| {
                    r.endpoint.config_key() == location.endpoint_config_key
                        && r.endpoint.execution_node() == location.execution_node
                })
                .filter(|r| {
                    r.endpoint.mode() == crate::config::session_hosts::EndpointMode::UserDefault
                })
                .and_then(|r| {
                    self.launch.as_ref().map(|launch| {
                        launch.mutation_evidence(&r.endpoint, &stored.execution_nonce)
                    })
                })
                .unwrap_or_else(|| Ok(ExistingMutationEvidence::default()))
        } else {
            Ok(ExistingMutationEvidence::default())
        };
        #[cfg(not(unix))]
        let evidence = Ok(ExistingMutationEvidence::default());
        self.target_with_evidence_result(stored, evidence)
    }

    /// Preloaded evidence only; constructing or judging a target performs no store lookup.
    pub(crate) fn target_with_evidence(
        &self,
        stored: &HostedExecution,
        evidence: ExistingMutationEvidence,
    ) -> Option<HerdrTarget> {
        self.target_with_evidence_result(stored, Ok(evidence))
    }

    pub(crate) fn target_with_evidence_result(
        &self,
        stored: &HostedExecution,
        evidence: Result<ExistingMutationEvidence, super::herdr::policy::Unverified>,
    ) -> Option<HerdrTarget> {
        let target = self.endpoints.iter().find_map(|registered| {
            let transport: Arc<dyn HerdrTransport> = Arc::new(registered.transport.clone());
            HerdrTarget::new_with_evidence_result(
                registered.endpoint.clone(),
                transport,
                stored,
                evidence.clone(),
            )
        });
        #[cfg(test)]
        let target = match &self.reads {
            Some((restore, os)) => target.map(|target| target.with_reads(*restore, os.clone())),
            None => target,
        };
        target
    }

    /// A read-only view of a stored execution located on a registered endpoint; unlike a target
    /// it needs no launch evidence.
    pub(crate) fn view(&self, stored: &HostedExecution) -> Option<HerdrPaneView> {
        let view = self.endpoints.iter().find_map(|registered| {
            let transport: Arc<dyn HerdrTransport> = Arc::new(registered.transport.clone());
            HerdrPaneView::new(&registered.endpoint, transport, stored)
        });
        #[cfg(test)]
        let view = match &self.reads {
            Some((_, os)) => view.map(|view| view.with_os(os.clone())),
            None => view,
        };
        view
    }

    #[cfg(unix)]
    pub(crate) fn retired_probe(
        &self,
        stored: &HostedExecution,
    ) -> Option<super::herdr_gate::legacy_return::RetiredProbe> {
        use super::herdr_gate::legacy_return::RetiredProbe;
        let probe = self.endpoints.iter().find_map(|registered| {
            let transport: Arc<dyn HerdrTransport> = Arc::new(registered.transport.clone());
            RetiredProbe::new(registered.endpoint.clone(), transport, stored)
        });
        #[cfg(test)]
        let probe = match &self.reads {
            Some((_, os)) => probe.map(|probe| probe.with_os(os.clone())),
            None => probe,
        };
        probe
    }
}

impl Registered {
    fn new(endpoint: HerdrEndpoint, transport: Shared) -> Self {
        let host = ReadOnlyHerdrHost(HerdrHost::new(endpoint.clone(), transport.clone()));
        Self {
            endpoint,
            transport,
            host,
        }
    }
}

static EMPTY: HerdrRegistry = HerdrRegistry::EMPTY;
#[cfg(not(test))]
static REGISTRY: OnceLock<HerdrRegistry> = OnceLock::new();

/// The registry of the boot section, fixed on first use after boot installed it.
#[cfg(not(test))]
pub(crate) fn registry() -> &'static HerdrRegistry {
    if let Some(registry) = REGISTRY.get() {
        return registry;
    }
    crate::config::session_hosts::with_boot(|boot| match boot {
        Some(boot) => REGISTRY.get_or_init(|| HerdrRegistry::build(boot)),
        None => &EMPTY,
    })
}

/// Tests see only a registry they installed on their own thread.
#[cfg(test)]
pub(crate) fn registry() -> &'static HerdrRegistry {
    FORCED.with(|forced| forced.get()).unwrap_or(&EMPTY)
}

#[cfg(test)]
thread_local! {
    static FORCED: std::cell::Cell<Option<&'static HerdrRegistry>> = const { std::cell::Cell::new(None) };
}

/// Installs `registry` on this thread until dropped.
#[cfg(test)]
pub(crate) struct ForcedRegistry(Option<&'static HerdrRegistry>);

#[cfg(test)]
pub(crate) fn force_for_test(registry: HerdrRegistry) -> ForcedRegistry {
    let leaked: &'static HerdrRegistry = Box::leak(Box::new(registry));
    ForcedRegistry(FORCED.with(|forced| forced.replace(Some(leaked))))
}

#[cfg(test)]
impl Drop for ForcedRegistry {
    fn drop(&mut self) {
        FORCED.with(|forced| forced.set(self.0.take()));
    }
}

#[cfg(all(test, unix))]
#[path = "herdr_registry_census_tests.rs"]
mod census_tests;
