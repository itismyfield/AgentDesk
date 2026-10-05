//! This node's Herdr endpoints from the boot `session_hosts` section: one socket transport per
//! local endpoint, the read-only host `host_for(Herdr)` returns, the launch host and gated input
//! targets. Building dials nothing; an empty section or another node's endpoint builds nothing.
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
use super::herdr_gate::HerdrTarget;
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
                .and_then(|endpoint| endpoint.with_herdr_home(&configured.herdr_home))
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
        let target = self.endpoints.iter().find_map(|registered| {
            let transport: Arc<dyn HerdrTransport> = Arc::new(registered.transport.clone());
            HerdrTarget::new(registered.endpoint.clone(), transport, stored)
        });
        #[cfg(test)]
        let target = match &self.reads {
            Some((restore, os)) => target.map(|target| target.with_reads(*restore, os.clone())),
            None => target,
        };
        target
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
