//! Starts, only with `runtime.channel_home_delegation_enabled` on, the delegated homes a provider
//! runtime's boot rows name: one watch per home, one lease per held one, ended before a replacement.

#[cfg(test)]
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use sqlx::PgPool;
use tokio::task::JoinHandle;

pub(crate) use super::channel_home::HomeGate;
use super::channel_home::{self, RENEW_EVERY};
pub(crate) use super::channel_home_drain::ResetRefused;
use super::channel_home_drain::{self as drain, Blocker, DrainPort, Owed, finish_return};
use super::channel_home_port::ChannelHomePort;
pub(crate) use super::channel_home_port::PersistedInput;
use super::home_supervisor::{HomeBundle, Phase, Settled, StopReason, Supervisor};
use crate::db::o_channel_homes::{self, ChannelHome, HomeError, HomeState, HomeWrite};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::channel_policy::{self, BootChannels, Candidate};
use crate::services::tui_o::cutover;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::writer::host::{
    self, HostIo, HostParts, ManagedWriterHandle, Readiness,
};

type ResetFuture = Pin<Box<dyn Future<Output = Result<(), ResetRefused>> + Send>>;

/// The gateway side's existing session reset for a releasing drain, built by the runtime.
pub(crate) type LegacyReset = Arc<dyn Fn() -> ResetFuture + Send + Sync>;

type RestoreFuture = Pin<Box<dyn Future<Output = bool> + Send>>;

/// A runtime's restore of one held channel's persisted input; `true` only once it is installed.
pub(crate) type ChannelRestore = Arc<dyn Fn() -> RestoreFuture + Send + Sync>;

/// The holder's one boot restore and the provider its recovery permit names.
#[derive(Clone)]
struct BootRestore {
    provider: String,
    run: ChannelRestore,
    tried: Arc<AtomicBool>,
}

/// The drain's reads of the channel in this process, the reset its runtime supplies and the
/// runtime's "persisted turns restored" flag a released channel's adoption waits on.
pub(crate) struct BootPort<R = ChannelHomePort> {
    reads: Arc<R>,
    reset: LegacyReset,
    restored: Arc<AtomicBool>,
    restore: Option<BootRestore>,
}

impl<R> Clone for BootPort<R> {
    fn clone(&self) -> Self {
        let (reads, reset) = (Arc::clone(&self.reads), Arc::clone(&self.reset));
        let restored = Arc::clone(&self.restored);
        Self {
            reads,
            reset,
            restored,
            restore: self.restore.clone(),
        }
    }
}

impl<R> BootPort<R> {
    /// Unrestored, so it adopts nothing until [`BootPort::restored_by`] names the runtime's flag.
    pub(crate) fn new(reads: R, reset: LegacyReset) -> Self {
        let reads = Arc::new(reads);
        let restored = Arc::default();
        Self {
            reads,
            reset,
            restored,
            restore: None,
        }
    }

    pub(crate) fn restored_by(mut self, restored: Arc<AtomicBool>) -> Self {
        self.restored = restored;
        self
    }

    /// Restores the channel once this node holds it: `restored` turns true only if `run` says so.
    pub(crate) fn restoring(mut self, provider: &str, run: ChannelRestore) -> Self {
        let (provider, tried) = (provider.to_owned(), Arc::default());
        self.restore = Some(BootRestore {
            provider,
            run,
            tried,
        });
        self
    }

    fn restored(&self) -> bool {
        self.restored.load(Ordering::Acquire)
    }

    /// Whether the boot restore is settled for `epoch`: none to run, already tried, or run now under
    /// a recovery permit once the renewal holds it. A failed restore leaves `restored` false.
    async fn restore_settled(&self, home: &HomeGate, epoch: i64) -> bool {
        let Some(restore) = &self.restore else {
            return true;
        };
        if restore.tried.load(Ordering::Acquire) {
            return true;
        }
        let Some(permit) = home.admit_at_epoch(&restore.provider, epoch, true) else {
            return false;
        };
        restore.tried.store(true, Ordering::Release);
        #[cfg(test)]
        if restore_mutant("standby_marks_restored_before_restore") {
            self.restored.store(true, Ordering::Release);
        }
        #[cfg(test)]
        let installed = restore_mutant("restored_always_true")
            || (!restore_mutant("standby_skips_persisted_restore") && (restore.run)().await);
        #[cfg(not(test))]
        let installed = (restore.run)().await;
        if installed {
            self.restored.store(true, Ordering::Release);
        }
        drop(permit);
        true
    }
}

