//! One channel's O actor: replays the spool, follows source binds, spools captured bytes and
//! delivers owed pieces in order. Capture goes on while the gateway is not Owned.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::binding::BindingEvents;
use super::deliver::{ChannelWriter, Step, StopCause};
use super::pieces::{Derived, UnitDeriver};
use super::rotation::Sources;
use super::{AlarmSink, DeliveryLease, DiscordPort, WriterAlarm, WriterConfig};
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey};
use crate::services::tui_o::store::rotation::Boundary;
use crate::services::tui_o::store::spool::source_key;

pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
const WAIT_LIMIT: Duration = Duration::from_secs(300);

/// Runs the channel's actor only when the channel's boot ownership enabled `config`.
pub fn spawn_if_enabled<P, L, A, B>(
    config: &WriterConfig,
    writer: ChannelWriter<P, L, A>,
    provider: ShadowProvider,
    bindings: Arc<B>,
    stop: watch::Receiver<bool>,
    resumed: watch::Sender<bool>,
) -> Option<JoinHandle<Option<StopCause>>>
where
    P: DiscordPort,
    L: DeliveryLease + 'static,
    A: AlarmSink + 'static,
    B: BindingEvents,
{
    let (unsettled, owing) = (watch::channel(None).0, Owing::default());
    spawn_projecting(
        config,
        writer,
        provider,
        bindings,
        (stop, resumed, unsettled, owing),
    )
}

/// After each poll the actor of a Herdr-configured channel publishes how many rotated-away
/// sources it has not retired yet; `None` while its store cannot be read, or for other channels.
pub type Unsettled = watch::Sender<Option<usize>>;

/// What a Herdr-configured channel's actor still owes Discord, as its last poll read it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Undelivered {
    /// Derived items not delivered yet.
    pub owed: usize,
    /// A `Prepared` piece without a recorded result.
    pub prepared: usize,
    /// Announced units whose sealing record is still to come.
    pub unsealed: usize,
    /// Sources not retired whose spooled cursor is behind the file's length.
    pub uncaptured: usize,
    /// Binds past the checkpoint not applied yet, and sources whose start waits on an operator.
    pub binding_pending: usize,
    /// The last read that had asked when this poll began; a read takes only a poll begun after it.
    pub asked: u64,
}

#[derive(Debug, Default)]
struct Asks {
    waiting: AtomicUsize,
    last: AtomicU64,
}

/// Drain reads waiting on what a channel owes; its actor reads the store for them only meanwhile.
#[derive(Clone, Debug, Default)]
pub struct Demand(Arc<Asks>);

impl Demand {
    /// Counts one waiting read until the returned guard drops, numbered after every earlier read.
    pub fn want(&self) -> Wanting {
        self.0.waiting.fetch_add(1, Ordering::SeqCst);
        let asked = self.0.last.fetch_add(1, Ordering::SeqCst) + 1;
        Wanting(Arc::clone(&self.0), asked)
    }

    pub fn wanted(&self) -> bool {
        self.0.waiting.load(Ordering::SeqCst) > 0
    }

    /// The number of the last read that asked so far.
    pub fn asked(&self) -> u64 {
        self.0.last.load(Ordering::SeqCst)
    }
}

pub struct Wanting(Arc<Asks>, u64);

impl Wanting {
    /// This read's number; an answer stamped lower was computed before it asked.
    pub fn asked(&self) -> u64 {
        self.1
    }
}

