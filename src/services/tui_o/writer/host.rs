//! One actor per adopted channel, creating its first store unless Legacy took it.
//! Ready while resumed and owned by the gateway lease.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::activation::ActivationFacts;
use super::actor::{self, Demand, Owing, Undelivered};
use super::adoption::LegacyView;
use super::binding::BindingEvents;
use super::deliver::{ChannelWriter, StopCause};
use super::resume::{self, Backoff};
use super::{AlarmSink, DeliveryLease, DiscordPort, ReaderConfig, WriterAlarm, WriterConfig};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::channel_policy::{Adoption, Candidate};
use crate::services::tui_o::cutover;
use crate::services::tui_o::ownership::{GatewayOwnership, OwnershipGate};
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::store::ChannelStore;
use crate::services::tui_o::store::ledger::Unsent;

mod activation_io;
mod first_init;
mod fresh_input;
mod recover;
mod transition;
pub use activation_io::{Custody, FencedFacts};
pub(crate) use fresh_input::initial_fresh_refusal;
use recover::{Recovered, recover, recover_boot};
/// What the gateway runtime supplies; asked only for channels O owns.
pub trait HostIo: Send + Sync + 'static {
    type Port: DiscordPort;
    type Lease: DeliveryLease + 'static;
    type Alarms: AlarmSink + Clone + 'static;
    type Bindings: BindingEvents;
    /// Resolves once the gateway's HTTP client and the bot's own id are known.
    fn port(&self) -> impl Future<Output = Arc<Self::Port>> + Send;
    fn port_for(
        &self,
        _channel: u64,
    ) -> impl Future<Output = Result<Arc<Self::Port>, String>> + Send {
        Box::pin(async move { Ok(self.port().await) })
    }
    fn execution_permit(
        &self,
        _channel: u64,
    ) -> Option<Result<Arc<crate::services::tui_o::ownership::execution::OwnerPermit>, String>>
    {
        None
    }
    fn lease(&self) -> Self::Lease;
    /// A channel's writer stopped for good; the shared lease turns its verified episodes Unknown.
    fn writer_unavailable(&self, channel: u64) {
        self.lease().writer_unavailable(channel);
    }
    fn alarms(&self) -> Self::Alarms;
    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<Self::Bindings>;
    /// Enables new late-adoption attempts; a reserved channel stays Legacy while off.
    fn late_adopt_enabled(&self) -> bool {
        false
    }
    /// Asked only for a channel with no store yet, before its first `init`, while the gate is Owned.
    fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send;
    /// Fresh intake facts held through deferred activation; hosts without a fence refuse it.
    fn intake_fence(
        &self,
        _channel: u64,
        _provider: ShadowProvider,
    ) -> impl Future<Output = Result<FencedFacts, String>> + Send {
        std::future::ready(Err("this host has no intake fence".into()))
    }
    /// Legacy custody under its adoption lock before first `init`; never judges TUI output.
    fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<Custody, String>;
    /// Legacy's relay state, asked only for a Claude channel whose sources already hold output.
    fn legacy(&self) -> Arc<dyn LegacyView>;
    /// Whether Legacy's mailbox for the channel holds a turn, an intervention or a pending dispatch.
    fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send;
    /// Whether Legacy's watcher is emitting the channel's terminal delivery or its chrome now.
    fn relaying(&self, channel: u64) -> bool;
    /// Called once a first activation committed the channel, before it is ready for intake.
    fn adopted(&self, _channel: u64, _provider: ShadowProvider) {}
    /// The channel's re-post turn for one actor start; none leaves the writer as it was.
    fn repost(&self, _channel: u64, _port: &Arc<Self::Port>) -> Option<RepostTurn<Self>> {
        None
    }
}
/// A host's re-post turn for one channel writer.
pub(crate) type RepostTurn<I> = Box<
    dyn crate::services::tui_o::repost::runner::RepostHook<
            ChannelWriter<<I as HostIo>::Port, <I as HostIo>::Lease, <I as HostIo>::Alarms>,
        >,