/// Test-only mutants of the standby boot restore.
#[cfg(test)]
pub(crate) fn restore_mutant(name: &str) -> bool {
    std::env::var("ADK_S3A_RESTORE_MUTANT").ok().as_deref() == Some(name)
}

/// One channel's managed writer as the supervisor runs it.
pub(crate) struct WriterBundle(ManagedWriterHandle);

impl HomeBundle for WriterBundle {
    fn stop_and_join(self, _: StopReason) -> impl Future<Output = Settled> + Send {
        let stopped = self.0.stop_and_join();
        async move {
            match stopped.await {
                Ok(_) => Settled::Joined,
                Err(detail) => Settled::Stuck(detail),
            }
        }
    }
}

/// Starts a channel's writer under the gateway's authority; `None` when none is hosted.
pub(crate) type StartWriter = Arc<dyn Fn(u64) -> Option<ManagedWriterHandle> + Send + Sync>;

/// A gateway runtime's writers, one per channel under one supervisor: a home's watch ends the
/// channel's writer before the row returns and starts the gateway's one only after it.
pub(crate) struct ChannelWriters {
    supervisor: Arc<Supervisor<WriterBundle>>,
    gateway: Arc<OwnershipGate>,
    readiness: Arc<Readiness>,
    start: OnceLock<Option<StartWriter>>,
    /// The channels the boot's rows name another node for; `None` until read, so nothing starts.
    foreign: Mutex<Option<BTreeSet<u64>>>,
}

impl ChannelWriters {
    pub(crate) fn new(gateway: Arc<OwnershipGate>, readiness: Arc<Readiness>) -> Self {
        let supervisor = Arc::default();
        let start = OnceLock::new();
        Self {
            supervisor,
            gateway,
            readiness,
            start,
            foreign: Mutex::default(),
        }
    }

    fn read_boot(&self, foreign: Option<BTreeSet<u64>>) {
        *self.foreign.lock().unwrap_or_else(PoisonError::into_inner) = foreign;
    }

    /// The runtime's first writers: only the boot channels its row read admitted, none when it
    /// failed, supervised here with later starts armed under the gateway lease.
    pub(crate) fn start_boot<I: HostIo>(
        &self,
        provider: ShadowProvider,
        pg_gateway: bool,
        parts: impl Fn() -> HostParts<I> + Send + Sync + 'static,
    ) {
        let foreign = self
            .foreign
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        #[cfg(test)]
        let foreign = if writer_mutant("first_spawn_ignores_boot_rows") {
            Some(BTreeSet::new())
        } else {
            foreign
        };
        let mut owned = cutover::boot_ownership();
        owned.retain(|(channel, ..)| foreign.as_ref().is_some_and(|rows| !rows.contains(channel)));
        let started = host::start_managed(provider, pg_gateway, owned, &parts);
        let start = move |channel| {
            let owned = cutover::boot_ownership().into_iter();
            let owned = owned.filter(|(owned, ..)| *owned == channel).collect();
            host::start_managed(provider, true, owned, &parts).pop()
        };
        self.arm(pg_gateway.then(|| Arc::new(start) as StartWriter), started);
    }

    /// Supervises the writers the boot started and arms later starts; `None` (no gateway lease)
    /// starts none.
    pub(crate) fn arm(&self, start: Option<StartWriter>, started: Vec<ManagedWriterHandle>) {
        for handle in started {
            let (channel, bundle) = (handle.channel(), WriterBundle(handle));
            drop(
                self.supervisor
                    .start(channel, move |_| async move { Ok(bundle) }),
            );
        }
        let _ = self.start.set(start);
    }

    #[cfg(test)]
    pub(crate) fn phase(&self, channel: u64) -> Option<Phase> {
        self.supervisor.phase(channel)
    }

    /// Stops and joins the channel's writer; false while its end is not confirmed.
    pub(crate) async fn stop(&self, channel: u64) -> bool {
        let stopped = self.supervisor.stop(channel, None, StopReason::RowGone);
        stopped.await.is_none()
    }

