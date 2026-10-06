//! Starts the O writer on the gateway runtime: one actor per channel this home may adopt, creating
//! a new channel's first store unless Legacy took it. Ready only while resumed and Owned.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::activation::{self, ActivationFacts};
use super::actor::{self, Undelivered};
use super::adoption::{self, LegacyView};
use super::binding::BindingEvents;
use super::deferred;
use super::deliver::{ChannelWriter, StopCause};
use super::resume::{self, Backoff};
use super::{AlarmSink, DeliveryLease, DiscordPort, WriterAlarm, WriterConfig};
use crate::services::agent_protocol::RuntimeHandoffKind;
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

/// Legacy's local hold on a channel as the gateway reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Custody {
    Free,
    /// Only an inflight row, which outlives its turn when Legacy never clears it.
    Row,
    /// A pending start or a terminal delivery Legacy still owns.
    Active,
}

/// Channels with a hosted actor and those ready to take work.
#[derive(Default)]
pub struct Readiness {
    hosted: Mutex<BTreeSet<u64>>,
    ready: Mutex<BTreeSet<u64>>,
    live: Mutex<BTreeMap<u64, Live>>,
}

/// What a hosted channel's readiness is derived from, kept so intake can read it directly.
struct Live {
    gate: Arc<OwnershipGate>,
    resumed: watch::Receiver<bool>,
    unsettled: watch::Receiver<Option<usize>>,
    owing: watch::Receiver<Option<Undelivered>>,
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

    /// Only the first claim of a channel hosts it, so a channel never has two actors.
    fn claim(&self, channel: u64) -> bool {
        locked(&self.hosted).insert(channel)
    }