>;

/// Channels with a hosted actor, by the claim that hosts each, and those ready to take work.
#[derive(Default)]
pub struct Readiness {
    hosted: Mutex<BTreeMap<u64, u64>>,
    ready: Mutex<BTreeSet<u64>>,
    live: Mutex<BTreeMap<u64, Live>>,
    claims: AtomicU64,
}

/// Keeps a channel reserved until every child settled; an abnormal exit closes readiness.
pub struct ReadinessClaim {
    readiness: Arc<Readiness>,
    channel: u64,
    generation: u64,
    settled: Arc<AtomicBool>,
}

impl ReadinessClaim {
    /// Clears readiness and frees the channel only after its host and actor ended.
    fn release(self) {
        self.clear(true);
    }

    fn clear(&self, release: bool) {
        let readiness = &self.readiness;
        let mut hosted = locked(&readiness.hosted);
        if hosted.get(&self.channel) != Some(&self.generation) {
            return;
        }
        locked(&readiness.ready).remove(&self.channel);
        locked(&readiness.live).remove(&self.channel);
        if release {
            hosted.remove(&self.channel);
        }
    }
}

impl Drop for ReadinessClaim {
    fn drop(&mut self) {
        self.clear(false);
    }
}

/// What a hosted channel's readiness is derived from, kept so intake can read it directly.
struct Live {
    gate: Arc<OwnershipGate>,
    resumed: watch::Receiver<bool>,
    unsettled: watch::Receiver<Option<usize>>,
    owed: OwedView,
    stopping: watch::Receiver<bool>,
    submission: Option<actor::codex_submission::Port>,
    transition: Option<super::transition_port::TransitionPort>,
}

/// The actor's published owed view and the demand that requests a store read.
#[derive(Clone)]
pub struct OwedView {
    pub published: watch::Receiver<Option<Undelivered>>,
    pub demand: Demand,
}