    /// Whether a live writer runs the channel under the gateway's authority, starting one when
    /// none does: a running slot whose host ended or that posts under a home is replaced.
    pub(crate) async fn ensure(&self, channel: u64) -> bool {
        let Some(Some(start)) = self.start.get() else {
            return false;
        };
        if channel_home::registered_channel(channel).is_some() {
            return false;
        }
        let gateway = Arc::as_ptr(&self.gateway) as usize;
        #[cfg(test)]
        let any = writer_mutant("return_keeps_home_bound_writer");
        #[cfg(not(test))]
        let any = false;
        let ours = |bundle: &WriterBundle| any || bundle.0.authority() == gateway;
        let running = self.supervisor.running(channel, ours) == Some(true);
        #[cfg(test)]
        if running && writer_mutant("running_slot_counts_as_alive") {
            return true;
        }
        if running && self.readiness.is_hosted(channel) {
            return true;
        }
        if let Some(Phase::Blocked(..) | Phase::Starting(_) | Phase::Stopping(_)) =
            self.supervisor.phase(channel)
        {
            return false;
        }
        let start = Arc::clone(start);
        let build = move |_| async move {
            let started = start(channel).map(WriterBundle);
            started.ok_or_else(|| "no writer started".to_owned())
        };
        self.supervisor.start(channel, build).await.is_ok()
    }
}

/// Test-only mutants of the watch's writer return.
#[cfg(test)]
pub(crate) fn writer_mutant(name: &str) -> bool {
    std::env::var("ADK_S3B_MUTANT").ok().as_deref() == Some(name)
}

impl BootPort {
    /// The channel's reads against this process's writer, with `restored` as its runtime sets it.
    pub(crate) fn reading(channel: u64, restored: Arc<AtomicBool>, reset: LegacyReset) -> Self {
        let readiness = crate::services::tui_o::writer::host::process_readiness();
        let reads = ChannelHomePort::new(channel, readiness, Arc::clone(&restored));
        Self::new(reads, reset).restored_by(restored)
    }

    /// [`BootPort::reading`] that also counts the channel's persisted queue and marker as busy.
    pub(crate) fn reading_persisted(
        channel: u64,
        restored: Arc<AtomicBool>,
        persisted: PersistedInput,
        reset: LegacyReset,
    ) -> Self {
        let readiness = crate::services::tui_o::writer::host::process_readiness();
        let reads = ChannelHomePort::new(channel, readiness, Arc::clone(&restored));
        Self::new(reads.persisted_by(persisted), reset).restored_by(restored)
    }
}

impl<R: DrainPort + Send + Sync> DrainPort for BootPort<R> {
    fn turn_running(&self) -> impl Future<Output = Option<bool>> + Send {
        self.reads.turn_running()
    }

    fn owed(&self) -> impl Future<Output = Option<Owed>> + Send {
        self.reads.owed()
    }

    fn posts_in_flight(&self) -> impl Future<Output = Option<usize>> + Send {
        self.reads.posts_in_flight()
    }

    fn reset_legacy_source(&self) -> impl Future<Output = Result<(), ResetRefused>> + Send {
        (self.reset)()
    }
}

type Rows = Pin<Box<dyn Future<Output = Result<Vec<ChannelHome>, HomeError>> + Send>>;

/// Every home row, read when the boot awaits it.
pub(crate) fn listed(pool: &PgPool) -> Rows {
    let pool = pool.clone();
    Box::pin(async move { o_channel_homes::list_homes(&pool).await })
}

/// What a booting provider runtime brings once the switch is on.
pub(crate) struct Boot<P> {
    pub(crate) provider: String,
    /// `cluster.instance_id`: the holder or target name rows use for this node.
    pub(crate) local: String,
    pub(crate) pool: PgPool,
    pub(crate) rows: Rows,
    /// This provider's channels that may be delegated here (writer selection with a local Herdr
    /// endpoint): watched for a row naming this node, and held `Lost` when the rows cannot be read.
    pub(crate) candidates: Vec<u64>,
    pub(crate) port: P,
    /// The gateway runtime's writers a reclaimed channel returns to; none on other roles.
    pub(crate) writers: Option<Arc<ChannelWriters>>,
}