impl Drop for Wanting {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

/// After each poll in which a read waits on `demand`, a Herdr-configured channel's actor publishes
/// what it still owes; `None` while its store or a source cannot be read.
pub struct Owing {
    pub published: watch::Sender<Option<Undelivered>>,
    pub demand: Demand,
}

impl Default for Owing {
    fn default() -> Self {
        let published = watch::channel(None).0;
        let demand = Demand::default();
        Self { published, demand }
    }
}

/// [`spawn_if_enabled`] that also publishes the channel's [`Unsettled`] count and [`Owing`].
pub fn spawn_projecting<P, L, A, B>(
    config: &WriterConfig,
    writer: ChannelWriter<P, L, A>,
    provider: ShadowProvider,
    bindings: Arc<B>,
    (stop, resumed, unsettled, owing): (
        watch::Receiver<bool>,
        watch::Sender<bool>,
        Unsettled,
        Owing,
    ),
) -> Option<JoinHandle<Option<StopCause>>>
where
    P: DiscordPort,
    L: DeliveryLease + 'static,
    A: AlarmSink + 'static,
    B: BindingEvents,
{
    let run = || {
        let projections = (unsettled, owing);
        tokio::spawn(run_publishing(
            writer,
            provider,
            bindings,
            (stop, resumed),
            projections,
            None,
        ))
    };
    config.enabled.then(run)
}

struct Actor<P, L, A, B> {
    writer: ChannelWriter<P, L, A>,
    deriver: UnitDeriver,
    owed: VecDeque<Derived>,
    sources: Sources<B>,
    bindings: Arc<B>,
    /// The binding log's latest seq, watched once the channel first publishes what it owes.
    notice: Option<watch::Receiver<u64>>,
    waiting: Waiting,
    clock: (tokio::time::Instant, chrono::DateTime<chrono::Utc>),
}

#[derive(Default)]
struct Waiting {
    ready: HashMap<(UnitKey, u32), tokio::time::Instant>,
    incident: Option<String>,
}

/// Returns when the channel stops, with why, or when `stop` turns true or closes. `resumed` turns
/// true once the spool and sources are recovered, and closes when the actor returns.
pub async fn run_channel<P, L, A, B>(
    writer: ChannelWriter<P, L, A>,
    provider: ShadowProvider,
    bindings: Arc<B>,
    stop: watch::Receiver<bool>,
    resumed: watch::Sender<bool>,
) -> Option<StopCause>
where
    P: DiscordPort,
    L: DeliveryLease,
    A: AlarmSink,
    B: BindingEvents,
{
    let unsettled = watch::channel(None).0;
    run_projecting(writer, provider, bindings, stop, resumed, unsettled).await
}

/// [`run_channel`] publishing its [`Unsettled`] count once each poll's sources are tended.
pub async fn run_projecting<P, L, A, B>(
    writer: ChannelWriter<P, L, A>,
    provider: ShadowProvider,
    bindings: Arc<B>,
    stop: watch::Receiver<bool>,
    resumed: watch::Sender<bool>,
    unsettled: Unsettled,
) -> Option<StopCause>
where
    P: DiscordPort,
    L: DeliveryLease,
    A: AlarmSink,
    B: BindingEvents,
{
    let projections = (unsettled, Owing::default());
    run_publishing(
        writer,
        provider,
        bindings,
        (stop, resumed),
        projections,
        None,
    )
    .await
}

/// [`run_projecting`] that also publishes what the channel still owes, beside its count.
async fn run_publishing<P, L, A, B>(
    writer: ChannelWriter<P, L, A>,
    provider: ShadowProvider,
    bindings: Arc<B>,
    (mut stop, resumed): (watch::Receiver<bool>, watch::Sender<bool>),
    (unsettled, owing): (Unsettled, Owing),
    utc: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<StopCause>
where
    P: DiscordPort,
    L: DeliveryLease,
    A: AlarmSink,
    B: BindingEvents,
{
    let deriver = UnitDeriver::new(writer.channel(), provider);
    let sources = Sources::new(writer.channel(), provider, Arc::clone(&bindings));
    let owed = VecDeque::new();
    let mut actor = Actor {
        writer,
        deriver,
        owed,
        sources,
        bindings,
        notice: None,
        waiting: Waiting::default(),
        clock: (
            tokio::time::Instant::now(),
            utc.unwrap_or_else(chrono::Utc::now),
        ),
    };
    let mut sources_resumed = false;
    while !actor.writer.is_stopped() && !*stop.borrow() {
        if !sources_resumed && actor.delivery_allowed() {
            let (writer, deriver, owed) = (&mut actor.writer, &mut actor.deriver, &mut actor.owed);
            if let Err(alarm) = actor.sources.resume(writer, deriver, owed) {
                actor.writer.stop(alarm);
            }
            sources_resumed = true;
            if !actor.writer.is_stopped() {
                resumed.send_replace(true);
            }
        }
        actor.observe_waiting(false);
        actor.deliver_owed().await;
        actor.collect_settled();
        actor.read_sources();
        actor.observe_waiting(sources_resumed);
        // A writer that stopped in this poll ends now, so its readiness drops before the next poll.
        if actor.writer.is_stopped() {
            break;
        }
        // Only a Herdr-configured channel's clear reads it, so other channels skip the store read.
        if crate::config::session_hosts::herdr_endpoint(actor.writer.channel()).is_some() {
            unsettled.send_replace(actor.unsettled());
            // Read only while a drain waits on it, so a channel nobody drains adds no reads.
            let asked = owing.demand.asked();
            if owing.demand.wanted() {
                owing.published.send_replace(actor.undelivered(asked));
            }
        }
        tokio::select! {
            () = tokio::time::sleep(POLL_INTERVAL) => {}
            changed = stop.changed() => if changed.is_err() { break },
        }
    }
    actor.writer.stop_cause().cloned()
}

#[cfg(test)]
pub(crate) async fn run_channel_at<P, L, A, B>(
    writer: ChannelWriter<P, L, A>,
    provider: ShadowProvider,
    bindings: Arc<B>,
    stop: watch::Receiver<bool>,
    utc: chrono::DateTime<chrono::Utc>,
) -> Option<StopCause>
where
    P: DiscordPort,
    L: DeliveryLease,
    A: AlarmSink,
    B: BindingEvents,
{
    run_publishing(
        writer,
        provider,
        bindings,
        (stop, watch::channel(false).0),
        (watch::channel(None).0, Owing::default()),
        Some(utc),
    )
    .await
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents> Actor<P, L, A, B> {
    fn observe_waiting(&mut self, restored: bool) {
        if self.writer.is_stopped() {
            return;
        }
        let now = tokio::time::Instant::now();
        let elapsed =
            chrono::Duration::from_std(now - self.clock.0).unwrap_or(chrono::Duration::MAX);
        let utc = self
            .clock
            .1
            .checked_add_signed(elapsed)
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC);
        let open = self
            .writer
            .store()
            .ledger()
            .unresolved()
            .map(|(serial, piece)| {
                (
                    serial,
                    piece.unit_key.clone(),
                    piece.piece_index,
                    piece.prepared_at,
                )
            });
        let keys: HashSet<_> = self
            .owed
            .iter()
            .filter_map(|item| match item {
                Derived::Piece(piece)
                    if !open.as_ref().is_some_and(|(_, key, index, _)| {
                        key == &piece.unit_key && *index == piece.index
                    }) =>
                {
                    Some((piece.unit_key.clone(), piece.index))
                }
                _ => None,
            })
            .collect();
        self.waiting.ready.retain(|key, _| keys.contains(key));
        for key in keys {
            self.waiting.ready.entry(key).or_insert(now);
        }
        let ready = self
            .waiting
            .ready
            .values()
            .filter(|first| now - **first > WAIT_LIMIT)
            .count();
        // UTC restores a Prepared wait's initial age; monotonic elapsed advances that age thereafter.
        let prepared = open.as_ref().filter(|(_, _, _, at)| {
            utc.signed_duration_since(*at) > chrono::Duration::seconds(WAIT_LIMIT.as_secs() as i64)
        });
        if ready == 0 && prepared.is_none() {
            // A fresh actor's empty queue is evidence only after its durable sources were restored.
            if self.waiting.incident.take().is_some()
                || (restored && self.waiting.ready.is_empty() && open.is_none())
            {
                self.writer.waiting_cleared();
            }
            return;
        }
        if self.waiting.incident.is_some() {
            return;
        }
        let incident = match prepared {
            Some((serial, _, _, at)) => format!("prepared:{serial}:{}", at.to_rfc3339()),
            None => format!("ready:{}", uuid::Uuid::new_v4()),
        };
        self.waiting.incident = Some(incident.clone());
        self.writer.alarm(WriterAlarm::WaitingTooLong {
            incident,
            ready,
            prepared: prepared.map(|piece| piece.0),
        });
    }

    fn delivery_allowed(&mut self) -> bool {
        let channel = self.writer.channel();
        if !crate::services::tui_prompt_dedupe::codex_verified_channel_delivery_allowed(channel) {
            return false;
        }
        // Derived items have no source field, so outstanding work keeps every retained source pinned.
        let owing = !self.owed.is_empty();
        self.writer
            .store()
            .cursors()
            .filter(|cursor| !cursor.retired || owing)
            .all(|cursor| {
                crate::services::tui_prompt_dedupe::codex_verified_o_source_allowed(
                    channel,
                    &cursor.source,
                )
            })
    }

    async fn deliver_owed(&mut self) {
        while !self.owed.is_empty() {
            if !self.delivery_allowed() {
                return;
            }
            let Some(item) = self.owed.front() else {
                return;
            };
            match self.writer.deliver(item).await {
                Step::Done => {
                    self.owed.pop_front();
                }
                Step::LeaseBusy | Step::NoGateway | Step::Stopped => return,
            }
        }
    }

    /// With nothing owed, unsealed or open, every retained segment of a source with a decided start
    /// is settled. The open segment stays unless the spool is full, so segment files do not churn.
    fn collect_settled(&mut self) {
        if !self.delivery_allowed() {
            return;
        }
        let open = self.writer.store().ledger().unresolved().is_some();
        if self.writer.is_stopped() || open || !self.owed.is_empty() || self.deriver.has_unsealed()
        {
            return;
        }
        for (source, keep) in self.sources.collectable() {
            if !crate::services::tui_prompt_dedupe::codex_verified_o_source_allowed(
                self.writer.channel(),
                &source,
            ) {
                continue;
            }
            while self.writer.store().retained_segments(&source) > keep {
                if let Err(error) = self.writer.store().gc_oldest_segment(&source) {
                    let violation = self.writer.store().ledger().violation().map(str::to_string);
                    let alarm = match violation {
                        Some(detail) => WriterAlarm::LedgerViolation { detail },
                        None => WriterAlarm::Halted {
                            detail: format!("spool gc: {error:?}"),
                        },
                    };
                    self.writer.stop(alarm);
                    return;
                }
            }
        }
    }

    /// Sources rotated away from whose cursor is not retired yet: O still reads each of them.
    fn unsettled(&mut self) -> Option<usize> {
        let store = self.writer.store();
        let rotation = store.rotation().ok()?;
        let retired = store.cursors().filter(|cursor| cursor.retired);
        let retired: HashSet<String> = retired.map(|cursor| source_key(&cursor.source)).collect();
        let old = rotation.successors.keys();
        Some(old.filter(|key| !retired.contains(*key)).count())
    }

    /// Pieces, an open `Prepared`, unsealed units, sources read short of their end and binds not
    /// applied, stamped `asked`; `None` when the store, checkpoint or a source length is unreadable.
    fn undelivered(&mut self, asked: u64) -> Option<Undelivered> {
        #[cfg(test)]
        UNDELIVERED_READS.with(|reads| reads.set(reads.get() + 1));
        let channel = self.writer.channel();
        let notice = (self.notice).get_or_insert_with(|| self.bindings.subscribe(channel));
        let latest = *notice.borrow();
        let (owed, unsealed) = (self.owed.len(), usize::from(self.deriver.has_unsealed()));
        let store = self.writer.store();
        let prepared = usize::from(store.ledger().unresolved().is_some());
        let rotation = store.rotation().ok()?;
        let checkpoint = store.binding_checkpoint().ok()?;
        let mut uncaptured = 0;
        for cursor in store.cursors().filter(|cursor| !cursor.retired) {
            let len = std::fs::metadata(&cursor.source.path).ok()?.len();
            uncaptured += usize::from(cursor.captured_through < len);
        }
        let unapplied = latest.saturating_sub(checkpoint.unwrap_or(0));
        let unapplied = usize::try_from(unapplied).unwrap_or(usize::MAX);
        let links = rotation.links.values();
        let waiting = links.filter(|link| matches!(link.boundary, Boundary::Pending { .. }));
        Some(Undelivered {
            owed,
            prepared,
            unsealed,
            uncaptured,
            binding_pending: unapplied.saturating_add(waiting.count()),
            asked,
        })
    }

    /// Applies new binds first, so a bound source is read in the same poll as its predecessor.
    fn read_sources(&mut self) {
        if self.writer.is_stopped() || !self.delivery_allowed() {
            return;
        }
        let (writer, deriver, owed) = (&mut self.writer, &mut self.deriver, &mut self.owed);
        let read = (self.sources.follow(writer))
            .and_then(|()| self.sources.capture(writer, deriver, owed))
            .and_then(|()| self.sources.tend(writer));
        if let Err(alarm) = read {
            self.writer.stop(alarm);
        }
    }
}

#[cfg(test)]
thread_local! {
    static UNDELIVERED_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many times an actor on this thread read what its channel owes.
#[cfg(test)]
pub(crate) fn undelivered_reads_for_test() -> usize {
    UNDELIVERED_READS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) use test_support::exercise_held_actor_for_tests;

#[cfg(test)]
mod test_support {
    use super::*;
    #[cfg(test)]
    pub(crate) async fn exercise_held_actor_for_tests<P, L, A, B>(
        writer: ChannelWriter<P, L, A>,
        provider: ShadowProvider,
        bindings: Arc<B>,
        hold: impl FnOnce(),
    ) where
        P: DiscordPort,
        L: DeliveryLease,
        A: AlarmSink,
        B: BindingEvents,
    {
        let channel = writer.channel();
        let mut actor = Actor {
            writer,
            deriver: UnitDeriver::new(channel, provider),
            owed: VecDeque::new(),
            sources: Sources::new(channel, provider, Arc::clone(&bindings)),
            bindings,
            notice: None,
            waiting: Waiting::default(),
            clock: (tokio::time::Instant::now(), chrono::Utc::now()),
        };
        actor
            .sources
            .resume(&mut actor.writer, &mut actor.deriver, &mut actor.owed)
            .unwrap();
        actor.read_sources();
        assert!(
            !actor.owed.is_empty(),
            "the actor must already owe captured output"
        );
        let cursor = |actor: &mut Actor<P, L, A, B>| {
            actor
                .writer
                .store()
                .cursors()
                .map(|c| (c.source.clone(), c.captured_through))
                .collect::<Vec<_>>()
        };
        let previous_cursor = cursor(&mut actor);
        let previous_checkpoint = actor.writer.store().binding_checkpoint().unwrap();
        let previous_owed = actor.owed.len();
        hold();
        actor.read_sources();
        actor.collect_settled();
        actor.deliver_owed().await;
        assert_eq!(
            cursor(&mut actor),
            previous_cursor,
            "held capture must not advance"
        );
        assert_eq!(
            actor.writer.store().binding_checkpoint().unwrap(),
            previous_checkpoint
        );
        assert_eq!(
            actor.owed.len(),
            previous_owed,
            "held delivery must retain every owed item"
        );
        assert!(
            !actor.writer.is_stopped(),
            "permission hold must not stop the actor"
        );
    }
}