fn locked<T>(set: &Mutex<T>) -> MutexGuard<'_, T> {
    set.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Readiness {
    /// The generation whose claim hosts `channel`, read now. A claim is not a live host: one whose
    /// actor ended abnormally keeps it. No claim does mean no host, as each claims before it spawns.
    pub fn hosted_by(&self, channel: u64) -> Option<u64> {
        locked(&self.hosted).get(&channel).copied()
    }
    pub fn is_ready(&self, channel: u64) -> bool {
        locked(&self.ready).contains(&channel)
    }

    fn set(&self, channel: u64, ready: bool) {
        let mut set = locked(&self.ready);
        if ready {
            set.insert(channel);
        } else {
            set.remove(&channel);
        }
    }

    /// Only the first claim of a channel hosts it until released, so a channel never has two actors.
    fn claim(self: &Arc<Self>, channel: u64) -> Option<ReadinessClaim> {
        let mut hosted = locked(&self.hosted);
        if hosted.contains_key(&channel) {
            return None;
        }
        let generation = self.claims.fetch_add(1, Ordering::SeqCst) + 1;
        hosted.insert(channel, generation);
        let readiness = Arc::clone(self);
        Some(ReadinessClaim {
            readiness,
            channel,
            generation,
            settled: Arc::new(AtomicBool::new(true)),
        })
    }

    pub fn is_hosted(&self, channel: u64) -> bool {
        locked(&self.hosted).contains_key(&channel)
    }

    fn track(
        &self,
        channel: u64,
        gate: Arc<OwnershipGate>,
        (resumed, unsettled, owed, stopping): Watched,
    ) {
        let live = Live {
            gate,
            resumed,
            unsettled,
            owed,
            stopping,
            submission: None,
            transition: None,
        };
        locked(&self.live).insert(channel, live);
    }

    /// Ready, with the gate Owned and actor running resumed as read now.
    pub fn accepts(&self, channel: u64) -> bool {
        let live = locked(&self.live);
        let Some(live) = live.get(&channel).filter(|_| self.is_ready(channel)) else {
            return false;
        };
        let owned = owned_now(&live.gate);
        owned
            && !*live.stopping.borrow()
            && live.resumed.has_changed().is_ok()
            && *live.resumed.borrow()
    }

    /// Rotated-away sources the channel's running actor has not retired, as its last poll read
    /// them; `None` when no actor runs, its store could not be read or Herdr is not configured.
    pub fn rotation_unsettled(&self, channel: u64) -> Option<usize> {
        let live = locked(&self.live);
        let unsettled = &live.get(&channel)?.unsettled;
        unsettled.has_changed().ok()?;
        *unsettled.borrow()
    }

    /// Running actor's owed view, cloned under the map lock; `None` without an actor.
    pub fn undelivered(&self, channel: u64) -> Option<OwedView> {
        let live = locked(&self.live);
        Some(live.get(&channel)?.owed.clone())
    }

    pub(crate) fn submission(&self, channel: u64) -> Option<actor::codex_submission::Port> {
        locked(&self.live).get(&channel)?.submission.clone()
    }

    #[cfg(test)]
    pub(crate) fn drop_submission_for_tests(&self, channel: u64) {
        if let Some(live) = locked(&self.live).get_mut(&channel) {
            live.submission = None;
        }
    }

    pub(super) fn with_submission<T>(
        &self,
        channel: u64,
        consume: impl FnOnce(&actor::codex_submission::Port) -> Result<T, &'static str>,
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
        let port = live.submission.as_ref().ok_or("actor_unavailable")?;
        if !port.sources_unsuspended() {
            return Err("suspended_sources");
        }
        consume(port)
    }

    /// Hosts `owed` for `channel` as a running actor would publish it, with no actor behind it.
    #[cfg(test)]
    pub(crate) fn track_owed_for_test(&self, channel: u64, owed: OwedView) {
        let gate = Arc::new(OwnershipGate::default());
        self.track(
            channel,
            gate,
            (
                watch::channel(false).1,
                watch::channel(None).1,
                owed,
                watch::channel(false).1,
            ),
        );
    }
}

type Published = (
    watch::Receiver<GatewayOwnership>,
    watch::Receiver<bool>,
    watch::Receiver<bool>,
);

type Watched = (
    watch::Receiver<bool>,
    watch::Receiver<Option<usize>>,
    OwedView,
    watch::Receiver<bool>,
);

static PROCESS: LazyLock<Arc<Readiness>> = LazyLock::new(Arc::default);

pub(crate) fn process_readiness() -> Arc<Readiness> {
    Arc::clone(&PROCESS)
}

/// Whether this process's writer can take work for `channel`; false for any channel O does not own.
pub(crate) fn channel_accepts(channel: u64) -> bool {
    #[cfg(test)]
    if crate::services::claude_tui::source_verify::n2b_mutant("b1b-global") {
        return fresh_input::initial_fresh_refusal(channel).is_none();
    }
    PROCESS.accepts(channel)
}

/// [`Readiness::rotation_unsettled`] of this process's writer.
pub(crate) fn rotation_unsettled(channel: u64) -> Option<usize> {
    #[cfg(test)]
    if let Some(forced) = FORCED_UNSETTLED.with(std::cell::Cell::get) {
        return forced;
    }
    PROCESS.rotation_unsettled(channel)
}

#[cfg(test)]
thread_local! {
    static FORCED_UNSETTLED: std::cell::Cell<Option<Option<usize>>> = const { std::cell::Cell::new(None) };
}

/// Reports `unsettled` for every channel on this thread until dropped, as a running actor would.
#[cfg(test)]
pub(crate) fn force_unsettled_for_test(unsettled: Option<usize>) -> impl Drop {
    struct Restore(Option<Option<usize>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            FORCED_UNSETTLED.with(|cell| cell.set(self.0));
        }
    }
    Restore(FORCED_UNSETTLED.with(|cell| cell.replace(Some(unsettled))))
}

