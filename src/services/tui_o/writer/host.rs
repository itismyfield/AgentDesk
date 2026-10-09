//! Starts the O writer on the gateway runtime: one actor per channel this home may adopt, creating
//! a new channel's first store unless Legacy took it. Ready only while resumed and Owned. A
//! delegated channel waits on, posts under and is ready by its registered home gate instead.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::activation::{self, ActivationFacts};
use super::actor::{self, Demand, Owing, Undelivered};
use super::adoption::{self, LegacyView};
use super::binding::BindingEvents;
use super::deferred;
use super::deliver::{ChannelWriter, StopCause};
use super::resume::{self, Backoff};
use super::{AlarmSink, DeliveryLease, DiscordPort, WriterAlarm, WriterConfig};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::cluster::channel_home::{self, HomeOwnership};
use crate::services::cluster::home_availability;
use crate::services::tui_o::channel_policy::{Adoption, Candidate};
use crate::services::tui_o::cutover;
use crate::services::tui_o::ownership::{GatewayOwnership, OwnershipGate};
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::store::ledger::Unsent;
use crate::services::tui_o::store::{ChannelStore, OStore, StoreConfig, StoreError, transient_io};

/// What the gateway runtime supplies; asked only for channels O owns.
pub trait HostIo: Send + Sync + 'static {
    type Port: DiscordPort;
    type Lease: DeliveryLease + 'static;
    type Alarms: AlarmSink + Clone + 'static;
    type Bindings: BindingEvents;
    /// Resolves once the gateway's HTTP client and the bot's own id are known.
    fn port(&self) -> impl Future<Output = Arc<Self::Port>> + Send;
    fn lease(&self) -> Self::Lease;
    fn alarms(&self) -> Self::Alarms;
    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<Self::Bindings>;
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
    /// What local Legacy inflight, custody or a pending start holds of the channel. Read under its
    /// adoption lock right before the first `init`, so it must not judge TUI output itself.
    fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<Custody, String>;
    /// Legacy's relay state, asked only for a Claude channel whose sources already hold output.
    fn legacy(&self) -> Arc<dyn LegacyView>;
    /// Whether Legacy's mailbox for the channel holds a turn, an intervention or a pending dispatch.
    fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send;
    /// Whether Legacy's watcher is emitting the channel's terminal delivery or its chrome now.
    fn relaying(&self, channel: u64) -> bool;
    /// Called once a first activation committed the channel, before it is ready for intake.
    fn adopted(&self, _channel: u64, _provider: ShadowProvider) {}
}

/// The facts and queued Legacy bodies read while intake writes are fenced.
pub struct FencedFacts {
    pub hold: crate::db::o_channel_activation::IntakeFence,
    pub facts: ActivationFacts,
    pub queued_bodies: i64,
}

/// Legacy's local hold on a channel as the gateway reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Custody {
    Free,
    /// Only an inflight row, which outlives its turn when Legacy never clears it.
    Row,
    /// A pending start or a terminal delivery Legacy still owns.
    Active,
}

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
    /// Ends this hosting: not ready, no live view, and the channel free to host again. Called
    /// only once its host and actor ended, so a new actor never runs beside them.
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
}

/// A hosted channel's owed pieces as a drain reads them: the actor's published view, and the
/// demand that makes the actor read its store at all.
#[derive(Clone)]
pub struct OwedView {
    pub published: watch::Receiver<Option<Undelivered>>,
    pub demand: Demand,
}

