use super::super::lifecycle::{Lifetime, Runtime, Ticket};
use super::*;
// The one installer, under the path existing hook tests already name.
pub(in crate::services::discord) use super::install;

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

/// Polls until `done` holds, within a few seconds.
async fn until(done: impl Fn() -> bool) {
    for _ in 0..800 {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("never got there");
}

// A relay the cluster loop drops, by an update, a removal or its final drain, withdraws its
// channel in its provider's runtime only.
#[tokio::test(flavor = "current_thread")]
async fn b2b2_a_removed_cluster_relay_withdraws_only_its_provider_runtime() {
    use crate::services::cluster::session_matcher::{self, MatchedChannel};
    use crate::services::cluster::watcher_supervisor as relays;
    use crate::services::cluster::{relay_producer_registry, session_registry, stream_relay};
    use crate::services::provider::ProviderKind;
    let claude = crate::services::discord::make_shared_data_for_tests();
    let mut codex = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut codex).expect("unshared").provider = ProviderKind::Codex;
    let (probe, other) = (Probe::install(&claude), Probe::install(&codex));
    let channel = 6_325_931_u64;
    let session = ProviderKind::Claude.build_tmux_session_name("b2b2-relay");
    let matched = |agent: &str| MatchedChannel {
        channel_id: channel.to_string(),
        agent_id: agent.to_string(),
        provider: ProviderKind::Claude,
        expected_session_name: session.clone(),
        expected_rollout_path: session_matcher::expected_rollout_path_for(&session),
    };
    let registry = Arc::new(session_registry::SessionRegistry::new());
    let producers = Arc::new(relay_producer_registry::RelayProducerRegistry::new());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let run = tokio::spawn(
        relays::run_watcher_supervisor_loop_with_registry_and_producers(
            relays::SupervisorConfig::for_test(),
            Arc::new(stream_relay::DiscardSink),
            shutdown.clone(),
            registry.clone(),
            producers.clone(),
        ),
    );
    let relayed = || producers.get_producer(&session).is_some();
    registry.upsert(matched("a"), Some("mac-mini"));
    until(relayed).await;
    let original = producers.get_producer(&session).unwrap();
    let foreign = other.arm(channel);
    let ticket = probe.arm(channel);
    registry.upsert(matched("b"), Some("mac-mini"));
    // Producer shutdown follows removal and withdrawal, independently of presence state.
    until(|| !original.is_alive()).await;
    assert!(!Probe::current(&ticket), "the updated relay withdraws");
    let replacement = producers.get_producer(&session).unwrap();
    assert!(replacement.is_alive(), "the replacement relay is running");
    let ticket = probe.arm(channel);
    registry.remove(&session);
    until(|| !replacement.is_alive()).await;
    assert!(producers.get_producer(&session).is_none());
    assert!(!Probe::current(&ticket), "the removed relay withdraws");
    registry.upsert(matched("c"), Some("mac-mini"));
    until(relayed).await;
    let ticket = probe.arm(channel);
    shutdown.store(true, Ordering::Release);
    run.await.unwrap();
    assert!(!Probe::current(&ticket), "the final drain withdraws");
    assert!(
        Probe::current(&foreign),
        "another provider's runtime keeps the channel"
    );
}