/// What hosting needs once a channel is owned; built only then, so an off or empty writer takes nothing.
pub struct HostParts<I> {
    pub io: Arc<I>,
    pub runtime_root: Option<PathBuf>,
    pub gate: Arc<OwnershipGate>,
    pub readiness: Arc<Readiness>,
}

/// Whether the gateway gate owns this writer now.
pub(super) fn owned_now(gate: &OwnershipGate) -> bool {
    gate.current().epoch().is_some()
}

/// Spawns one host per channel this bot may adopt. A missing PG gateway lease holds the
/// channels with an alarm; only the gateway gate can become Owned.
pub fn start<I: HostIo>(
    provider: ShadowProvider,
    pg_gateway: bool,
    prepare: impl FnOnce() -> HostParts<I>,
) -> Vec<JoinHandle<()>> {
    let owned = cutover::boot_ownership();
    detached(start_managed(provider, pg_gateway, owned, prepare))
}

fn detached(handles: Vec<ManagedWriterHandle>) -> Vec<JoinHandle<()>> {
    handles
        .into_iter()
        .map(ManagedWriterHandle::into_detached)
        .collect()
}

pub type OwnedChannels = Vec<(u64, Option<RuntimeHandoffKind>, Option<Candidate>)>;

/// One hosted channel's writer: its host task, the stop it obeys and the claim that hosts it.
pub struct ManagedWriterHandle {
    #[cfg(test)]
    channel: u64,
    #[cfg(test)]
    generation: u64,
    #[cfg(test)]
    settled: Arc<AtomicBool>,
    stop: StopOnDrop,
    host: JoinHandle<()>,
    #[cfg(test)]
    keep: Arc<AtomicBool>,
}

struct StopOnDrop {
    signal: watch::Sender<bool>,
    armed: bool,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.signal.send_replace(true);
        }
    }
}

/// A writer whose host, actor and admitted POST all ended, its claim released.
#[derive(Debug, PartialEq, Eq)]
pub struct WriterStopped {
    pub channel: u64,
    pub generation: u64,
}

impl ManagedWriterHandle {
    #[cfg(test)]
    pub fn channel(&self) -> u64 {
        self.channel
    }

    /// Whether the host task still runs and was not told to stop.
    #[cfg(test)]
    pub fn live(&self) -> bool {
        !self.host.is_finished() && !*self.stop.signal.borrow()
    }

    /// Leaves the late reservation unsettled when this host ends, for a successor that takes it
    /// over; set before the stop is signalled.
    #[cfg(test)]
    pub fn keep_reservation(&self) {
        self.keep.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Leaves shutdown to the existing gateway lifecycle; the host still owns its claim.
    pub fn into_detached(mut self) -> JoinHandle<()> {
        self.stop.armed = false;
        self.host
    }

    /// Requests stop before returning the wait future; its cancellation never cancels cleanup.
    #[cfg(test)]
    pub fn stop_and_join(
        self,
    ) -> impl Future<Output = Result<WriterStopped, String>> + Send + 'static {
        let Self {
            channel,
            generation,
            settled,
            stop,
            host,
            keep: _,
        } = self;
        stop.signal.send_replace(true);
        let settle = tokio::spawn(async move {
            let joined = host.await;
            drop(stop);
            joined.map_err(|error| format!("writer host ended abnormally: {error}"))?;
            if !settled.load(Ordering::SeqCst) {
                return Err("writer child ended without confirmed settlement".into());
            }
            Ok(WriterStopped {
                channel,
                generation,
            })
        });
        async move { settle.await.map_err(|error| error.to_string())? }
    }
}

