use super::super::lifecycle::{Lifetime, Runtime, Ticket};
use super::*;

/// Installs `fence` for `shared` until dropped; production installs nothing in this unit.
pub(in crate::services::discord) struct Installation(Weak<dyn Fence>);

pub(in crate::services::discord) fn install(
    shared: &Arc<SharedData>,
    fence: Weak<dyn Fence>,
) -> Installation {
    let mut installed = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
    installed.push((Arc::downgrade(shared), fence.clone()));
    INSTALLED_COUNT.fetch_add(1, Ordering::AcqRel);
    Installation(fence)
}

impl Drop for Installation {
    fn drop(&mut self) {
        let mut installed = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
        let before = installed.len();
        installed.retain(|(_, fence)| !Weak::ptr_eq(fence, &self.0));
        INSTALLED_COUNT.fetch_sub(before - installed.len(), Ordering::AcqRel);
    }
}

/// A real B2a runtime installed behind the hooks, with one registration per armed channel.
pub(in crate::services::discord) struct Probe {
    runtime: Arc<Runtime>,
    _lifetime: Lifetime,
    _installed: Installation,
}

impl Probe {
    pub(in crate::services::discord) fn install(shared: &Arc<SharedData>) -> Self {
        let runtime = Arc::new(Runtime::default());
        let lifetime = runtime.own();
        let fence: Weak<dyn Fence> = Arc::downgrade(&runtime) as Weak<dyn Fence>;
        let installed = install(shared, fence);
        Self {
            runtime,
            _lifetime: lifetime,
            _installed: installed,
        }
    }

    pub(in crate::services::discord) fn arm(&self, channel: u64) -> Ticket {
        self.runtime.register(channel).expect("open runtime")
    }

    /// Whether an approval taken under `ticket` could still reach its first poll.
    pub(in crate::services::discord) fn current(ticket: &Ticket) -> bool {
        ticket.incarnation().is_some()
    }

    /// Whether the runtime still admits a new registration; a suspended one admits none.
    pub(in crate::services::discord) fn open(&self, channel: u64) -> bool {
        self.runtime.register(channel).is_some()
    }

    /// Arms `channel` with an approval resting on `gate` instead of a Home.
    pub(in crate::services::discord) fn arm_gateway(
        &self,
        channel: u64,
        gate: Arc<OwnershipGate>,
    ) -> Ticket {
        let ticket = self.arm(channel);
        let identity = super::super::admission::Identity {
            provider: crate::services::tui_o::shadow::ShadowProvider::Claude,
            channel,
            session: format!("b2b1-gateway-{channel}"),
            source: crate::services::tui_o::shadow::SourceId {
                session_id: String::new(),
                path: std::path::PathBuf::from("/dev/null"),
                dev: 0,
                ino: channel,
            },
            binding_seq: 1,
            bot_id: 42,
        };
        let incarnation = ticket.incarnation().unwrap();
        assert!(incarnation.seat_gateway_for_tests(&ticket, identity, gate));
        self.arm(channel)
    }

    pub(in crate::services::discord) fn rests_on(
        ticket: &Ticket,
        gate: &Arc<OwnershipGate>,
    ) -> bool {
        ticket
            .incarnation()
            .is_some_and(|incarnation| incarnation.rests_on(gate))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn b2b1_suspend_and_resume_reach_only_the_runtime_installed_for_that_shared() {
    let (a, b) = (
        crate::services::discord::make_shared_data_for_tests(),
        crate::services::discord::make_shared_data_for_tests(),
    );
    let (probe_a, probe_b) = (Probe::install(&a), Probe::install(&b));
    let (channel_a, channel_b) = (6_325_901, 6_325_902);
    let (ticket_a, ticket_b) = (probe_a.arm(channel_a), probe_b.arm(channel_b));
    suspend(&a);
    assert!(!Probe::current(&ticket_a));
    assert!(!probe_a.open(channel_a));
    assert!(
        Probe::current(&ticket_b),
        "another provider runtime stays open"
    );
    resume_fresh(&a);
    assert!(!Probe::current(&ticket_a), "no earlier approval comes back");
    assert!(probe_a.open(channel_a));
}
