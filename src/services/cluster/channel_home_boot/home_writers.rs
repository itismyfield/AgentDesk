//! A mounted home's writer as its watch judges it: whose gate hosts the channel here, the start or
//! replacement that makes it the home's, and the confirmed end before the home leaves.

use std::sync::Arc;

#[cfg(test)]
use super::writer_mutant;
use super::{ChannelWriters, HomeGate, Seen, StartWriter, gate_address as gate_of};
use crate::services::cluster::home_supervisor::{HomeBundle, Phase, Refused, Settled, StopReason};
use crate::services::tui_o::writer::host::ManagedWriterHandle;

/// Shown while a claim the supervisor cannot account for keeps the home from renewing.
pub(crate) const WRITER_AUTHORITY: &str = "writer_authority";

/// How far the runtime's boot got with its writers: until it handed them over nothing may judge
/// a channel free of them, as its first writer may still be claiming it.
pub(super) enum BootStage<'a> {
    Incomplete,
    /// The boot finished and starts no writer later.
    Unarmed,
    Armed(&'a StartWriter),
}

/// Whose gate hosts a mounted home's channel in this process, from its claim and supervised slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Authority {
    /// No claim, so no host: each host claims the channel before it is spawned.
    Absent,
    /// The running slot's host claims the channel under the home's gate.
    Current,
    /// The running slot's host claims the channel under another gate.
    Mismatch,
    /// A claim the supervisor does not account for yet, or a slot starting, stopping or blocked.
    Pending,
    /// This round started or replaced the channel's writer under the home's gate.
    Started,
    /// No writer under the home's gate could start; not tried again while it stays mounted.
    Failed,
}

#[cfg(test)]
fn mutant(name: &str) -> bool {
    writer_mutant(name)
}

/// One channel's managed writer as the supervisor runs it, and whether a delegated runtime runs it.
pub(crate) struct WriterBundle {
    pub(super) handle: ManagedWriterHandle,
    delegated: bool,
}

impl WriterBundle {
    pub(super) fn new(handle: ManagedWriterHandle, delegated: bool) -> Self {
        #[cfg(test)]
        events::record(
            handle.channel(),
            events::Event::Started(handle.generation(), handle.authority()),
        );
        Self { handle, delegated }
    }
}

impl HomeBundle for WriterBundle {
    /// A replacement, or a delegated home's leave, hands the late reservation to the next host as
    /// it stands; any other end settles it, as the host's own exit does.
    fn stop_and_join(self, reason: StopReason) -> impl Future<Output = Settled> + Send {
        let keep = match reason {
            StopReason::Replaced => true,
            StopReason::RowGone => self.delegated,
            StopReason::Sigterm | StopReason::BackendExited | StopReason::CommittedRestart => false,
        };
        #[cfg(test)]
        let keep = match () {
            () if writer_mutant("keep_on_every_stop") => true,
            () if writer_mutant("replaced_stop_settles_reservation") => {
                keep && reason != StopReason::Replaced
            }
            () => keep,
        };
        if keep {
            self.handle.keep_reservation();
        }
        let stopped = self.handle.stop_and_join();
        async move {
            match stopped.await {
                Ok(_stopped) => {
                    #[cfg(test)]
                    events::record(_stopped.channel, events::Event::Joined(_stopped.generation));
                    Settled::Joined
                }
                Err(detail) => Settled::Stuck(detail),
            }
        }
    }
}

impl Seen {
    pub(super) fn note(&self, blocker: &'static str) {
        if let Some(home) = &self.mounted {
            home.note_drain(Some(blocker));
        }
    }

    pub(super) fn delegated(&self) -> bool {
        self.writers
            .as_ref()
            .is_some_and(|writers| writers.delegated)
    }

    /// Whether the mounted home may go: its writer's end under its gate is confirmed, if any.
    pub(super) async fn left(&self, reason: StopReason) -> bool {
        match (&self.writers, &self.mounted) {
            (Some(writers), Some(home)) => writers.stop_under(self.id, home, reason).await,
            _ => true,
        }
    }