/// [`start`] over the supplied owned channels.
pub fn start_managed<I: HostIo>(
    provider: ShadowProvider,
    pg_gateway: bool,
    owned: OwnedChannels,
    prepare: impl FnOnce() -> HostParts<I>,
) -> Vec<ManagedWriterHandle> {
    let kind = match provider {
        ShadowProvider::Claude => RuntimeHandoffKind::ClaudeTui,
        ShadowProvider::Codex => RuntimeHandoffKind::CodexTui,
    };
    let ours = |(channel, channel_kind, candidate): (u64, _, Option<Candidate>)| {
        Some((channel, candidate.filter(|_| channel_kind == Some(kind))?))
    };
    let channels: Vec<_> = owned.into_iter().filter_map(ours).collect();
    if channels.is_empty() {
        return Vec::new();
    }
    let HostParts {
        io,
        runtime_root,
        gate,
        readiness,
    } = prepare();
    let mut tasks = Vec::new();
    for (channel, candidate) in channels {
        let Some(claim) = readiness.claim(channel) else {
            continue;
        };
        let keep = Arc::new(AtomicBool::new(false));
        let reservation = candidate.late_cleanup_guard(channel, runtime_root.clone());
        let reservation = reservation.map(|guard| guard.kept_by(Arc::clone(&keep)));
        let execution = io.execution_permit(channel);
        let authorized = match &execution {
            Some(Ok(_)) => true,
            Some(Err(_)) => false,
            None => pg_gateway,
        };
        let root = match (authorized, &runtime_root) {
            (false, _) => Err("no PG gateway lease"),
            (true, None) => Err("runtime root unresolved"),
            (true, Some(root)) => Ok(root.clone()),
        };
        let root = match root {
            Ok(root) => root,
            Err(detail) => {
                hold(&io.alarms(), channel, detail);
                drop(reservation);
                claim.release();
                continue;
            }
        };
        let gate = match execution {
            Some(Ok(permit)) => permit.gate(),
            _ => Arc::clone(&gate),
        };
        #[cfg(test)]
        let generation = claim.generation;
        #[cfg(test)]
        let settled = Arc::clone(&claim.settled);
        let (stop, stopping) = watch::channel(false);
        let io = Arc::clone(&io);
        let host = tokio::spawn(async move {
            host_channel(
                io,
                channel,
                candidate,
                provider,
                root,
                gate,
                (&claim, stopping),
            )
            .await;
            drop(reservation);
            if claim.settled.load(Ordering::SeqCst) {
                claim.release();
            }
        });
        tasks.push(ManagedWriterHandle {
            #[cfg(test)]
            channel,
            #[cfg(test)]
            generation,
            #[cfg(test)]
            settled,
            stop: StopOnDrop {
                signal: stop,
                armed: true,
            },
            host,
            #[cfg(test)]
            keep,
        });
    }
    tasks
}

/// Resolves once the writer is told to stop; a detached writer, whose sender is gone, never is.
async fn stopped(mut stop: watch::Receiver<bool>) {
    if stop.wait_for(|stop| *stop).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// `step`'s output, or `None` once the writer was told to stop first.
async fn unless_stopped<T>(
    stop: &watch::Receiver<bool>,
    step: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        () = stopped(stop.clone()) => None,
        output = step => Some(output),
    }
}

fn hold(alarms: &impl AlarmSink, channel: u64, detail: &str) {
    tracing::error!(channel, detail, "[tui_o] writer host held the channel");
    let detail = format!("writer host: {detail}");
    alarms.raise(channel, WriterAlarm::Halted { detail });
}

/// A channel not adopted before its first `init` stays Legacy's for this process.
pub(super) fn release(candidate: &Candidate, alarms: &impl AlarmSink, channel: u64, detail: &str) {
    candidate.release(channel);
    let detail = format!("adoption held: {detail}");
    stop(candidate, alarms, channel, &detail);
}

/// Names a stop by the adoption it left: a released or deferred channel's output is Legacy's, so
/// only an owned or undecided channel is held.
pub(super) fn stop(candidate: &Candidate, alarms: &impl AlarmSink, channel: u64, detail: &str) {
    if !matches!(candidate.peek(), Adoption::Released | Adoption::Deferred) {
        return hold(alarms, channel, detail);
    }
    tracing::warn!(
        channel,
        detail,
        "[tui_o] writer host left the channel to Legacy"
    );
    let detail = format!("writer host: {detail}");
    alarms.raise(channel, WriterAlarm::Released { detail });
}

