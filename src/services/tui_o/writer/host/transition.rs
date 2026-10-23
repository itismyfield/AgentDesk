//! The ports each actor run answers on, registered in readiness for that run only.

use std::time::SystemTime;

use super::*;
use crate::services::tui_o::writer::transition_port::{self, TransitionPort};

/// The ports of one actor run.
pub(super) struct Ports {
    submission: Option<actor::codex_submission::Port>,
    transition: TransitionPort,
}

impl<I: HostIo> Hosted<'_, I> {
    /// Starts the actor inside the scopes that hand this run's receivers to it.
    pub(super) fn spawn_ports<T>(
        &self,
        suspension: super::super::suspension::Suspension,
        spawn: impl FnOnce() -> T,
    ) -> (Ports, T) {
        let generation = self.claim.generation;
        let wall: transition_port::Wall = Arc::new(SystemTime::now);
        let stopping = self.stopping.clone();
        let (transition, receiver) =
            TransitionPort::new(generation, Arc::clone(self.gate), wall, stopping);
        let transition = transition.with_suspension(suspension.clone());
        let (submission, spawned) = if self.provider == ShadowProvider::Codex {
            let (submission, commands) = actor::codex_submission::Port::new(generation);
            let submission = submission.with_suspension(suspension);
            let spawn = || actor::codex_submission::spawning(commands, spawn);
            (Some(submission), transition_port::spawning(receiver, spawn))
        } else {
            (None, transition_port::spawning(receiver, spawn))
        };
        let ports = Ports {
            submission,
            transition,
        };
        (ports, spawned)
    }
}

impl Readiness {
    /// Registers a run's ports; a transition port no actor took is left out. Returns it to close.
    pub(super) fn register_ports(&self, channel: u64, ports: Ports) -> Option<TransitionPort> {
        let mut live = locked(&self.live);
        let live = live.get_mut(&channel)?;
        live.submission = ports.submission;
        live.transition = Some(ports.transition).filter(TransitionPort::is_open);
        live.transition.clone()
    }

    /// Drops the port of a run whose actor ended, unless a later run's took its place.
    pub(super) fn close_transition(&self, channel: u64, ended: Option<TransitionPort>) {
        let Some(ended) = ended else {
            return;
        };
        if let Some(live) = locked(&self.live).get_mut(&channel)
            && live
                .transition
                .as_ref()
                .is_some_and(|port| port.same_run(&ended))
        {
            live.transition = None;
        }
    }

    /// The running actor's port, while it takes work as `with_submission` checks it.
    pub(crate) fn transition(&self, channel: u64) -> Option<TransitionPort> {
        self.with_transition(channel, |port| Ok(port.clone())).ok()
    }

    /// Consumes `reply` only while the run `sent` named is still the channel's current one.
    pub(crate) fn with_transition_reply<T>(
        &self,
        channel: u64,
        sent: &TransitionPort,
        reply: transition_port::Reply,
        consume: impl FnOnce(&transition_port::Reply) -> T,
    ) -> Result<T, &'static str> {
        self.with_transition(channel, |current| match current.same_run(sent) {
            true => Ok(consume(&reply)),
            false => Err("stale_actor_reply"),
        })
    }

    fn with_transition<T>(
        &self,
        channel: u64,
        consume: impl FnOnce(&TransitionPort) -> Result<T, &'static str>,
    ) -> Result<T, &'static str> {
        let live = locked(&self.live);
        let live = live.get(&channel).ok_or("actor_unavailable")?;
        if *live.stopping.borrow()
            || live.resumed.has_changed().is_err()
            || !self.is_ready(channel)
            || !*live.resumed.borrow()
            || !owned_now(&live.gate)
        {
            return Err("actor_unavailable");
        }
        let port = live.transition.as_ref().ok_or("actor_unavailable")?;
        if !port.sources_unsuspended() {
            return Err("suspended_sources");
        }
        consume(port)
    }
}

#[cfg(test)]
mod test_support {
    use super::*;

    /// A channel [`Readiness::host_port_for_test`] registered; it leaves readiness on drop.
    pub(crate) struct HostedForTest(Arc<Readiness>, u64);

    impl Drop for HostedForTest {
        fn drop(&mut self) {
            locked(&self.0.live).remove(&self.1);
            self.0.set(self.1, false);
        }
    }

    impl Readiness {
        /// Registers `port` for `channel` as a running run under `gate` would, readable while
        /// `resumed` holds and `stopping` does not.
        pub(crate) fn host_port_for_test(
            self: &Arc<Self>,
            channel: u64,
            gate: &Arc<OwnershipGate>,
            port: TransitionPort,
            (resumed, stopping): (watch::Receiver<bool>, watch::Receiver<bool>),
        ) -> HostedForTest {
            let owing = Owing::default();
            let owed = OwedView {
                published: owing.published.subscribe(),
                demand: owing.demand.clone(),
            };
            let watched = (resumed, watch::channel(None).1, owed, stopping);
            self.track(channel, Arc::clone(gate), watched);
            self.set(channel, true);
            locked(&self.live).get_mut(&channel).unwrap().transition = Some(port);
            HostedForTest(Arc::clone(self), channel)
        }
    }
}
