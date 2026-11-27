//! Cheap idle observation and lossless wakeups for a publishing actor.

use super::*;
use crate::services::tui_o::{
    ownership::GatewayOwnership, shadow::capture::Fingerprint, store::ChannelStore,
};
use std::sync::{LazyLock, Mutex, Weak};
use tokio::time::Instant;

const IDLE: Duration = Duration::from_secs(5);
const MAX_IDLE: Duration = Duration::from_secs(15);

type Listeners = HashMap<u64, Vec<Weak<Notify>>>;
static LISTENERS: LazyLock<Mutex<Listeners>> = LazyLock::new(Mutex::default);

/// Called after a producer publishes work; each active run has its own stored permit.
pub(crate) fn wake(channel: u64) {
    let listeners = LISTENERS.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(runs) = listeners.get(&channel) {
        for notify in runs.iter().filter_map(Weak::upgrade) {
            notify.notify_one();
        }
    }
}

struct Listener(u64, Arc<Notify>);

impl Listener {
    fn new(channel: u64) -> Self {
        let notify = Arc::new(Notify::new());
        let mut listeners = LISTENERS.lock().unwrap_or_else(|error| error.into_inner());
        listeners
            .entry(channel)
            .or_default()
            .push(Arc::downgrade(&notify));
        Self(channel, notify)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let mut listeners = LISTENERS.lock().unwrap_or_else(|error| error.into_inner());
        if let std::collections::hash_map::Entry::Occupied(mut entry) = listeners.entry(self.0) {
            entry
                .get_mut()
                .retain(|run| !std::ptr::eq(run.as_ptr(), Arc::as_ptr(&self.1)));
            if entry.get().is_empty() {
                entry.remove();
            }
        }
    }
}

#[derive(PartialEq)]
struct Stamp {
    transcripts: Vec<Option<Fingerprint>>,
    spool: u64,
}

impl Stamp {
    fn read(store: &ChannelStore) -> Option<Self> {
        if store.io_failure().is_some() {
            return None;
        }
        let transcripts = store
            .cursors()
            .map(|cursor| match std::fs::metadata(&cursor.source.path) {
                Ok(meta) => Some(Some(Fingerprint::of(&meta).ok()?)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(None),
                Err(_) => None,
            })
            .collect::<Option<_>>()?;
        Some(Self {
            transcripts,
            spool: store.spool_generation(),
        })
    }
}

pub(super) struct Tick {
    stamp: Option<Stamp>,
    interval: Duration,
    full: Instant,
    listener: Listener,
    binding: Option<watch::Receiver<u64>>,
    ownership: Option<watch::Receiver<GatewayOwnership>>,
}

impl Tick {
    pub(super) fn new<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents>(
        actor: &Actor<P, L, A, B>,
    ) -> Self {
        let channel = actor.writer.channel();
        Self {
            stamp: None,
            interval: POLL_INTERVAL,
            full: Instant::now(),
            listener: Listener::new(channel),
            binding: Some(actor.bindings.subscribe(channel)),
            ownership: Some(actor.writer.gate.subscribe()),
        }
    }

    pub(super) fn poll(&mut self, store: &ChannelStore, can_skip: bool) -> (bool, Duration) {
        // Sample before work so a write racing with capture still differs on the next tick.
        let stamp = can_skip.then(|| Stamp::read(store)).flatten();
        let same = stamp.is_some() && stamp == self.stamp;
        self.stamp = stamp;
        if same {
            self.interval = if self.interval == POLL_INTERVAL {
                IDLE
            } else {
                MAX_IDLE
            };
            let remaining = MAX_IDLE.saturating_sub(self.full.elapsed());
            if !remaining.is_zero() {
                return (false, self.interval.min(remaining));
            }
        } else {
            self.interval = POLL_INTERVAL;
        }
        // Maintenance also sees state without a cheap stamp, such as retirement deadlines.
        self.full = Instant::now();
        (true, self.interval)
    }

    pub(super) async fn wait<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents>(
        &mut self,
        actor: &mut Actor<P, L, A, B>,
        demand: &Demand,
        commands: &mut Option<codex_submission::Receiver>,
        stop: &mut watch::Receiver<bool>,
        delay: Duration,
    ) -> bool {
        tokio::select! {
            biased;
            result = stop.changed() => if result.is_err() || *stop.borrow() { return false; },
            request = command(commands) => match request {
                Some(request) => commands.as_ref().unwrap().apply(&mut actor.writer, request),
                None => *commands = None,
            },
            () = changed(&mut self.binding) => {},
            () = changed(&mut self.ownership) => {},
            () = self.listener.1.notified() => {},
            () = demand.0.wake.notified() => {},
            () = actor.transitions.notified() => {},
            () = tokio::time::sleep(delay) => return true,
        }
        self.stamp = None;
        self.interval = POLL_INTERVAL;
        true
    }
}

async fn changed<T>(receiver: &mut Option<watch::Receiver<T>>) {
    if let Some(watch) = receiver
        && watch.changed().await.is_ok()
    {
        return;
    }
    *receiver = None;
    std::future::pending().await
}

async fn command(
    receiver: &mut Option<codex_submission::Receiver>,
) -> Option<codex_submission::Request> {
    match receiver {
        Some(receiver) => receiver.next().await,
        None => std::future::pending().await,
    }
}