async fn host_channel<I: HostIo>(
    io: Arc<I>,
    channel: u64,
    candidate: Candidate,
    provider: ShadowProvider,
    runtime_root: PathBuf,
    gate: Arc<OwnershipGate>,
    (claim, stopping): (&ReadinessClaim, watch::Receiver<bool>),
) {
    let alarms = io.alarms();
    let boot = super::boot::Trace::new(channel, Some(provider), claim.generation, 0);
    let mut bindings = None;
    let phase = boot.phase("recover");
    let recovered = recover_boot(&runtime_root, channel, &stopping).await;
    phase.finish(if recovered.as_ref().is_some_and(Result::is_ok) {
        "ok"
    } else {
        "held"
    });
    let Some(recovered) = recovered else {
        return;
    };
    let store = match recovered {
        Ok(Recovered::Store(_)) if !candidate.confirm_store() => {
            return hold(
                &alarms,
                channel,
                "Legacy took the channel before its store was seen",
            );
        }
        Ok(Recovered::Store(store)) => store,
        Ok(Recovered::Fresh(fresh)) => {
            let phase = boot.phase("activate");
            let ids = (channel, provider);
            let started = first_init::start(
                &io,
                ids,
                &runtime_root,
                fresh,
                &candidate,
                (&gate, &stopping),
            );
            let Some(started) = started.await else {
                return;
            };
            phase.finish("ok");
            bindings = Some(started.bindings);
            started.store
        }
        Err(error) => return hold(&alarms, channel, &error.detail),
    };
    // Seeded before the port wait, so a recovered panel tick already knows O's newest post.
    let phase = boot.phase("wait_metadata");
    let metadata = fresh_input::wait_metadata(&store, &alarms, &stopping).await;
    phase.finish(if metadata { "ok" } else { "held" });
    if !metadata {
        return;
    }
    super::deliver::seed_last_posted(channel, store.ledger());
    let phase = boot.phase("port");
    let port = match unless_stopped(&stopping, io.port_for(channel)).await {
        Some(Ok(port)) => port,
        Some(Err(reason)) => {
            phase.finish("held");
            return hold(&alarms, channel, &reason);
        }
        None => return,
    };
    phase.finish("ok");
    let bindings = bindings.unwrap_or_else(|| io.bindings(channel, provider));
    let hosted = Hosted {
        io: &*io,
        channel,
        provider,
        runtime_root: &runtime_root,
        gate: &gate,
        claim,
        port,
        bindings,
        alarms,
        stopping,
        boot,
    };
    hosted.serve(store).await;
}

/// A channel's running host: what each start of its actor needs.
struct Hosted<'a, I: HostIo> {
    boot: super::boot::Trace,
    io: &'a I,
    channel: u64,
    provider: ShadowProvider,
    runtime_root: &'a Path,
    gate: &'a Arc<OwnershipGate>,
    claim: &'a ReadinessClaim,
    port: Arc<I::Port>,
    bindings: Arc<I::Bindings>,
    alarms: I::Alarms,
    stopping: watch::Receiver<bool>,
}

