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
    /// Derived items and pending local retries not delivered yet.
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
    scope_hold: Option<&'static str>,
    boot_previous_nonce: Option<String>,
}

#[derive(Default)]
struct Waiting {
    ready: HashMap<(UnitKey, u32), tokio::time::Instant>,
    incident: Option<String>,
}

/// Runs until stopped; `resumed` means every non-suspended source was restored.
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
    let deriver =
        fresh_resume::configured_deriver(writer.channel(), provider, writer.store().ledger());
    let mut sources = Sources::new(writer.channel(), provider, Arc::clone(&bindings))
        .with_suspension(writer.suspension.clone());
    if let Some(readers) = readers {
        sources = sources.with_readers(readers);
    }
    let owed = VecDeque::new();
    let ready = admission::owed_delivery_ready(writer.channel());
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
        scope_hold: None,
        boot_previous_nonce: None,
        clock: (
            tokio::time::Instant::now(),
            utc.unwrap_or_else(chrono::Utc::now),
        ),
    };
    let selection = actor.writer.suspension.clone();
    let restore = provider == ShadowProvider::Codex;
    #[cfg(test)]
    let restore =
        restore && !crate::services::claude_tui::source_verify::n2b_mutant("b1b-selection-restore");
    if restore {
        match selection.restore_current(actor.writer.store()) {
            Ok(restored) => {
                if !restored
                    && crate::services::tui_prompt_dedupe::clear_relaunch::fresh_scope_eligible(
                        actor.writer.channel(),
                    )
                {
                    actor.boot_previous_nonce = actor
                        .writer
                        .store()
                        .fresh_selection()
                        .ok()
                        .flatten()
                        .map(|record| record.nonce);
                }
            }
            Err(error) => {
                actor.writer.stop(WriterAlarm::Halted {
                    detail: format!("fresh selection restore: {error:?}"),
                });
                return actor.writer.stop_cause().cloned();
            }
        }
    }
    let mut sources_resumed = false;
    while !actor.writer.is_stopped() && !*stop.borrow() {
        if actor.rescope() {
            sources_resumed = false;
            resumed.send_replace(false);
        }
        if actor.writer.is_stopped() {
            break;
        }
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
                actor.boot_previous_nonce = None;
                actor.project_operator_resumes();
                resumed.send_replace(true);
            }
        }
        actor.observe_waiting(false);
        let now = actor.utc_now();
        if actor.delivery_allowed() {
            actor.writer.retry_local(now).await;
        }
        if !actor.writer.has_local_retry() {
            actor.deliver_owed().await;
        }
        // Re-post runs in this same turn and only once nothing is owed, so it never sends beside
        // a piece of this channel. It holds no exact permit, so a verified channel never runs it.
        if actor.verified.is_none()
            && actor.owed.is_empty()
            && !actor.writer.has_local_retry()
            && actor.delivery_allowed()
        {
            actor.writer.repost_turn().await;
        }
        actor.collect_settled();
        actor.read_sources();
        actor.poll_transition(sources_resumed, &unsettled).await;
        actor.observe_waiting(sources_resumed);
        #[cfg(test)]
        fresh_resume::note_work(actor.owed.len(), actor.deriver.has_unsealed());
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

mod fresh_resume;
#[cfg(test)]
pub(crate) use fresh_resume::work_for_tests;
mod automatic;
mod automatic_age;
mod codex_age;
pub(crate) mod codex_retention;
mod transition;
mod transition_snapshot;

pub(crate) mod codex_submission;

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

    fn utc_now(&self) -> chrono::DateTime<chrono::Utc> {
        let elapsed =
            chrono::Duration::from_std(self.clock.0.elapsed()).unwrap_or(chrono::Duration::MAX);
        self.clock
            .1
            .checked_add_signed(elapsed)
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
    }

    fn observe_waiting(&mut self, restored: bool) {
        let scope_waiting = self.scope_hold == Some("new_root_waiting");
        #[cfg(test)]
        let scope_waiting = scope_waiting
            && !crate::services::claude_tui::source_verify::n2b_mutant("b1b-scope-hold");
        if self.writer.is_stopped() || scope_waiting {
            return;
        }
        let now = tokio::time::Instant::now();
        let utc = self.utc_now();
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
            let incident = self.waiting.incident.take().is_some();
            #[cfg(test)]
            let incident = incident
                && (self.writer.suspension.is_empty()
                    || !crate::services::claude_tui::source_verify::n2b_mutant("b1b-own-clear"));
            if incident
                || (restored
                    && self.waiting.ready.is_empty()
                    && open.is_none()
                    && self.writer.suspension.is_empty())
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
        let owing = !self.owed.is_empty() || self.writer.has_local_retry();
        self.writer.delivery_allowed(owing)
    }

    /// Verified sources are restored apart from delivery permission, which each piece proves.
    fn sources_allowed(&mut self) -> bool {
        if self.verified.is_none() {
            return self.delivery_allowed();
        }
        let channel = self.writer.channel();
        let suspension = self.writer.suspension.clone();
        self.writer
            .store()
            .cursors()
            .filter(|cursor| !cursor.retired && !suspension.contains(&cursor.source))
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
                    if self.writer.has_local_retry() {
                        return;
                    }
                }
                Step::LeaseBusy | Step::NoGateway | Step::Stopped | Step::Withheld => return,
            }
        }
    }

    /// With nothing owed, unsealed or open, every retained segment of a source with a decided start
    /// is settled. The open segment stays unless the spool is full, so segment files do not churn.
    fn collect_settled(&mut self) {
        #[cfg(test)]
        if crate::services::claude_tui::source_verify::n2b_mutant("b1b-gc")
            && !self.writer.suspension.is_empty()
        {
            return;
        }
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
        let (owed, unsealed) = (
            self.owed.len() + usize::from(self.writer.has_local_retry()),
            usize::from(self.deriver.has_unsealed()),
        );
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
#[cfg(all(test, unix))]
pub(crate) use transition::scoped_tests::exercise_scope_waiting_job_for_tests;
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
            scope_hold: None,
            boot_previous_nonce: None,
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
        let now = actor.utc_now();
        if actor.delivery_allowed() {
            actor.writer.retry_local(now).await;
        }
        if !actor.writer.has_local_retry() {
            actor.deliver_owed().await;
        }
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

#[cfg(all(test, unix))]
#[path = "p5a2_actor_tests.rs"]
pub(crate) mod p5a2_tests;
