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
use super::transition_port;
use super::{AlarmSink, DeliveryLease, DiscordPort, ReaderConfig, WriterAlarm, WriterConfig};
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey};
use crate::services::tui_o::store::rotation::Boundary;
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_prompt_dedupe::codex_verified::provenance::admission;

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
        None,
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

/// Publishes [`Unsettled`] and [`Owing`]; `None` keeps legacy readers without cleanup policy.
pub fn spawn_projecting<P, L, A, B>(
    config: &WriterConfig,
    readers: Option<ReaderConfig>,
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
        let commands = codex_submission::take_spawn_receiver();
        let transitions = transition_port::take_spawn_receiver();
        let projections = (unsettled, owing);
        tokio::spawn(run_publishing(
            writer,
            provider,
            bindings,
            (stop, resumed),
            projections,
            (None, readers),
            (commands, transitions),
        ))
    };
    config.enabled.then(run)
}

struct Actor<P, L, A, B> {
    writer: ChannelWriter<P, L, A>,
    deriver: UnitDeriver,
    owed: VecDeque<Derived>,
    /// Set only where verified owed delivery is ready; every piece then needs its exact permit.
    verified: Option<super::owed::OwedLog>,
    sources: Sources<B>,
    bindings: Arc<B>,
    transitions: transition::Transitions,
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
        (None, None),
        (None, None),
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
    (utc, readers): (Option<chrono::DateTime<chrono::Utc>>, Option<ReaderConfig>),
    (mut commands, transitions): (
        Option<codex_submission::Receiver>,
        Option<transition_port::Receiver>,
    ),
) -> Option<StopCause>
where
    P: DiscordPort,
    L: DeliveryLease,
    A: AlarmSink,
    B: BindingEvents,
{
    let mut writer = writer.with_repost_stop(stop.clone());
    let deriver = UnitDeriver::new(writer.channel(), provider);
    let mut sources = Sources::new(writer.channel(), provider, Arc::clone(&bindings));
    if let Some(readers) = readers {
        sources = sources.with_readers(readers);
    }
    let owed = VecDeque::new();
    let ready = admission::owed_delivery_ready(writer.channel())
        || (commands.is_some()
            && provider == ShadowProvider::Codex
            && writer.channel() == crate::services::codex_tui::canary::CANARY_CHANNEL
            && crate::services::tui_prompt_dedupe::codex_verified_channel_requires_proof(
                writer.channel(),
            )
            && {
                let channel = writer.channel();
                let mut cursors = writer
                    .store()
                    .cursors()
                    .filter(|cursor| !cursor.retired)
                    .peekable();
                cursors.peek().is_some()
                    && cursors.all(|cursor| {
                        crate::services::tui_prompt_dedupe::codex_verified_o_source_allowed(
                            channel,
                            &cursor.source,
                        )
                    })
            });
    let verified = ready.then(|| super::owed::OwedLog::new(writer.channel(), provider));
    let mut actor = Actor {
        writer,
        deriver,
        owed,
        verified,
        sources,
        bindings,
        transitions: transition::Transitions::new(transitions, stop.clone()),
        notice: None,
        waiting: Waiting::default(),
        clock: (
            tokio::time::Instant::now(),
            utc.unwrap_or_else(chrono::Utc::now),
        ),
    };
    let mut sources_resumed = false;
    while !actor.writer.is_stopped() && !*stop.borrow() {
        if !sources_resumed && actor.sources_allowed() {
            let (writer, deriver) = (&mut actor.writer, &mut actor.deriver);
            let restored = match &mut actor.verified {
                Some(log) => actor.sources.resume(writer, deriver, log),
                None => actor.sources.resume(writer, deriver, &mut actor.owed),
            };
            if let Err(alarm) = restored {
                actor.writer.stop(alarm);
            }
            sources_resumed = true;
            if !actor.writer.is_stopped() {
                actor.project_operator_resumes();
                resumed.send_replace(true);
            }
        }
        actor.observe_waiting(false);
        actor.deliver_owed().await;
        // Re-post runs in this same turn and only once nothing is owed, so it never sends beside
        // a piece of this channel. It holds no exact permit, so a verified channel never runs it.
        if actor.verified.is_none() && actor.owed.is_empty() && actor.delivery_allowed() {
            actor.writer.repost_turn().await;
        }
        actor.collect_settled();
        actor.read_sources();
        actor.poll_transition(sources_resumed, &unsettled);
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
        if let Some(receiver) = &mut commands {
            tokio::select! {
                request = receiver.next() => {
                    if let Some(request) = request {
                        receiver.apply(&mut actor.writer, request);
                    } else { commands = None; }
                }
                () = tokio::time::sleep(POLL_INTERVAL) => {}
                changed = stop.changed() => if changed.is_err() { break },
            }
            if commands.is_some() {
                continue;
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
        (Some(utc), None),
        (None, None),
    )
    .await
}

mod transition;
mod transition_snapshot;

pub(crate) mod codex_submission {
    use super::*;
    use crate::services::tui_o::store::StoreError;
    use crate::services::tui_o::store::codex_provenance::submit_floor::{
        DurableClosedEpisode, NativeFloor, PreparedLaunch, SubmitFloor,
    };
    use crate::services::tui_prompt_dedupe::codex_verified::provenance::{
        CodexEpisodeSpan, ExecutionProofRef,
    };
    use tokio::sync::{mpsc, oneshot};

    #[derive(Clone, Debug)]
    pub(crate) struct Port {
        generation: u64,
        run: Arc<()>,
        send: mpsc::Sender<Request>,
    }

    pub(crate) struct Receiver {
        generation: u64,
        run: Arc<()>,
        receive: mpsc::Receiver<Request>,
    }

    pub(crate) enum Operation {
        #[cfg(test)]
        Prepare(SubmitFloor),
        Capture(Box<(PreparedLaunch, ExecutionProofRef)>),
        Close(Box<CodexEpisodeSpan>),
    }

    pub(crate) struct Request {
        run: Arc<()>,
        operation: Operation,
        reply: oneshot::Sender<Result<Reply, StoreError>>,
    }

    #[derive(Debug)]
    pub(crate) struct Reply {
        generation: u64,
        run: Arc<()>,
        closed: Option<Closed>,
        prepared: Option<Prepared>,
    }

    #[derive(Debug)]
    pub(crate) struct Prepared {
        run: Arc<()>,
        floor: NativeFloor,
    }

    impl Prepared {
        pub(crate) fn floor(&self) -> &NativeFloor {
            &self.floor
        }
    }

    #[derive(Debug)]
    pub(crate) struct Closed {
        run: Arc<()>,
        durable: DurableClosedEpisode,
    }

    impl Closed {
        pub(crate) fn span(&self) -> &CodexEpisodeSpan {
            self.durable.span()
        }
    }

    impl Port {
        pub(crate) fn new(generation: u64) -> (Self, Receiver) {
            let run = Arc::new(());
            let (send, receive) = mpsc::channel(4);
            let receiver = Receiver {
                generation,
                run: Arc::clone(&run),
                receive,
            };
            (
                Self {
                    generation,
                    run,
                    send,
                },
                receiver,
            )
        }

        #[cfg(test)]
        pub(crate) fn same_run(&self, other: &Self) -> bool {
            Arc::ptr_eq(&self.run, &other.run)
        }

        pub(crate) async fn request(&self, operation: Operation) -> Result<Reply, String> {
            let (reply, receive) = oneshot::channel();
            self.send
                .send(Request {
                    run: Arc::clone(&self.run),
                    operation,
                    reply,
                })
                .await
                .map_err(|_| "actor ended".to_owned())?;
            receive
                .await
                .map_err(|_| "actor reply unavailable".to_owned())?
                .map_err(|error| format!("{error:?}"))
        }

        pub(crate) fn take_closed(&self, reply: Reply) -> Result<Closed, &'static str> {
            if self.generation != reply.generation || !Arc::ptr_eq(&self.run, &reply.run) {
                return Err("stale_actor_reply");
            }
            let closed = reply.closed.ok_or("episode_not_closed")?;
            if !Arc::ptr_eq(&self.run, &closed.run) {
                return Err("stale_actor_reply");
            }
            Ok(closed)
        }

        pub(crate) fn accepts_prepared(&self, prepared: &Prepared) -> bool {
            Arc::ptr_eq(&self.run, &prepared.run)
        }

        pub(crate) fn take_prepared(&self, reply: Reply) -> Result<Prepared, &'static str> {
            if self.generation != reply.generation || !Arc::ptr_eq(&self.run, &reply.run) {
                return Err("stale_actor_reply");
            }
            let prepared = reply.prepared.ok_or("episode_not_prepared")?;
            if !Arc::ptr_eq(&self.run, &prepared.run) {
                return Err("stale_actor_reply");
            }
            Ok(prepared)
        }
    }

    impl Receiver {
        pub(super) async fn next(&mut self) -> Option<Request> {
            self.receive.recv().await
        }

        pub(super) fn apply<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
            &self,
            writer: &mut ChannelWriter<P, L, A>,
            request: Request,
        ) {
            let mut prepared = None;
            let result = if !Arc::ptr_eq(&self.run, &request.run) {
                Err(StoreError::Rejected("foreign actor command".into()))
            } else {
                match request.operation {
                    #[cfg(test)]
                    Operation::Prepare(floor) => {
                        if let SubmitFloor::NativeFloor(floor) = &floor {
                            prepared = Some((**floor).clone());
                        }
                        writer.store().prepare_codex_floor(floor).map(|()| None)
                    }
                    Operation::Capture(intent) => {
                        let (launch, execution) = *intent;
                        let capture = || -> Result<NativeFloor, StoreError> {
                            let cursor = writer
                                .store()
                                .cursor(&execution.source)
                                .ok_or_else(|| {
                                    StoreError::Rejected("canonical cursor unavailable".into())
                                })?
                                .clone();
                            let modified = std::fs::metadata(&execution.source.path)?.modified()?;
                            let generation = modified
                                .duration_since(std::time::UNIX_EPOCH)
                                .ok()
                                .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
                                .ok_or_else(|| {
                                    StoreError::Rejected("native generation unavailable".into())
                                })?;
                            NativeFloor::capture(launch, execution, &cursor, generation)
                                .map_err(|reason| StoreError::Rejected(reason.into()))
                        };
                        capture().and_then(|floor| {
                            writer
                                .store()
                                .prepare_codex_floor(SubmitFloor::NativeFloor(Box::new(
                                    floor.clone(),
                                )))?;
                            prepared = Some(floor);
                            Ok(None)
                        })
                    }
                    Operation::Close(span) => writer.store().close_codex_episode(*span).map(Some),
                }
            };
            if writer.store().io_failure().is_some() {
                writer.stop(WriterAlarm::Halted {
                    detail: "Codex provenance publication failed".into(),
                });
            }
            let closed = result.map(|closed| {
                closed.map(|durable| Closed {
                    run: Arc::clone(&self.run),
                    durable,
                })
            });
            let reply = closed.map(|closed| Reply {
                generation: self.generation,
                run: Arc::clone(&self.run),
                closed,
                prepared: prepared.map(|floor| Prepared {
                    run: Arc::clone(&self.run),
                    floor,
                }),
            });
            let _ = request.reply.send(reply);
        }
    }

    impl crate::services::tui_o::writer::host::Readiness {
        pub(crate) fn prepared_reply(
            &self,
            channel: u64,
            reply: Reply,
        ) -> Result<Prepared, &'static str> {
            self.with_submission(channel, |port| port.take_prepared(reply))
        }

        /// Rechecks this run under Live through one synchronous source-guarded effect.
        pub(crate) fn with_current_prepared<T>(
            &self,
            channel: u64,
            prepared: &Prepared,
            effect: impl FnOnce() -> T,
        ) -> Result<T, &'static str> {
            self.with_submission(channel, |port| {
                if !port.accepts_prepared(prepared) {
                    return Err("stale_actor_reply");
                }
                Ok(effect())
            })
        }

        /// Consumers must be bounded and synchronous, without re-entering Readiness.
        #[cfg(test)]
        pub(crate) fn with_prepared_reply<T>(
            &self,
            channel: u64,
            reply: Reply,
            consume: impl FnOnce(&Prepared) -> T,
        ) -> Result<T, &'static str> {
            self.with_submission(channel, |port| Ok(consume(&port.take_prepared(reply)?)))
        }

        /// Consumers must be bounded and synchronous, without re-entering Readiness.
        pub(crate) fn with_closed_reply<T>(
            &self,
            channel: u64,
            reply: Reply,
            issue: impl FnOnce(&Closed) -> T,
        ) -> Result<T, &'static str> {
            self.with_submission(channel, |port| Ok(issue(&port.take_closed(reply)?)))
        }
    }

    thread_local! {
        static SPAWN: std::cell::RefCell<Option<Receiver>> = const { std::cell::RefCell::new(None) };
    }

    pub(crate) fn spawning<T>(receiver: Receiver, spawn: impl FnOnce() -> T) -> T {
        struct Scope;
        impl Drop for Scope {
            fn drop(&mut self) {
                SPAWN.with(|slot| slot.borrow_mut().take());
            }
        }
        SPAWN.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(receiver);
        });
        let _scope = Scope;
        spawn()
    }

    pub(super) fn take_spawn_receiver() -> Option<Receiver> {
        SPAWN.with(|slot| slot.borrow_mut().take())
    }
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents> Actor<P, L, A, B> {
    fn project_operator_resumes(&mut self) {
        let pieces: Vec<_> = self
            .writer
            .resume_pieces()
            .into_iter()
            .map(|(rejected, piece)| {
                (
                    rejected,
                    super::pieces::PieceWork {
                        unit_key: piece.unit_key.clone(),
                        index: piece.piece_index,
                        payload: piece.payload.clone(),
                    },
                )
            })
            .collect();
        for (rejected_serial, piece) in pieces.into_iter().rev() {
            self.owed.retain(|item| {
                let Derived::Piece(derived) = item else {
                    return true;
                };
                if derived.unit_key != piece.unit_key || derived.index != piece.index {
                    return true;
                }
                if derived.payload != piece.payload {
                    tracing::warn!(rejected_serial, unit_key = ?piece.unit_key,
                        "[tui_o] rederived payload differs; keeping ledger payload");
                }
                false
            });
            // Retry precedes later pieces, but never a retained schema stop.
            let at = self
                .owed
                .iter()
                .rposition(|item| matches!(item, Derived::Blocked { .. }))
                .map_or(0, |blocked| blocked + 1);
            self.owed.insert(at, Derived::Piece(piece));
        }
    }

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
        self.writer.delivery_allowed(!self.owed.is_empty())
    }

    /// Verified sources are restored apart from delivery permission, which each piece proves.
    fn sources_allowed(&mut self) -> bool {
        if self.verified.is_none() {
            return self.delivery_allowed();
        }
        let channel = self.writer.channel();
        self.writer
            .store()
            .cursors()
            .filter(|cursor| !cursor.retired)
            .all(|cursor| {
                crate::services::tui_prompt_dedupe::codex_verified_o_source_allowed(
                    channel,
                    &cursor.source,
                )
            })
    }

    async fn deliver_owed(&mut self) {
        if let Some(log) = &mut self.verified {
            // Legacy retries in `owed` have no provenance, so they stay Unknown here.
            return super::owed::deliver_owed(&mut self.writer, log).await;
        }
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
                Step::LeaseBusy | Step::NoGateway | Step::Stopped | Step::Withheld => return,
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
        let protected = self.verified.as_ref().map(super::owed::protected);
        let collectable = self.sources.collectable().into_iter();
        let mut collectable = collectable.filter(|(source, _)| {
            !protected
                .as_ref()
                .is_some_and(|keys| keys.contains(&source_key(source)))
        });
        for (source, keep) in collectable.by_ref() {
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
        if self.transitions.applying() {
            return None;
        }
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
        if self.writer.is_stopped() || !self.sources_allowed() {
            return;
        }
        let (writer, deriver) = (&mut self.writer, &mut self.deriver);
        let read = (self.sources.follow(writer))
            .and_then(|()| match &mut self.verified {
                Some(log) => self.sources.capture(writer, deriver, log),
                None => self.sources.capture(writer, deriver, &mut self.owed),
            })
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
pub(crate) use transition::test_hooks as transition_hooks;
#[cfg(test)]
pub(crate) use transition_snapshot::{
    execution_read, execution_sources, force_execution_for_tests,
};

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
            verified: None,
            sources: Sources::new(channel, provider, Arc::clone(&bindings)),
            bindings,
            transitions: transition::Transitions::new(None, watch::channel(false).1),
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