impl<I: HostIo> Hosted<'_, I> {
    /// Restarts from the recovered store after transient halts; other stops end hosting.
    async fn serve(self, mut store: ChannelStore) {
        let (channel, alarms) = (self.channel, &self.alarms);
        let mut backoff = Backoff::default();
        let mut attempt = 0u32;
        // Once a resume was tried, leaving the loop ends the wait for health as well.
        let settle = |attempt: u32| {
            if attempt > 0 {
                alarms.resume_pending(channel, false);
            }
        };
        loop {
            let started = tokio::time::Instant::now();
            let Some(cause) = self.run(store, attempt).await else {
                settle(attempt);
                return;
            };
            if !resume::resumable(&cause) {
                settle(attempt);
                self.io.writer_unavailable(channel);
                let alarm = &cause.alarm;
                tracing::error!(channel, ?alarm, "[tui_o] writer stopped and stays stopped");
                return;
            }
            if started.elapsed() >= resume::STABLE_RUN {
                backoff = Backoff::default();
            }
            alarms.resume_pending(channel, true);
            let Some(recovered) = self.recover_after(cause, &mut backoff, &mut attempt).await
            else {
                return;
            };
            store = recovered;
            super::deliver::seed_last_posted(channel, store.ledger());
            // Cleared before the next actor exists, so a halt it raises itself is never cleared.
            alarms.halt_cleared(channel);
        }
    }

    /// Starts one actor and returns how it stopped, once it has ended.
    async fn run(&self, store: ChannelStore, attempt: u32) -> Option<StopCause> {
        let boot = self.boot.attempt(attempt);
        let (channel, gate) = (self.channel, self.gate);
        let (port, lease) = (Arc::clone(&self.port), self.io.lease());
        // Built for each start, so a restarted actor reopens the sidecar and keeps no stale probe.
        let (repost, alarms) = (self.io.repost(channel, &port), self.alarms.clone());
        let phase = boot.phase("hosted");
        let hosted = ChannelWriter::hosted(store, Arc::clone(gate), port, lease, alarms, repost);
        // A stop during the receipt read ends this start before any actor exists.
        let mut writer = unless_stopped(&self.stopping, hosted).await?;
        phase.finish("ok");
        writer.boot = boot;
        let writer = writer.with_repost_stop(self.stopping.clone());
        let (stop_tx, stop) = watch::channel(false);
        let (resumed_tx, resumed) = watch::channel(false);
        // Each start publishes its own count; the ended actor's closed one reads as `None`.
        let (unsettled_tx, unsettled) = watch::channel(None);
        let owing = Owing::default();
        let owed = OwedView {
            published: owing.published.subscribe(),
            demand: owing.demand.clone(),
        };
        let config = WriterConfig { enabled: true };
        let bindings = Arc::clone(&self.bindings);
        let watches = (stop, resumed_tx, unsettled_tx, owing);
        let readers = crate::services::tui_o::channel_policy::boot()
            .filter(|boot| boot.dormant_channels().contains(&channel))
            .map(|_| ReaderConfig { dormant: true });
        let suspension = writer.suspension.clone();
        let spawn =
            || actor::spawn_projecting(&config, readers, writer, self.provider, bindings, watches);
        let (ports, spawned) = self.spawn_ports(suspension, spawn);
        let actor = spawned?;
        let watched = (resumed.clone(), unsettled, owed, self.stopping.clone());
        let readiness = &self.claim.readiness;
        readiness.track(channel, Arc::clone(gate), watched);
        let port = readiness.register_ports(channel, ports);
        let on_resumed = || {
            if attempt > 0 {
                self.alarms.resume_pending(channel, false);
                tracing::info!(
                    channel,
                    attempt,
                    "[tui_o] writer resumed in process after a halt"
                );
            }
        };
        let watched = (gate.subscribe(), resumed, self.stopping.clone());
        let ended = publish(channel, self.claim, watched, (actor, &stop_tx), on_resumed);
        let cause = ended.await;
        drop(stop_tx);
        readiness.close_transition(channel, port);
        cause
    }

    /// Waits and recovers the store until it opens; `None` once recovery fails for good. Evidence
    /// of an unposted piece is offered to every attempt and dropped with the first store it opens.
    async fn recover_after(
        &self,
        cause: StopCause,
        backoff: &mut Backoff,
        attempt: &mut u32,
    ) -> Option<ChannelStore> {
        let channel = self.channel;
        let (alarm, io, unsent) = (cause.alarm, cause.io, cause.unsent);
        let unsent_serial = unsent.as_ref().map(Unsent::serial);
        loop {
            let wait = backoff.next_wait();
            *attempt += 1;
            let (attempt, wait_secs) = (*attempt, wait.as_secs());
            tracing::warn!(
                channel,
                attempt,
                wait_secs,
                ?alarm,
                ?io,
                unsent_serial,
                "[tui_o] writer halted on a transient store error; resuming after a wait"
            );
            if unless_stopped(&self.stopping, tokio::time::sleep(wait))
                .await
                .is_none()
            {
                self.alarms.resume_pending(channel, false);
                return None;
            }
            let phase = self.boot.attempt(attempt).phase("recover");
            let recovered = recover(self.runtime_root, channel, unsent.as_ref());
            phase.finish(if recovered.is_ok() { "ok" } else { "held" });
            match recovered {
                Ok(Recovered::Store(store)) => return Some(store),
                Err(error) if error.transient => {
                    let detail = error.detail;
                    tracing::warn!(
                        channel,
                        attempt,
                        detail,
                        "[tui_o] writer store recovery failed transiently; waiting again"
                    );
                }
                Ok(Recovered::Fresh(_)) => {
                    self.alarms.resume_pending(channel, false);
                    self.io.writer_unavailable(channel);
                    hold(&self.alarms, channel, "init missing on resume");
                    return None;
                }
                Err(error) => {
                    self.alarms.resume_pending(channel, false);
                    self.io.writer_unavailable(channel);
                    hold(&self.alarms, channel, &error.detail);
                    return None;
                }
            }
        }
    }
}