/// Registers a gate for each of this provider's rows naming this node holder or target, and starts
/// its watch and, for the holder, its lease; a candidate with no such row is watched without a
/// gate. `prepare` runs only with the switch on.
pub(crate) async fn start<P, R>(
    switch: Option<bool>,
    prepare: impl FnOnce() -> Option<Boot<P>>,
) -> Vec<Arc<HomeGate>>
where
    P: Fn(u64) -> BootPort<R>,
    R: DrainPort + Send + Sync + 'static,
{
    if switch != Some(true) {
        return Vec::new();
    }
    let Some(boot) = prepare() else {
        tracing::error!("channel homes need a PG pool and cluster.instance_id; none started");
        return Vec::new();
    };
    let read = boot.rows.await;
    if let Some(writers) = &boot.writers {
        let foreign = read.as_ref().ok();
        let foreign = foreign.map(|rows| foreign_rows(rows, &boot.provider, &boot.local));
        #[cfg(test)]
        let foreign = foreign
            .or_else(|| writer_mutant("first_spawn_reads_error_as_absence").then(BTreeSet::new));
        writers.read_boot(foreign);
    }
    let (mut channels, unreadable) = match read {
        Ok(rows) => (named(&rows, &boot.provider, &boot.local), false),
        Err(error) => {
            tracing::error!(%error, "channel home rows unreadable at boot; candidates held");
            (boot.candidates.iter().map(u64::to_string).collect(), true)
        }
    };
    let mounted = channels.len();
    for candidate in &boot.candidates {
        let candidate = candidate.to_string();
        if !channels.contains(&candidate) {
            channels.push(candidate);
        }
    }
    let mut started = Vec::new();
    for (index, channel) in channels.into_iter().enumerate() {
        let Ok(id) = channel.parse::<u64>() else {
            tracing::error!(
                channel,
                "channel home row names no Discord channel; skipped"
            );
            continue;
        };
        stop(&channel).await;
        let home = (index < mounted).then(|| Arc::new(HomeGate::new(&channel, &boot.local)));
        if let Some(home) = &home {
            if unreadable {
                home.note_drain(Some(Blocker::RowUnreadable.as_str()));
            }
            channel_home::register(Arc::clone(home));
            started.push(Arc::clone(home));
        }
        let lease = Arc::new(LeaseSlot::default());
        let seen = Seen {
            pool: boot.pool.clone(),
            id,
            local: boot.local.clone(),
            mounted: home,
            candidate: boot.candidates.contains(&id),
            writers: boot.writers.clone(),
        };
        let watch = tokio::spawn(watch(seen, (boot.port)(id), Arc::clone(&lease)));
        live(|live| live.insert(channel, Watched { watch, lease }));
    }
    started
}

/// The provider's channels whose row names `local` holder or target.
fn named(rows: &[ChannelHome], provider: &str, local: &str) -> Vec<String> {
    let names = |row: &&ChannelHome| {
        row.provider == provider
            && (row.holder.as_deref() == Some(local) || row.target.as_deref() == Some(local))
    };
    rows.iter()
        .filter(names)
        .map(|row| row.channel_id.clone())
        .collect()
}

/// The channels with a row this boot mounts no gate for: another node's, or another provider's.
fn foreign_rows(rows: &[ChannelHome], provider: &str, local: &str) -> BTreeSet<u64> {
    let named = named(rows, provider, local);
    let ids = rows.iter().map(|row| &row.channel_id);
    let ids = ids.filter(|id| !named.contains(id));
    ids.filter_map(|id| id.parse().ok()).collect()
}

/// A holder's running lease and the epoch it renews.
#[derive(Default)]
struct LeaseSlot(Mutex<Option<(i64, JoinHandle<()>)>>);

impl LeaseSlot {
    fn locked(&self) -> MutexGuard<'_, Option<(i64, JoinHandle<()>)>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ends the running lease, if any, before it returns.
    async fn end(&self) {
        let running = self.locked().take();
        if let Some((_, handle)) = running {
            end(handle).await;
        }
    }

    /// Renews at `epoch`: a lease still running there stays, any other is ended first.
    async fn renew_at(&self, pool: &PgPool, home: &Arc<HomeGate>, epoch: i64) {
        let running = self.locked().take();
        if let Some((at, handle)) = running {
            if at == epoch && !handle.is_finished() {
                *self.locked() = Some((at, handle));
                return;
            }
            end(handle).await;
        }
        let renewing = channel_home::run_lease(pool.clone(), Arc::clone(home), epoch);
        *self.locked() = Some((epoch, tokio::spawn(renewing)));
    }
}

async fn end(handle: JoinHandle<()>) {
    handle.abort();
    let _ = handle.await;
}