    fn track(&self, channel: u64, gate: Arc<OwnershipGate>, (resumed, unsettled, owing): Watched) {
        let live = Live {
            gate,
            resumed,
            unsettled,
            owing,
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
        let owned = matches!(live.gate.current(), GatewayOwnership::Owned { .. });
        owned && live.resumed.has_changed().is_ok() && *live.resumed.borrow()
    }

    /// Rotated-away sources the channel's running actor has not retired, as its last poll read
    /// them; `None` when no actor runs, its store could not be read or Herdr is not configured.
    pub fn rotation_unsettled(&self, channel: u64) -> Option<usize> {
        let live = locked(&self.live);
        let unsettled = &live.get(&channel)?.unsettled;
        unsettled.has_changed().ok()?;
        *unsettled.borrow()
    }

    /// The running actor's view of what the channel still owes, to read as it publishes; `None`
    /// when no actor runs. The map is locked only to clone it.
    pub fn undelivered(&self, channel: u64) -> Option<watch::Receiver<Option<Undelivered>>> {
        let live = locked(&self.live);
        let owing = &live.get(&channel)?.owing;
        Some(owing.clone())
    }
}

type Watched = (
    watch::Receiver<bool>,
    watch::Receiver<Option<usize>>,
    watch::Receiver<Option<Undelivered>>,
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

/// Spawns one host task per channel this provider's bot may adopt. Without a PG gateway lease the
/// gate never becomes Owned, so those channels are held with an alarm and get no actor.
pub fn start<I: HostIo>(
    provider: ShadowProvider,
    pg_gateway: bool,
    prepare: impl FnOnce() -> HostParts<I>,
) -> Vec<JoinHandle<()>> {
    let kind = match provider {
        ShadowProvider::Claude => RuntimeHandoffKind::ClaudeTui,
        ShadowProvider::Codex => RuntimeHandoffKind::CodexTui,
    };
    let ours = |(channel, channel_kind, candidate): (u64, _, Option<Candidate>)| {
        Some((channel, candidate.filter(|_| channel_kind == Some(kind))?))
    };
    let channels: Vec<_> = cutover::boot_ownership()
        .into_iter()
        .filter_map(ours)
        .collect();
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
        if !readiness.claim(channel) {
            continue;
        }
        let root = match (pg_gateway, &runtime_root) {
            (false, _) => Err("no PG gateway lease"),
            (true, None) => Err("runtime root unresolved"),
            (true, Some(root)) => Ok(root.clone()),
        };
        let root = match root {
            Ok(root) => root,
            Err(detail) => {
                hold(&io.alarms(), channel, detail);
                continue;
            }
        };
        let (gate, readiness) = (Arc::clone(&gate), Arc::clone(&readiness));
        let host = host_channel(
            Arc::clone(&io),
            channel,
            candidate,
            provider,
            root,
            gate,
            readiness,
        );
        tasks.push(tokio::spawn(host));
    }
    tasks
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
    readiness: Arc<Readiness>,
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
                && !adoption::legacy_started(&**legacy).await
            {
                return release(
                    &candidate,
                    &alarms,
                    channel,
                    "legacy cursor not established",
                );
            }
            let facts = loop {
                until_owned(&gate).await;
                let facts = io.activation_facts(channel, provider).await;
                // Facts read before a lost gate are not acted on; wait for Owned and read again.
                if matches!(gate.current(), GatewayOwnership::Owned { .. }) {
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
                    let seq = events.as_ref().map_or(0, |e| e.last().map_or(0, |e| e.seq));
                    let current = events.as_deref().ok().and_then(adoption::current);
                    let first = deferred::first(&*io, channel, provider, &legacy, events).await;
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
                            if !deferred::retry(waiting, refused, seq).await {
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
    let port = io.port().await;
    let bindings = bindings.unwrap_or_else(|| io.bindings(channel, provider));
    let hosted = Hosted {
        io: &*io,
        channel,
        provider,
        runtime_root: &runtime_root,
        gate: &gate,
        readiness: &readiness,
        port,
        bindings,
        alarms,
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
    readiness: &'a Readiness,
    port: Arc<I::Port>,
    bindings: Arc<I::Bindings>,
    alarms: I::Alarms,
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
        let (owing_tx, owing) = watch::channel(None);
        let config = WriterConfig { enabled: true };
        let bindings = Arc::clone(&self.bindings);
        let watches = (stop, resumed_tx, unsettled_tx, owing_tx);
        let spawned = actor::spawn_projecting(&config, writer, self.provider, bindings, watches);
        let actor = spawned?;
        let watched = (resumed.clone(), unsettled, owing);
        self.readiness.track(channel, Arc::clone(gate), watched);
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
        let ended = publish(
            channel,
            self.readiness,
            gate.subscribe(),
            resumed,
            actor,
            on_resumed,
        );
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
            tokio::time::sleep(wait).await;
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

/// Ready only while the actor has resumed and the gate is Owned; cleared once the actor ends.
/// Returns how the actor stopped, read from its finished task; `None` when the gate closed first.
async fn publish(
    channel: u64,
    readiness: &Readiness,
    mut gate: watch::Receiver<GatewayOwnership>,
    mut resumed: watch::Receiver<bool>,
    mut actor: JoinHandle<Option<StopCause>>,
    on_resumed: impl FnOnce(),
) -> Option<StopCause> {
    let mut on_resumed = Some(on_resumed);
    let ended = loop {
        let owned = matches!(*gate.borrow_and_update(), GatewayOwnership::Owned { .. });
        let up = *resumed.borrow_and_update();
        if up && let Some(on_resumed) = on_resumed.take() {
            on_resumed();
        }
        readiness.set(channel, owned && up);
        tokio::select! {
            changed = gate.changed() => if changed.is_err() { break None },
            // A closed flag means the actor is returning; its task settles right after.
            changed = resumed.changed() => if changed.is_err() {
                readiness.set(channel, false);
                break (&mut actor).await.ok().flatten();
            },
            ended = &mut actor => break ended.ok().flatten(),
        }
    };
    readiness.set(channel, false);
    ended
}

/// A gateway stand-in for tests that drive the real host: every POST is recorded with its
/// channel, and each channel binds the one source it was given.
#[cfg(test)]
pub(crate) mod test_io {
    use super::*;
    use crate::services::tui_o::shadow::SourceId;
    use crate::services::tui_o::writer::binding::{
        BindingCause, BindingEvent, BindingEvidence, BindingRecord, BindingTarget,
    };
    use crate::services::tui_o::writer::{PostOutcome, SeenMessage};

    #[derive(Default)]
    pub(crate) struct Posts(Mutex<Vec<(u64, SeenMessage)>>);

    impl Posts {
        pub(crate) fn to(&self, channel: u64) -> Vec<String> {
            let posts = locked(&self.0);
            let to = posts.iter().filter(|(c, _)| *c == channel);
            to.map(|(_, message)| message.content.clone()).collect()
        }
    }

    impl DiscordPort for Posts {
        fn bot_id(&self) -> u64 {
            42
        }

        fn post(
            &self,
            channel: u64,
            content: String,
        ) -> impl Future<Output = PostOutcome> + Send + 'static {
            let mut posts = locked(&self.0);
            let id = 101 + posts.len() as u64;
            let author_id = 42;
            let receipt = SeenMessage {
                id,
                author_id,
                content,
            };
            posts.push((channel, receipt.clone()));
            std::future::ready(PostOutcome::Created(receipt))
        }

        fn history_after(
            &self,
            channel: u64,
            after: u64,
        ) -> impl Future<Output = Result<Vec<SeenMessage>, String>> + Send {
            let posts = locked(&self.0);
            let page = posts.iter().filter(|(c, m)| *c == channel && m.id > after);
            std::future::ready(Ok(page.map(|(_, m)| m.clone()).collect()))
        }

        fn history_readable(&self, _: u64) -> bool {
            true
        }
    }

    pub(crate) struct AnyLease;

    impl DeliveryLease for AnyLease {
        type Held = ();
        fn try_acquire(&self, _: u64, _: u64) -> Option<()> {
            Some(())
        }
    }

    #[derive(Clone, Default)]
    pub(crate) struct Alarms(pub(crate) Arc<Mutex<Vec<(u64, WriterAlarm)>>>);

    impl AlarmSink for Alarms {
        fn raise(&self, channel: u64, alarm: WriterAlarm) {
            locked(&self.0).push((channel, alarm));
        }
    }

    pub(crate) struct Startup {
        event: BindingEvent,
        notice: watch::Sender<u64>,
    }

    impl BindingEvents for Startup {
        fn binding_events_since(
            &self,
            channel: u64,
            after: u64,
        ) -> Result<Vec<BindingEvent>, String> {
            let due = channel == self.event.channel_id && after < self.event.seq;
            Ok(due.then(|| self.event.clone()).into_iter().collect())
        }

        fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
            self.notice.subscribe()
        }
    }

    /// Reports `facts` for every channel (none blocking by default); the store's own checks and
    /// the adoption still apply. `on_facts` runs once as the next facts are read.
    pub(crate) struct TestHost {
        pub(crate) posts: Arc<Posts>,
        pub(crate) alarms: Alarms,
        sources: BTreeMap<u64, SourceId>,
        pub(crate) facts: Mutex<ActivationFacts>,
        pub(crate) on_facts: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        /// Legacy's relay state for channels that already hold output; fails closed when unset.
        pub(crate) legacy: Mutex<Option<Arc<dyn LegacyView>>>,
        /// The tmux session each channel's binding names, `host-<channel>` when unset.
        pub(crate) sessions: Mutex<BTreeMap<u64, String>>,
        /// Legacy's custody of a channel as the gateway reads it, as an inflight row; none when unset.
        pub(crate) custody: Mutex<Option<fn(u64) -> bool>>,
        /// Legacy's mailbox work and watcher emission for every channel; idle by default.
        pub(crate) busy: std::sync::atomic::AtomicBool,
        pub(crate) relaying: std::sync::atomic::AtomicBool,
    }

    impl TestHost {
        pub(crate) fn new(sources: impl IntoIterator<Item = (u64, SourceId)>) -> Arc<Self> {
            Arc::new(Self {
                posts: Arc::default(),
                alarms: Alarms::default(),
                sources: sources.into_iter().collect(),
                facts: Mutex::default(),
                on_facts: Mutex::default(),
                legacy: Mutex::default(),
                sessions: Mutex::default(),
                custody: Mutex::default(),
                busy: Default::default(),
                relaying: Default::default(),
            })
        }
    }

    impl HostIo for TestHost {
        type Port = Posts;
        type Lease = AnyLease;
        type Alarms = Alarms;
        type Bindings = Startup;

        fn port(&self) -> impl Future<Output = Arc<Posts>> + Send {
            std::future::ready(Arc::clone(&self.posts))
        }

        fn lease(&self) -> AnyLease {
            AnyLease
        }

        fn alarms(&self) -> Alarms {
            self.alarms.clone()
        }

        fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<Startup> {
            let source = self.sources.get(&channel).cloned();
            let source = source.unwrap_or_else(|| panic!("no source for channel {channel}"));
            let received_at = chrono::Utc::now();
            let evidence = BindingEvidence {
                hook_event: "SessionStart".into(),
                received_at,
                reclaims: false,
            };
            let record = BindingRecord::Bound {
                old: None,
                new: BindingTarget::Source(source),
                cause: BindingCause::Startup,
                parent_hint: None,
                evidence,
            };
            let event = BindingEvent {
                seq: 1,
                channel_id: channel,
                provider,
                tmux_session: locked(&self.sessions)
                    .get(&channel)
                    .cloned()
                    .unwrap_or_else(|| format!("host-{channel}")),
                execution_nonce: "host".into(),
                record,
                committed_at: received_at,
            };
            let notice = watch::channel(1).0;
            Arc::new(Startup { event, notice })
        }

        fn activation_facts(
            &self,
            _: u64,
            _: ShadowProvider,
        ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
            if let Some(hook) = locked(&self.on_facts).take() {
                hook();
            }
            std::future::ready(Ok(locked(&self.facts).clone()))
        }

        fn local_custody(&self, channel: u64, _: ShadowProvider) -> Result<Custody, String> {
            let custody = *locked(&self.custody);
            let row = custody.is_some_and(|custody| custody(channel));
            Ok(if row { Custody::Row } else { Custody::Free })
        }

        fn legacy(&self) -> Arc<dyn LegacyView> {
            let set = locked(&self.legacy).clone();
            set.unwrap_or_else(|| Arc::new(crate::services::tui_o::writer::adoption::NoLegacy))
        }

        fn legacy_busy(&self, _: u64) -> impl Future<Output = bool> + Send {
            std::future::ready(self.busy.load(std::sync::atomic::Ordering::SeqCst))
        }

        fn relaying(&self, _: u64) -> bool {
            self.relaying.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
}