fn locked<T>(set: &Mutex<T>) -> MutexGuard<'_, T> {
    set.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Readiness {
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
        };
        locked(&self.live).insert(channel, live);
    }

    /// Ready and, read now rather than from the published flag that trails them, the gate is Owned
    /// and the actor is still running resumed.
    pub fn accepts(&self, channel: u64) -> bool {
        let live = locked(&self.live);
        let Some(live) = live.get(&channel).filter(|_| self.is_ready(channel)) else {
            return false;
        };
        let owned = owned_now(channel, &live.gate);
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

    /// The running actor's view of what the channel still owes; `None` when no actor runs. The
    /// map is locked only to clone it.
    pub fn undelivered(&self, channel: u64) -> Option<OwedView> {
        let live = locked(&self.live);
        Some(live.get(&channel)?.owed.clone())
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

/// Whether `channel` is Owned now: by its registered home gate with a lapsed hold ended, else by
/// the writer's gate.
fn owned_now(channel: u64, gate: &OwnershipGate) -> bool {
    match channel_home::registered_channel(channel) {
        Some(home) => matches!(home.ownership(), HomeOwnership::Owned { .. }),
        None => matches!(gate.current(), GatewayOwnership::Owned { .. }),
    }
}

/// Spawns one host task per channel this provider's bot may adopt. Without a PG gateway lease the
/// gate never becomes Owned, so those channels are held with an alarm and get no actor; a
/// delegated channel is gated by its registered home gate instead of that lease.
pub fn start<I: HostIo>(
    provider: ShadowProvider,
    pg_gateway: bool,
    prepare: impl FnOnce() -> HostParts<I>,
) -> Vec<JoinHandle<()>> {
    let owned = cutover::boot_ownership();
    detached(start_managed(provider, pg_gateway, owned, prepare))
}

/// [`start`] off the gateway lease for `delegated`, each a channel with a registered home gate.
pub fn start_delegated<I: HostIo>(
    provider: ShadowProvider,
    delegated: Vec<(u64, Option<RuntimeHandoffKind>, Option<Candidate>)>,
    prepare: impl FnOnce() -> HostParts<I>,
) -> Vec<JoinHandle<()>> {
    detached(start_managed(provider, false, delegated, prepare))
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
    channel: u64,
    generation: u64,
    authority: Arc<OwnershipGate>,
    settled: Arc<AtomicBool>,
    stop: StopOnDrop,
    host: JoinHandle<()>,
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
    pub fn channel(&self) -> u64 {
        self.channel
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Process-local identity of the captured gate, independent of registry replacement.
    pub fn authority(&self) -> usize {
        Arc::as_ptr(&self.authority) as usize
    }

    /// Leaves shutdown to the existing gateway lifecycle; the host still owns its claim.
    pub fn into_detached(mut self) -> JoinHandle<()> {
        self.stop.armed = false;
        self.host
    }

    /// Requests stop before returning the wait future; its cancellation never cancels cleanup.
    pub fn stop_and_join(
        self,
    ) -> impl Future<Output = Result<WriterStopped, String>> + Send + 'static {
        let Self {
            channel,
            generation,
            authority: _,
            settled,
            stop,
            host,
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

/// [`start`] over `owned`, returning a handle per hosted channel. A channel whose delegation is
/// unavailable is held with an alarm and not claimed.
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
        if let Some(reason) = home_availability::refusal(channel) {
            hold(&io.alarms(), channel, reason.as_str());
            continue;
        }
        let Some(claim) = readiness.claim(channel) else {
            continue;
        };
        let home = channel_home::registered_channel(channel);
        let root = match (pg_gateway || home.is_some(), &runtime_root) {
            (false, _) => Err("no PG gateway lease"),
            (true, None) => Err("runtime root unresolved"),
            (true, Some(root)) => Ok(root.clone()),
        };
        let root = match root {
            Ok(root) => root,
            Err(detail) => {
                hold(&io.alarms(), channel, detail);
                claim.release();
                continue;
            }
        };
        let gate = home.map_or_else(|| Arc::clone(&gate), |home| home.gate());
        let generation = claim.generation;
        let authority = Arc::clone(&gate);
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
            if claim.settled.load(Ordering::SeqCst) {
                claim.release();
            }
        });
        tasks.push(ManagedWriterHandle {
            channel,
            generation,
            authority,
            settled,
            stop: StopOnDrop {
                signal: stop,
                armed: true,
            },
            host,
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
    let mut bindings = None;
    let store = match recover(&runtime_root, channel, None) {
        Ok(Recovered::Store(_)) if !candidate.confirm_store() => {
            return hold(
                &alarms,
                channel,
                "Legacy took the channel before its store was seen",
            );
        }
        Ok(Recovered::Store(store)) => store,
        Ok(Recovered::Fresh(fresh)) => {
            let log = bindings.insert(io.bindings(channel, provider));
            let held =
                provider == ShadowProvider::Claude && adoption::holds_output(&**log, channel);
            let legacy = held.then(|| io.legacy());
            if let Some(legacy) = &legacy
                && unless_stopped(&stopping, adoption::legacy_started(&**legacy)).await
                    != Some(true)
            {
                return release(
                    &candidate,
                    &alarms,
                    channel,
                    "legacy cursor not established",
                );
            }
            let facts = loop {
                if unless_stopped(&stopping, until_owned(&gate))
                    .await
                    .is_none()
                {
                    return;
                }
                let Some(facts) =
                    unless_stopped(&stopping, io.activation_facts(channel, provider)).await
                else {
                    return;
                };
                // Facts read before a lost gate are not acted on; wait for Owned and read again.
                if owned_now(channel, &gate) {
                    break facts;
                }
            };
            let local = || Ok(io.local_custody(channel, provider)? != Custody::Free);
            let created = match legacy {
                None => activation::activate(&fresh, channel, facts, &**log, local, &candidate),
                Some(legacy) => 'held: {
                    // Sessions on another node or an override end it before an open turn defers it.
                    let last = facts.as_ref().ok().and_then(ActivationFacts::final_blocker);
                    if let Some(detail) = last {
                        return release(&candidate, &alarms, channel, &detail);
                    }
                    let events = log.binding_events_since(channel, 0);
                    let events = events.map_err(|error| format!("binding log: {error}"));
                    // O's own copy names each file once; the shared log keeps every dev it read.
                    let events = events.map(super::renumbered::first_named);
                    let seq = events.as_ref().map_or(0, |e| e.last().map_or(0, |e| e.seq));
                    let current = events.as_deref().ok().and_then(adoption::current);
                    let first = deferred::first(&*io, channel, provider, &legacy, events);
                    let Some(first) = unless_stopped(&stopping, first).await else {
                        return;
                    };
                    let snapshot = match first {
                        deferred::First::Adopt(snapshot) => snapshot,
                        deferred::First::Wait(refused) => {
                            let detail = refused.to_string();
                            let Some((source, _)) = current else {
                                return release(&candidate, &alarms, channel, &detail);
                            };
                            if !candidate.defer(channel) {
                                return release(&candidate, &alarms, channel, &detail);
                            }
                            tracing::info!(channel, %refused, "[tui_o] adoption waits for Legacy");
                            let waiting = deferred::Waiting {
                                io: &*io,
                                channel,
                                provider,
                                candidate: &candidate,
                                gate: &*gate,
                                store: &fresh,
                                log: &**log,
                                legacy,
                                bound: (source, seq),
                            };
                            let retried = deferred::retry(waiting, refused, seq);
                            if unless_stopped(&stopping, retried).await != Some(true) {
                                return;
                            }
                            break 'held Ok(());
                        }
                        deferred::First::Leave(refused) => {
                            return release(&candidate, &alarms, channel, &refused.to_string());
                        }
                    };
                    let sources = || {
                        snapshot
                            .recheck(&*legacy, &**log, channel)
                            .map_err(String::from)
                    };
                    activation::activate_with(&fresh, channel, facts, local, &candidate, sources)
                }
            };
            if let Err(detail) = created {
                let detail = format!("first activation: {detail}");
                return stop(&candidate, &alarms, channel, &detail);
            }
            match recover(&runtime_root, channel, None) {
                Ok(Recovered::Store(store)) => {
                    io.adopted(channel, provider);
                    store
                }
                Ok(Recovered::Fresh(_)) => {
                    return hold(&alarms, channel, "init missing after activation");
                }
                Err(error) => return hold(&alarms, channel, &error.detail),
            }
        }
        Err(error) => return hold(&alarms, channel, &error.detail),
    };
    // Seeded before the port wait, so a recovered panel tick already knows O's newest post.
    super::deliver::seed_last_posted(channel, store.ledger());
    let Some(port) = unless_stopped(&stopping, io.port()).await else {
        return;
    };
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
    };
    hosted.serve(store).await;
}

/// A channel's running host: what each start of its actor needs.
struct Hosted<'a, I: HostIo> {
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
    /// Runs the actor; after a transient store halt, waits and starts it again from the store as
    /// a restart recovers it, until it stops for any other reason.
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
        let (channel, gate) = (self.channel, self.gate);
        let (port, lease) = (Arc::clone(&self.port), self.io.lease());
        let writer = ChannelWriter::new(store, Arc::clone(gate), port, lease, self.alarms.clone());
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
        let spawned = actor::spawn_projecting(&config, writer, self.provider, bindings, watches);
        let actor = spawned?;
        let watched = (resumed.clone(), unsettled, owed, self.stopping.clone());
        self.claim
            .readiness
            .track(channel, Arc::clone(gate), watched);
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
            match recover(self.runtime_root, channel, unsent.as_ref()) {
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
                    hold(&self.alarms, channel, "init missing on resume");
                    return None;
                }
                Err(error) => {
                    self.alarms.resume_pending(channel, false);
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
    while !matches!(*watch.borrow_and_update(), GatewayOwnership::Owned { .. }) {
        if watch.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

enum Recovered {
    Store(ChannelStore),
    /// Neither the era nor an `init` names the channel: only a first activation may create it.
    Fresh(OStore),
}

/// Why a store was not recovered; a transient one may recover on a later attempt.
struct Unrecovered {
    detail: String,
    transient: bool,
}

impl Unrecovered {
    fn new(detail: String, io: Option<std::io::ErrorKind>) -> Self {
        let transient = io.is_some_and(transient_io);
        Self { detail, transient }
    }

    fn of_store(context: &str, error: &StoreError) -> Self {
        let io = match error {
            StoreError::Io(error) => Some(error.kind()),
            _ => None,
        };
        Self::new(format!("{context}: {error:?}"), io)
    }
}

impl From<&str> for Unrecovered {
    fn from(detail: &str) -> Self {
        Self::new(detail.into(), None)
    }
}

/// Recovers the channel's store, first taking back `unsent` if the ledger still ends with it.
/// Damage, a foreign init, an era channel without init or an init without era holds it.
fn recover(
    runtime_root: &Path,
    channel: u64,
    unsent: Option<&Unsent>,
) -> Result<Recovered, Unrecovered> {
    let config = StoreConfig { enabled: true };
    let store = OStore::open_if_enabled(&config, runtime_root)
        .map_err(|error| Unrecovered::new(format!("store: {error}"), Some(error.kind())))?
        .ok_or_else(|| Unrecovered::from("store disabled"))?;
    let era = store
        .read_era()
        .map_err(|error| Unrecovered::of_store("era", &error))?;
    let Some(era) = era else {
        return match store.read_init(channel) {
            Ok(None) => Ok(Recovered::Fresh(store)),
            Ok(Some(_)) => Err("no writer era".into()),
            Err(error) => Err(Unrecovered::of_store("init", &error)),
        };
    };
    let opened = store.open_channel_withdrawing(&era, channel, unsent);
    let opened = opened.map_err(|halt| Unrecovered::new(format!("recovery: {halt:?}"), halt.io))?;
    let Some(opened) = opened else {
        return Ok(Recovered::Fresh(store));
    };
    match opened.init().channel {
        stored if stored != channel => Err(Unrecovered::new(
            format!("store names channel {stored}"),
            None,
        )),
        _ => Ok(Recovered::Store(opened)),
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
        let owned = matches!(*gate.borrow_and_update(), GatewayOwnership::Owned { .. });
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