/// What one watch follows: its channel, the gate it has mounted while a row names this node, and
/// whether it outlives the row (a candidate does).
struct Seen {
    pool: PgPool,
    id: u64,
    local: String,
    mounted: Option<Arc<HomeGate>>,
    candidate: bool,
    writers: Option<Arc<ChannelWriters>>,
}

impl Seen {
    /// The mounted gate, registering a fresh one when none is.
    fn mount(&mut self) -> Arc<HomeGate> {
        let (channel, local) = (self.id.to_string(), self.local.as_str());
        let home = self.mounted.get_or_insert_with(|| {
            let home = Arc::new(HomeGate::new(&channel, local));
            channel_home::register(Arc::clone(&home));
            home
        });
        Arc::clone(home)
    }

    fn unmount(&mut self) {
        if let Some(home) = self.mounted.take() {
            channel_home::unregister_if_same(&home);
        }
    }

    async fn stop_writer(&self) -> bool {
        match &self.writers {
            Some(writers) => writers.stop(self.id).await,
            None => true,
        }
    }

    async fn ensure_writer(&self) -> bool {
        match &self.writers {
            Some(writers) => writers.ensure(self.id).await,
            None => true,
        }
    }

    /// Reclaimed to this node: the channel's writer is joined before the row goes, and only once
    /// it is gone does the gateway's writer start.
    async fn give_back(&mut self, home: &Arc<HomeGate>, epoch: i64) {
        #[cfg(test)]
        if writer_mutant("return_starts_writer_before_old_join")
            && let Some(start) = self
                .writers
                .as_ref()
                .and_then(|writers| writers.start.get())
            && let Some(start) = start
        {
            drop(start(self.id));
        }
        #[cfg(test)]
        let joined = writer_mutant("return_keeps_home_bound_writer") || self.stop_writer().await;
        #[cfg(not(test))]
        let joined = self.stop_writer().await;
        if joined
            && finish_return(&self.pool, home, epoch)
                .await
                .unwrap_or(false)
        {
            self.mounted = None;
            self.ensure_writer().await;
        }
    }
}

/// Follows the row each renewal period: renews and drains as holder, adopts a release or ends a
/// reclaim as target, and while another node holds the channel keeps watching without a gate.
async fn watch<R>(mut seen: Seen, port: BootPort<R>, lease: Arc<LeaseSlot>)
where
    R: DrainPort + Send + Sync + 'static,
{
    while !seen.mounted.as_ref().is_some_and(|home| home.withdrawn()) {
        let read = o_channel_homes::read_home(&seen.pool, &seen.id.to_string()).await;
        #[cfg(test)]
        let read = match read {
            Err(_) if writer_mutant("return_read_error_as_absence") => Ok(None),
            read => read,
        };
        let name = seen.local.clone();
        let local = Some(name.as_str());
        match read {
            // An unreadable row is not its absence: nothing returns and no writer starts.
            Err(_) => {
                if let Some(home) = &seen.mounted {
                    home.note_drain(Some(Blocker::RowUnreadable.as_str()));
                }
            }
            Ok(None) => {
                seen.unmount();
                lease.end().await;
                if seen.ensure_writer().await && !seen.candidate {
                    break;
                }
            }
            Ok(Some(row)) if row.holder.as_deref() != local && row.target.as_deref() != local => {
                lease.end().await;
                #[cfg(test)]
                if writer_mutant("watch_ends_when_row_names_other_holder") {
                    seen.unmount();
                    break;
                }
                // Another node's channel: the gate goes only once this runtime's writer has ended.
                if seen.stop_writer().await {
                    seen.unmount();
                }
            }
            Ok(Some(row)) => {
                let home = seen.mount();
                home.note_drain(None);
                if row.holder.as_deref() != local {
                    lease.end().await;
                    // Adopted, the row names this node holder and the next read starts its lease.
                    let releases_here =
                        row.state == HomeState::Released && row.target.as_deref() == local;
                    if releases_here && port.restored() {
                        let (channel, holder) = (home.channel_id(), home.holder());
                        let adopted =
                            o_channel_homes::adopt(&seen.pool, channel, holder, row.epoch);
                        if let Ok(HomeWrite::Applied(_)) = adopted.await {
                            continue;
                        }
                    }
                    if row.state == HomeState::Reclaimed {
                        seen.give_back(&home, row.epoch).await;
                    }
                } else {
                    lease.renew_at(&seen.pool, &home, row.epoch).await;
                    // The boot restore runs before any drain round reads the channel's turns.
                    let settled = port.restore_settled(&home, row.epoch).await;
                    let draining =
                        matches!(row.state, HomeState::Releasing | HomeState::Reclaiming);
                    if draining && !settled {
                        home.note_drain(Some(Blocker::TurnUnreadable.as_str()));
                    } else if draining {
                        let blocker = match drain::drain_round(&seen.pool, &home, &port).await {
                            drain::DrainStep::Waiting(blocker) => Some(blocker.as_str()),
                            drain::DrainStep::Left(_) | drain::DrainStep::NotDraining => None,
                        };
                        home.note_drain(blocker);
                    }
                }
            }
        }
        #[cfg(test)]
        ROUNDS.with(|rounds| *rounds.borrow_mut().entry(seen.id).or_default() += 1);
        tokio::time::sleep(RENEW_EVERY).await;
    }
    lease.end().await;
}

