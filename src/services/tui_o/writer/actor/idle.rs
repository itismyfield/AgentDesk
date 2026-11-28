//! Cheap idle observation and lossless wakeups for a publishing actor.

use super::*;
use crate::services::tui_o::{
    ownership::GatewayOwnership,
    shadow::capture::Fingerprint,
    store::{ChannelStore, wake::Listener},
};

#[derive(PartialEq)]
struct Stamp {
    transcripts: Vec<Option<Fingerprint>>,
    spool: u64,
    restart_notices: Option<Fingerprint>,
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
            restart_notices: store.restart_notice_stamp().ok()?,
        })
    }
}

pub(super) struct Tick {
    stamp: Option<Stamp>,
    listener: Listener,
    binding: Option<watch::Receiver<u64>>,
    ownership: Option<watch::Receiver<GatewayOwnership>>,
    observations: Observations,
}

struct Observations {
    opened: tokio::time::Instant,
    ticks: u64,
    eligible: u64,
    skipped: u64,
}

impl Observations {
    fn new() -> Self {
        Self {
            opened: tokio::time::Instant::now(),
            ticks: 0,
            eligible: 0,
            skipped: 0,
        }
    }

    fn add(&mut self, channel: u64, can_skip: bool, skipped: bool) {
        self.ticks += 1;
        self.eligible += u64::from(can_skip);
        self.skipped += u64::from(skipped);
        if self.opened.elapsed() >= Duration::from_secs(60) {
            tracing::info!(
                channel,
                ticks = self.ticks,
                can_skip = self.eligible,
                stamp_skipped = self.skipped,
                "[tui_o] writer idle observations"
            );
            *self = Self::new();
        }
    }
}

impl Tick {
    pub(super) fn new<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents>(
        actor: &Actor<P, L, A, B>,
    ) -> Self {
        let channel = actor.writer.channel();
        Self {
            stamp: None,
            listener: Listener::new(channel),
            binding: Some(actor.bindings.subscribe(channel)),
            ownership: Some(actor.writer.gate.subscribe()),
            observations: Observations::new(),
        }
    }

    pub(super) fn needs_work(&mut self, store: &ChannelStore, can_skip: bool) -> bool {
        // Sample before work so a write racing with capture still differs on the next tick.
        let stamp = can_skip.then(|| Stamp::read(store)).flatten();
        let same = stamp.is_some() && stamp == self.stamp;
        self.stamp = stamp;
        self.observations.add(store.init().channel, can_skip, same);
        !same
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
            () = self.listener.notified() => {},
            () = demand.0.wake.notified() => {},
            () = async {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            } => return true,
        }
        self.stamp = None;
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