    pub(super) async fn ensure_home(&mut self, home: &HomeGate, missing_ok: bool) -> Authority {
        #[cfg(test)]
        if mutant("home_renews_before_authority") {
            return Authority::Current;
        }
        let Self {
            id, writers, tried, ..
        } = self;
        match writers {
            Some(writers) => writers.ensure_home(*id, home, missing_ok, tried).await,
            None => Authority::Absent,
        }
    }

    /// Whether a released home's writer may take the channel: one is started under its gate if
    /// none runs; without supervised writers the restored flag alone decides.
    pub(super) async fn adoptable(&mut self, home: &HomeGate) -> Result<(), &'static str> {
        if self.writers.is_none() {
            return Ok(());
        }
        let judged = match self.ensure_home(home, true).await {
            Authority::Pending => Err(WRITER_AUTHORITY),
            _ => Ok(()),
        };
        #[cfg(test)]
        events::record(self.id, events::Event::Adoptable(judged));
        judged
    }
}

impl ChannelWriters {
    pub(super) fn stage(&self) -> BootStage<'_> {
        match self.start.get() {
            None => BootStage::Incomplete,
            Some(None) => BootStage::Unarmed,
            Some(Some(start)) => BootStage::Armed(start),
        }
    }

    /// The channel's authority as read now; reading the claim first, a host the boot spawned but
    /// had not handed over yet is `Pending`, never `Absent`.
    fn authority(&self, channel: u64, gate: usize) -> Authority {
        let Some(claim) = self.readiness.hosted_by(channel) else {
            return Authority::Absent;
        };
        let running = self.supervisor.running(channel, |bundle| {
            (bundle.handle.generation(), bundle.handle.authority())
        });
        match running {
            #[cfg(test)]
            Some(_) if mutant("home_writer_keeps_authority") => Authority::Current,
            Some((generation, authority)) if generation == claim && authority == gate => {
                Authority::Current
            }
            Some((generation, _)) if generation == claim => Authority::Mismatch,
            #[cfg(test)]
            None if self.supervisor.phase(channel).is_none() && mutant("unarmed_means_empty") => {
                Authority::Absent
            }
            _ => Authority::Pending,
        }
    }

    /// Makes the channel's writer the home's: one under another gate is joined before the home's
    /// starts; with `missing_ok` a missing one is started once per mount, remembered in `tried`.
    pub(crate) async fn ensure_home(
        &self,
        channel: u64,
        home: &HomeGate,
        missing_ok: bool,
        tried: &mut Option<usize>,
    ) -> Authority {
        let gate = gate_of(home);
        let judged = self.authority(channel, gate);
        #[cfg(test)]
        events::record(channel, events::Event::Authority(judged));
        let BootStage::Armed(start) = self.stage() else {
            return match judged {
                Authority::Mismatch => Authority::Pending,
                judged => judged,
            };
        };
        let replace = match judged {
            Authority::Absent => missing_ok && *tried != Some(gate),
            Authority::Mismatch => true,
            #[cfg(test)]
            Authority::Current => mutant("boot_watch_restarts_writer"),
            _ => false,
        };
        if !replace {
            return judged;
        }
        let (start, delegated) = (Arc::clone(start), self.delegated);
        let build = move |_| async move {
            let handle = start(channel).ok_or_else(|| "no writer started".to_owned())?;
            if handle.authority() != gate {
                let _ = handle.stop_and_join().await;
                return Err("the writer started under another gate".to_owned());
            }
            Ok(WriterBundle::new(handle, delegated))
        };
        // A missing writer is started once per mount, so a host that ends is not restarted round
        // after round; a replacement is not counted, as each one is owed.
        let started = self.supervisor.start(channel, build).await;
        if judged == Authority::Absent && !matches!(started, Err(Refused::Busy(_))) {
            *tried = Some(gate);
        }
        match started {
            Ok(_) => Authority::Started,
            Err(Refused::Busy(_) | Refused::Blocked(..) | Refused::Stopped(_)) => {
                Authority::Pending
            }
            Err(Refused::Failed(_) | Refused::Lost) => Authority::Failed,
        }
    }

    /// Whether the home may leave: no host claims the channel under its gate, or the one that did
    /// was stopped and its end confirmed; never before the boot handed its writers over.
    pub(crate) async fn stop_under(
        &self,
        channel: u64,
        home: &HomeGate,
        reason: StopReason,
    ) -> bool {
        let left = self.left_under(channel, gate_of(home), reason).await;
        #[cfg(test)]
        events::record(channel, events::Event::Leave(left));
        left
    }

    async fn left_under(&self, channel: u64, gate: usize, reason: StopReason) -> bool {
        #[cfg(test)]
        let barrier = !mutant("boot_absent_means_gone");
        #[cfg(not(test))]
        let barrier = true;
        if barrier && matches!(self.stage(), BootStage::Incomplete) {
            return false;
        }
        let claim = self.readiness.hosted_by(channel);
        let running = self.supervisor.running(channel, |bundle| {
            (bundle.handle.generation(), bundle.handle.authority())
        });
        match (self.supervisor.phase(channel), running) {
            (None, _) => claim.is_none(),
            (Some(Phase::Running(_)), Some((generation, authority))) if authority != gate => {
                claim.is_none_or(|claim| claim == generation)
            }
            (Some(Phase::Running(slot)), Some(_)) => {
                #[cfg(test)]
                if self.delegated && mutant("delegated_leave_keeps_writer") {
                    return true;
                }
                let phase = self.supervisor.stop(channel, Some(slot), reason).await;
                phase.is_none() && self.readiness.hosted_by(channel).is_none()
            }
            #[cfg(test)]
            _ if mutant("nonrunning_means_absent") => true,
            _ => false,
        }
    }
}

