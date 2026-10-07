//! Starts, only with `runtime.channel_home_delegation_enabled` on, the delegated homes a provider
//! runtime's boot rows name: one watch per home, one lease per held one, ended before a replacement.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use sqlx::PgPool;
use tokio::task::JoinHandle;

pub(crate) use super::channel_home::HomeGate;
use super::channel_home::{self, RENEW_EVERY};
pub(crate) use super::channel_home_drain::ResetRefused;
use super::channel_home_drain::{self as drain, Blocker, DrainPort, Owed, finish_return};
use super::channel_home_port::ChannelHomePort;
use crate::db::o_channel_homes::{self, ChannelHome, HomeError, HomeState};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::channel_policy::{self, BootChannels, Candidate};
use crate::services::tui_o::cutover;

type ResetFuture = Pin<Box<dyn Future<Output = Result<(), ResetRefused>> + Send>>;

/// The gateway side's existing session reset for a releasing drain, built by the runtime.
pub(crate) type LegacyReset = Arc<dyn Fn() -> ResetFuture + Send + Sync>;

/// The drain's reads of the channel in this process and the reset its runtime supplies.
pub(crate) struct BootPort<R = ChannelHomePort> {
    reads: Arc<R>,
    reset: LegacyReset,
}

impl<R> Clone for BootPort<R> {
    fn clone(&self) -> Self {
        let (reads, reset) = (Arc::clone(&self.reads), Arc::clone(&self.reset));
        Self { reads, reset }
    }
}

impl<R> BootPort<R> {
    pub(crate) fn new(reads: R, reset: LegacyReset) -> Self {
        let reads = Arc::new(reads);
        Self { reads, reset }
    }
}

impl BootPort {
    /// The channel's reads against this process's writer, with `restored` as its runtime sets it.
    pub(crate) fn reading(channel: u64, restored: Arc<AtomicBool>, reset: LegacyReset) -> Self {
        let readiness = crate::services::tui_o::writer::host::process_readiness();
        Self::new(ChannelHomePort::new(channel, readiness, restored), reset)
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
    /// Channels that may be delegated here (writer selection with a local Herdr endpoint): held
    /// `Lost` when the rows cannot be read.
    pub(crate) candidates: Vec<u64>,
    pub(crate) port: P,
}

/// Registers a gate for each of this provider's rows naming this node holder or target, and starts
/// its watch and, for the holder, its lease. `prepare` runs only with the switch on.
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
    let (channels, unreadable) = match boot.rows.await {
        Ok(rows) => (named(&rows, &boot.provider, &boot.local), false),
        Err(error) => {
            tracing::error!(%error, "channel home rows unreadable at boot; candidates held");
            (boot.candidates.iter().map(u64::to_string).collect(), true)
        }
    };
    let mut started = Vec::new();
    for channel in channels {
        let Ok(id) = channel.parse::<u64>() else {
            tracing::error!(
                channel,
                "channel home row names no Discord channel; skipped"
            );
            continue;
        };
        stop(&channel).await;
        let home = Arc::new(HomeGate::new(&channel, &boot.local));
        if unreadable {
            home.note_drain(Some(Blocker::RowUnreadable.as_str()));
        }
        channel_home::register(Arc::clone(&home));
        let lease = Arc::new(LeaseSlot::default());
        let port = (boot.port)(id);
        let watching = watch(
            boot.pool.clone(),
            Arc::clone(&home),
            port,
            Arc::clone(&lease),
        );
        let watch = tokio::spawn(watching);
        live(|live| live.insert(channel, Watched { watch, lease }));
        started.push(home);
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

/// Follows the row each renewal period: renews and drains while it names this node holder, ends a
/// reclaim it targets, and returns the channel to the gateway rules once the row names it no more.
async fn watch<R>(pool: PgPool, home: Arc<HomeGate>, port: BootPort<R>, lease: Arc<LeaseSlot>)
where
    R: DrainPort + Send + Sync + 'static,
{
    while !home.withdrawn() {
        match o_channel_homes::read_home(&pool, home.channel_id()).await {
            Err(_) => home.note_drain(Some(Blocker::RowUnreadable.as_str())),
            Ok(row) => {
                let local = Some(home.holder());
                let row = row
                    .filter(|row| row.holder.as_deref() == local || row.target.as_deref() == local);
                let Some(row) = row else {
                    channel_home::unregister_if_same(&home);
                    break;
                };
                home.note_drain(None);
                if row.holder.as_deref() != local {
                    lease.end().await;
                    // The reclaim's target ends it: the row goes and the channel unregisters.
                    let reclaimed = row.state == HomeState::Reclaimed;
                    if reclaimed
                        && finish_return(&pool, &home, row.epoch)
                            .await
                            .unwrap_or(false)
                    {
                        break;
                    }
                } else {
                    lease.renew_at(&pool, &home, row.epoch).await;
                    if matches!(row.state, HomeState::Releasing | HomeState::Reclaiming) {
                        drain::run_drain(pool.clone(), Arc::clone(&home), port.clone()).await;
                    }
                }
            }
        }
        tokio::time::sleep(RENEW_EVERY).await;
    }
    lease.end().await;
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