/// Returns once the gate is Owned; a gate not yet acquired at startup is waited on, not held.
pub(super) async fn until_owned(gate: &OwnershipGate) {
    let mut watch = gate.subscribe();
    while watch.borrow_and_update().epoch().is_none() {
        if watch.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Publishes readiness and joins the actor on every normal exit; failed joins retain the claim.
async fn publish(
    channel: u64,
    claim: &ReadinessClaim,
    (mut gate, mut resumed, stop): Published,
    (mut actor, stop_actor): (JoinHandle<Option<StopCause>>, &watch::Sender<bool>),
    on_resumed: impl FnOnce(),
) -> Option<StopCause> {
    let readiness = &claim.readiness;
    let mut on_resumed = Some(on_resumed);
    let ended = loop {
        let owned = gate.borrow_and_update().epoch().is_some();
        let up = *resumed.borrow_and_update();
        if up && let Some(on_resumed) = on_resumed.take() {
            on_resumed();
        }
        readiness.set(channel, owned && up && !*stop.borrow());
        tokio::select! {
            changed = gate.changed() => if changed.is_err() {
                stop_actor.send_replace(true);
                break (&mut actor).await;
            },
            // A closed flag means the actor is returning; its task settles right after.
            changed = resumed.changed() => if changed.is_err() {
                readiness.set(channel, false);
                break (&mut actor).await;
            },
            ended = &mut actor => break ended,
            () = stopped(stop.clone()) => {
                readiness.set(channel, false);
                stop_actor.send_replace(true);
                break (&mut actor).await;
            }
        }
    };
    readiness.set(channel, false);
    ended.unwrap_or_else(|error| {
        claim.settled.store(false, Ordering::SeqCst);
        tracing::error!(channel, %error, "writer actor ended abnormally; hosting remains reserved");
        None
    })
}

/// A gateway stand-in for tests that drive the real host: every POST is recorded with its
/// channel, and each channel binds the one source it was given.
#[cfg(test)]
#[path = "host_io_tests.rs"]
pub(crate) mod test_io;

#[cfg(test)]
mod claim_tests {
    use super::*;

    #[test]
    fn an_old_claim_drop_leaves_a_replacement_ready_and_hosted() {
        let ready = Arc::new(Readiness::default());
        let old = ready.claim(7).unwrap();
        old.clear(true);
        let current = ready.claim(7).unwrap();
        ready.set(7, true);
        drop(old);
        assert!(ready.is_hosted(7) && ready.is_ready(7));
        current.release();
        assert!(!ready.is_hosted(7) && !ready.is_ready(7));
    }
}