/// What the watches and writers of this process did, in order, for tests to assert on; kept
/// process-wide so a host running on another thread records into the same log.
#[cfg(test)]
pub(crate) mod events {
    use std::sync::{Arc, Mutex};

    use tokio::sync::Semaphore;

    use super::Authority;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum Event {
        /// A gate was registered for the channel, by its gate's address.
        Mount(usize),
        Unmount(usize),
        /// A supervised writer of this claim generation runs under the gate at this address.
        Started(u64, usize),
        /// The writer of this claim generation was stopped and its end confirmed.
        Joined(u64),
        /// A lease began renewing this epoch for the home whose gate is at this address.
        Renew(i64, usize),
        Authority(Authority),
        /// Whether a leave found the home free to go.
        Leave(bool),
        /// A released home's adoption check and what it found.
        Adoptable(Result<(), &'static str>),
        /// The watch paused before acting on a row that no longer names this node.
        Parked,
        /// A step a test's own hook marks.
        Mark(&'static str),
    }

    static LOG: Mutex<Vec<(u64, Event)>> = Mutex::new(Vec::new());

    pub(crate) fn record(channel: u64, event: Event) {
        LOG.lock().unwrap().push((channel, event));
    }

    /// The channel's events so far, oldest first.
    pub(crate) fn of(channel: u64) -> Vec<Event> {
        let log = LOG.lock().unwrap();
        let ours = log.iter().filter(|(c, _)| *c == channel);
        ours.map(|(_, event)| event.clone()).collect()
    }

    pub(crate) fn clear(channel: u64) {
        LOG.lock().unwrap().retain(|(c, _)| *c != channel);
    }

    static PARKS: Mutex<Vec<(u64, Arc<Semaphore>)>> = Mutex::new(Vec::new());

    /// The channel's watch pauses once, before it acts on a row that does not name its node,
    /// until the returned park is released; it then reads the row again.
    pub(crate) fn park(channel: u64) -> Park {
        let held = Arc::new(Semaphore::new(0));
        PARKS.lock().unwrap().push((channel, Arc::clone(&held)));
        Park(held)
    }

    pub(crate) struct Park(Arc<Semaphore>);

    impl Park {
        pub(crate) fn release(self) {
            self.0.add_permits(1);
        }
    }

    /// The park armed for the channel, taken so it pauses the watch once.
    pub(crate) fn parked(channel: u64) -> Option<Arc<Semaphore>> {
        let mut parks = PARKS.lock().unwrap();
        let at = parks.iter().position(|(c, _)| *c == channel)?;
        Some(parks.remove(at).1)
    }

    /// Where the first event `matches` picks sits in the channel's log.
    pub(crate) fn position(channel: u64, matches: impl Fn(&Event) -> bool) -> Option<usize> {
        of(channel).iter().position(matches)
    }
}

#[cfg(test)]
#[path = "home_writers_tests.rs"]
mod tests;