#[cfg(test)]
thread_local! {
    static ROUNDS: RefCell<BTreeMap<u64, u64>> = const { RefCell::new(BTreeMap::new()) };
}

/// How many rounds the channel's watches on this thread have finished, for tests to order by.
#[cfg(test)]
pub(crate) fn rounds(channel: u64) -> u64 {
    ROUNDS.with(|rounds| rounds.borrow().get(&channel).copied().unwrap_or(0))
}

struct Watched {
    watch: JoinHandle<()>,
    lease: Arc<LeaseSlot>,
}

type Live = BTreeMap<String, Watched>;

#[cfg(not(test))]
static LIVE: Mutex<Live> = Mutex::new(BTreeMap::new());
#[cfg(test)]
thread_local! {
    static LIVE: Mutex<Live> = const { Mutex::new(BTreeMap::new()) };
}

fn live<R>(use_live: impl FnOnce(&mut Live) -> R) -> R {
    let locked =
        |live: &Mutex<Live>| use_live(&mut live.lock().unwrap_or_else(PoisonError::into_inner));
    #[cfg(not(test))]
    return locked(&LIVE);
    #[cfg(test)]
    LIVE.with(locked)
}

/// Ends a channel's watch and lease and waits for both, so nothing of an earlier start renews or
/// drains once this returns.
pub(crate) async fn stop(channel: &str) {
    let Some(watched) = live(|live| live.remove(channel)) else {
        return;
    };
    end(watched.watch).await;
    watched.lease.end().await;
}

/// Whether a channel's watch runs and the epoch its lease renews, for tests.
#[cfg(test)]
pub(crate) fn running(channel: &str) -> (bool, Option<i64>) {
    live(|live| {
        let Some(watched) = live.get(channel) else {
            return (false, None);
        };
        let lease = watched.lease.locked();
        let lease = lease.as_ref().filter(|(_, handle)| !handle.is_finished());
        (!watched.watch.is_finished(), lease.map(|(epoch, _)| *epoch))
    })
}

/// The boot channels a standby writer hosts: each registered here, selected for the writer and
/// `local` (its Herdr endpoint runs on this node), with its home or standby adoption.
pub(crate) fn delegated_boot_ownership(
    local: impl Fn(u64) -> bool,
) -> Vec<(u64, Option<RuntimeHandoffKind>, Option<Candidate>)> {
    if !channel_home::any_registered() {
        return Vec::new();
    }
    let enabled = cutover::writer_enabled();
    with_boot(|snapshot| {
        let Some(snapshot) = snapshot else {
            return Vec::new();
        };
        let channels = snapshot.channels();
        let delegated = |&&channel: &&u64| {
            local(channel) && channel_home::registered_channel(channel).is_some()
        };
        let judged = |&channel: &u64| {
            let kind = snapshot.kind(channel);
            let hosted = channel_policy::owns_output(enabled, channels, channel, kind);
            let adoption = snapshot.candidate(channel).or(snapshot.standby(channel));
            (channel, kind, adoption.filter(|_| hosted).cloned())
        };
        channels.iter().filter(delegated).map(judged).collect()
    })
}

fn with_boot<R>(read: impl FnOnce(Option<&BootChannels>) -> R) -> R {
    #[cfg(not(test))]
    return read(channel_policy::boot());
    #[cfg(test)]
    cutover::test_override::with_channels(read)
}

#[cfg(test)]
#[path = "channel_home_boot_tests.rs"]
mod tests;
