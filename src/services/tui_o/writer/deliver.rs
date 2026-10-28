//! One channel's delivery. For each piece: take the delivery lease, then under the ownership gate
//! fsync `Prepared` and start the POST, record the result, and settle unclear ones.

use std::collections::{BTreeMap, HashMap};
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::task::Poll;
use std::time::Duration;

use futures::FutureExt;
use tokio::sync::{oneshot, watch};

use super::confirm::{self, Verdict};
use super::pieces::{Derived, PieceWork};
use super::{AlarmSink, DeliveryLease, DiscordPort, PostOutcome, SeenMessage, WriterAlarm};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::repost::admission::OriginalGate;
use crate::services::tui_o::repost::dispatch::{DispatchPermit, RepostWriter};
use crate::services::tui_o::repost::projection::ReceiptOverlay;
use crate::services::tui_o::repost::runner::{RepostHook, TurnWriter};
use crate::services::tui_o::repost::send::{DispatchReport, WireOutcome};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::store::ChannelStore;
use crate::services::tui_o::store::ledger::{
    LedgerEntry, LedgerState, LocalRetryMark, PieceDisposition, PieceOutcome, PieceRecord, Unsent,
};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::admission::EpisodePermissionRecord;
use crate::services::tui_prompt_dedupe::codex_verified::provenance::dormant::OwedOrigin;

/// A POST still unanswered by then is treated as uncertain and settled from history.
pub const POST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Posted, settled, already delivered or excluded; recheck the ledger before advancing.
    Done,
    /// Another holder has the delivery lease; retry this item later.
    LeaseBusy,
    /// The gateway is not Owned; retry this item once ownership returns.
    NoGateway,
    /// The channel stays stopped until an operator acts; its alarm is raised.
    Stopped,
    /// The exact episode permit refused the piece before its `Prepared`; it stays owed.
    Withheld,
}

/// Verified owed work's exact permit, checked under the gate before `Prepared` is appended.
pub(crate) type Admission<'a> = (&'a EpisodePermissionRecord, &'a OwedOrigin);

/// Highest message id O posted per channel in this process; Legacy keeps its live panel below it.
static LAST_POSTED: LazyLock<Mutex<HashMap<u64, u64>>> = LazyLock::new(Mutex::default);

// Tests read the position they set as a late-O-post stimulus; no production path reads it.
#[cfg(test)]
pub fn last_posted(channel: u64) -> Option<u64> {
    let posted = LAST_POSTED
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    posted.get(&channel).copied()
}

#[cfg(test)]
pub(crate) fn note_posted_for_tests(channel: u64, msg_id: u64) {
    note_posted(channel, msg_id);
}

/// What a process restart does to the in-memory position; the guard keeps other restarts out.
#[cfg(test)]
pub(crate) fn forget_posted_for_tests(channel: u64) -> std::sync::MutexGuard<'static, ()> {
    static RESTARTS: Mutex<()> = Mutex::new(());
    let restart = RESTARTS.lock().unwrap_or_else(|poison| poison.into_inner());
    LAST_POSTED
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(&channel);
    restart
}

/// A restarted process learns O's newest post from the recovered ledger, not a later post.
pub fn seed_last_posted(channel: u64, ledger: &LedgerState) {
    let posted =
        (0..ledger.next_serial()).filter_map(|serial| match ledger.piece(serial)?.outcome {
            Some(PieceOutcome::Posted(msg_id)) => Some(msg_id),
            _ => None,
        });
    if let Some(msg_id) = posted.max() {
        note_posted(channel, msg_id);
    }
}

fn note_posted(channel: u64, msg_id: u64) {
    let mut posted = LAST_POSTED
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let last = posted.entry(channel).or_default();
    *last = (*last).max(msg_id);
}

type Counts = Mutex<BTreeMap<u64, Arc<AtomicUsize>>>;

/// POSTs started per channel in this process and not ended yet. A channel's
/// writers share one count, so a POST that outlives its writer still counts. Tests keep one per thread.
#[cfg(not(test))]
static IN_FLIGHT: Counts = Mutex::new(BTreeMap::new());
#[cfg(test)]
thread_local! {
    static IN_FLIGHT: Counts = const { Mutex::new(BTreeMap::new()) };
}

fn with_in_flight<R>(use_counts: impl FnOnce(&mut BTreeMap<u64, Arc<AtomicUsize>>) -> R) -> R {
    let locked =
        |counts: &Counts| use_counts(&mut counts.lock().unwrap_or_else(PoisonError::into_inner));
    #[cfg(not(test))]
    return locked(&IN_FLIGHT);
    #[cfg(test)]
    IN_FLIGHT.with(locked)
}

/// The POSTs of `channel` started and not ended in this process; `None` until a writer of the
/// channel counted them.
pub fn posts_in_flight(channel: u64) -> Option<usize> {
    #[cfg(test)]
    if bo_mutant("none_zero") {
        return Some(0);
    }
    with_in_flight(|counts| {
        counts
            .get(&channel)
            .map(|count| count.load(Ordering::SeqCst))
    })
}

/// Counts one POST from before its request exists until the request is dropped, ended or aborted.
struct Running(Arc<AtomicUsize>);

pub(super) fn source_delivery_allowed(
    channel: u64,
    sources: impl IntoIterator<Item = SourceId>,
) -> bool {
    crate::services::tui_prompt_dedupe::codex_verified_channel_delivery_allowed(channel)
        && sources.into_iter().all(|source| {
            crate::services::tui_prompt_dedupe::codex_verified_o_source_allowed(channel, &source)
        })
}

impl Running {
    fn start(count: Arc<AtomicUsize>) -> Self {
        #[cfg(test)]
        if bo_mutant("post_start") {
            return Self(count);
        }
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        #[cfg(test)]
        if bo_mutant("post_start") {
            return;
        }
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

type Request = Pin<Box<dyn Future<Output = PostOutcome> + Send>>;

/// A POST admitted under the gate: finished on its first poll, or still running.
enum Started {
    Done(PostOutcome),
    Running(Request),
}

enum Refusal {
    /// The `Prepared` append failed, so no POST was created for it.
    Store {
        detail: String,
        unsent: Option<Box<Unsent>>,
    },
    Violation(String),
    Withheld(&'static str),
    Later,
}

/// A writer's first stop, the store I/O error behind it, and a prepared piece it never posted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StopCause {
    pub alarm: WriterAlarm,
    pub io: Option<std::io::ErrorKind>,
    pub unsent: Option<Unsent>,
}

/// Rides in the POST task so the delivery lease is released only once that task can no longer
/// send; it goes back to the writer if the writer is still waiting.
struct LeaseReturn<H> {
    held: Option<H>,
    back: Option<oneshot::Sender<H>>,
}

impl<H> Drop for LeaseReturn<H> {
    fn drop(&mut self) {
        if let (Some(held), Some(back)) = (self.held.take(), self.back.take()) {
            let _ = back.send(held);
        }
    }
}

pub struct ChannelWriter<P, L, A> {
    pub(super) boot: super::boot::Trace,
    pub(super) suspension: super::suspension::Suspension,
    channel: u64,
    store: ChannelStore,
    gate: Arc<OwnershipGate>,
    port: Arc<P>,
    lease: L,
    alarms: A,
    stopped: Option<StopCause>,
    paused: bool,
    /// The channel's shared POST count, retained across writer replacement.
    posts: Option<Arc<AtomicUsize>>,
    /// The channel's re-post turn, run from the same actor turn as its posts.
    repost: Option<Box<dyn RepostHook<Self>>>,
    repost_live: RepostLive,
    repost_stops: Vec<watch::Receiver<bool>>,
    /// Shared receipts read at start; a receipted rejection or approval reads as settled.
    receipts: ReceiptOverlay,
}

/// Whether a re-post request may still leave: re-post on and the writer neither stopped nor gone.
#[derive(Default)]
struct RepostLive(Arc<AtomicBool>);

impl Drop for RepostLive {
    /// A request still running after its writer was dropped never sends again.
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> ChannelWriter<P, L, A> {
    /// A violation or an unapproved latest refusal stops the recovered channel before any POST.
    pub fn new(
        store: ChannelStore,
        gate: Arc<OwnershipGate>,
        port: Arc<P>,
        lease: L,
        alarms: A,
    ) -> Self {
        Self::with_receipts(store, gate, port, lease, alarms, ReceiptOverlay::Disabled)
    }

    /// As [`Self::new`], judging the recovered refusals with the shared receipts read before start.
    pub(crate) fn with_receipts(
        store: ChannelStore,
        gate: Arc<OwnershipGate>,
        port: Arc<P>,
        lease: L,
        alarms: A,
        receipts: ReceiptOverlay,
    ) -> Self {
        let channel = store.init().channel;
        let (stopped, paused) = (None, false);
        let shared = |counts: &mut BTreeMap<_, Arc<AtomicUsize>>| {
            Arc::clone(counts.entry(channel).or_default())
        };
        let posts = Some(with_in_flight(shared));
        #[cfg(test)]
        let posts = if bo_mutant("counter_herdr_only")
            && crate::config::session_hosts::herdr_endpoint(channel).is_none()
        {
            None
        } else if bo_mutant("counter_reset") {
            let fresh = Arc::new(AtomicUsize::new(0));
            with_in_flight(|counts| {
                counts.insert(channel, Arc::clone(&fresh));
            });
            Some(fresh)
        } else {
            posts
        };
        let mut writer = Self {
            boot: super::boot::Trace::new(channel, None, 0, 0),
            suspension: super::suspension::Suspension::default(),
            channel,
            store,
            gate,
            port,
            lease,
            alarms,
            stopped,
            paused,
            posts,
            repost: None,
            repost_live: RepostLive::default(),
            repost_stops: Vec::new(),
            receipts,
        };
        writer.exclude_stored_tool_results();
        let ledger = writer.store.ledger();
        seed_last_posted(channel, ledger);
        let refused = writer.receipts.blocked(ledger);
        if let Some(detail) = ledger.violation().map(str::to_string) {
            writer.stop(WriterAlarm::LedgerViolation { detail });
        } else if let Some((serial, status)) = refused {
            tracing::warn!(
                channel,
                serial,
                status,
                "[tui_o] output blocked; fix the refusal and use /clear for a fresh session"
            );
            writer.stop(WriterAlarm::Blocked { status });
        }
        writer
    }

    /// As [`Self::with_receipts`] over the receipts `repost` reads first, then gives it the turn.
    pub(crate) async fn hosted(
        store: ChannelStore,
        gate: Arc<OwnershipGate>,
        port: Arc<P>,
        lease: L,
        alarms: A,
        mut repost: Option<Box<dyn RepostHook<Self>>>,
    ) -> Self {
        let receipts = match repost.as_mut() {
            Some(turn) => turn.receipts().await,
            None => ReceiptOverlay::Disabled,
        };
        Self::with_receipts(store, gate, port, lease, alarms, receipts).with_repost(repost)
    }

    /// Gives the writer its re-post turn, if its host has one.
    pub(crate) fn with_repost(mut self, hook: Option<Box<dyn RepostHook<Self>>>) -> Self {
        self.repost = hook;
        self
    }

    pub(crate) fn with_repost_stop(mut self, stop: watch::Receiver<bool>) -> Self {
        self.repost_stops.push(stop);
        self
    }

    pub(super) fn stopping(&self) -> bool {
        self.repost_stops.iter().any(|stop| *stop.borrow())
    }

    /// Outstanding ordinary pieces pin retired cursors; re-post turns have nothing owed.
    pub(crate) fn delivery_allowed(&self, owing: bool) -> bool {
        source_delivery_allowed(
            self.channel,
            self.store
                .cursors()
                .filter(|cursor| {
                    (!cursor.retired || owing) && !self.suspension.contains(&cursor.source)
                })
                .map(|cursor| cursor.source.clone()),
        )
    }

    #[cfg(test)]
    pub(super) fn repost_posts_counter(&mut self) -> Arc<AtomicUsize> {
        let channel = self.channel;
        self.posts
            .get_or_insert_with(|| {
                with_in_flight(|counts| Arc::clone(counts.entry(channel).or_default()))
            })
            .clone()
    }

    /// One re-post turn, after this poll's owed pieces; nothing without a turn or once stopped.
    pub(crate) async fn repost_turn(&mut self) {
        let allowed = self.suspension.is_empty();
        #[cfg(test)]
        let allowed = allowed
            || crate::services::claude_tui::source_verify::n2b_mutant("b1b-repost")
            || crate::services::claude_tui::source_verify::n2b_mutant("b1b-live-s");
        if !allowed {
            return;
        }
        if self.is_stopped() {
            return;
        }
        let Some(mut hook) = self.repost.take() else {
            return;
        };
        hook.turn(self).await;
        self.repost = Some(hook);
    }

    pub fn store(&mut self) -> &mut ChannelStore {
        &mut self.store
    }

    /// The ledger's approved retries less those a shared receipt already settled.
    pub(super) fn resume_pieces(&self) -> Vec<(u64, PieceRecord)> {
        let ledger = self.store.ledger();
        let open = |piece: &PieceRecord| {
            let at = (&piece.unit_key, piece.piece_index);
            self.receipts.disposition(ledger, at.0, at.1) != PieceDisposition::Settled
        };
        let pieces = ledger.resume_pieces().filter(|(_, piece)| open(piece));
        pieces
            .map(|(serial, piece)| (serial, piece.clone()))
            .collect()
    }

    pub fn channel(&self) -> u64 {
        self.channel
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.is_some()
    }

    pub fn stop_cause(&self) -> Option<&StopCause> {
        self.stopped.as_ref()
    }

    pub fn alarm(&self, alarm: WriterAlarm) {
        self.alarms.raise(self.channel, alarm);
    }

    pub fn waiting_cleared(&self) {
        self.alarms.waiting_cleared(self.channel);
    }

    pub fn reconcile_reader_count(&self, count: usize) {
        self.alarms.reconcile_reader_count(self.channel, count);
    }

    pub fn reconcile_boundary_pending(&self, count: usize) {
        self.alarms.reconcile_boundary_pending(self.channel, count);
    }

    /// Stops the channel once; only the first stop raises its alarm.
    pub fn stop(&mut self, alarm: WriterAlarm) -> Step {
        self.stop_with(alarm, None)
    }

    /// The store's I/O error is read now, as the error that caused the first stop is just behind it.
    fn stop_with(&mut self, alarm: WriterAlarm, unsent: Option<Unsent>) -> Step {
        self.repost_live.0.store(false, Ordering::SeqCst);
        if self.stopped.is_none() {
            let (kept, io) = (alarm.clone(), self.store.io_failure());
            self.stopped = Some(StopCause {
                alarm: kept,
                io,
                unsent,
            });
            self.alarms.raise(self.channel, alarm);
        }
        Step::Stopped
    }

    /// Appends one ledger entry; a store error or a violation the entry exposed stops the channel.
    pub(super) fn record(&mut self, entry: LedgerEntry) -> Result<(), Step> {
        let posted = match entry {
            LedgerEntry::Posted { serial, msg_id } => Some((serial, msg_id)),
            _ => None,
        };
        #[cfg(test)]
        self.boot.before_record(&entry);
        if let Err(error) = self.store.append_ledger(entry) {
            return Err(self.stop(WriterAlarm::Halted {
                detail: format!("{error:?}"),
            }));
        }
        match self.store.ledger().violation().map(str::to_string) {
            Some(detail) => Err(self.stop(WriterAlarm::LedgerViolation { detail })),
            None => {
                if let Some((serial, msg_id)) = posted {
                    self.boot.posted(self.store.ledger(), serial, msg_id);
                    note_posted(self.channel, msg_id);
                }
                Ok(())
            }
        }
    }

    pub async fn deliver(&mut self, item: &Derived) -> Step {
        self.deliver_admitted(item, None).await
    }

    /// As [`Self::deliver`]; a piece with an admission posts only while its exact permit holds.
    pub(crate) async fn deliver_admitted(
        &mut self,
        item: &Derived,
        admission: Option<Admission<'_>>,
    ) -> Step {
        if self.is_stopped() {
            return Step::Stopped;
        }
        if let Err(step) = self.settle_open().await {
            return step;
        }
        if let Derived::Piece(piece) = item
            && let Some(step) = self.exclude_tool_result(&piece.unit_key)
        {
            return step;
        }
        if self.has_local_retry() {
            return Step::LeaseBusy;
        }
        match item {
            Derived::Blocked { reason } => self.stop(WriterAlarm::SchemaBlocked {
                reason: reason.clone(),
            }),
            Derived::Excluded { unit_key, .. }
                if self.store.ledger().excluded(unit_key).is_some() =>
            {
                Step::Done
            }
            Derived::Excluded { unit_key, reason } => {
                let (unit_key, reason) = (unit_key.clone(), reason.clone());
                self.record(LedgerEntry::Excluded { unit_key, reason })
                    .map_or_else(|step| step, |()| Step::Done)
            }
            Derived::Piece(piece) => self.deliver_piece(piece, admission).await,
        }
    }

    async fn deliver_piece(&mut self, piece: &PieceWork, admission: Option<Admission<'_>>) -> Step {
        let ledger = self.store.ledger();
        if ledger.excluded(&piece.unit_key).is_some() {
            return Step::Done;
        }
        let disposition = self
            .receipts
            .disposition(ledger, &piece.unit_key, piece.index);
        let (piece, approved) = match disposition {
            PieceDisposition::Blocked { serial, status } => {
                tracing::warn!(channel = self.channel, serial, status, unit_key = ?piece.unit_key,
                    "[tui_o] output blocked; fix the refusal and use /clear for a fresh session");
                return self.stop(WriterAlarm::Blocked { status });
            }
            PieceDisposition::Settled => return Step::Done,
            PieceDisposition::Open { .. } => {
                return self.stop(WriterAlarm::LedgerViolation {
                    detail: "open piece after settling".into(),
                });
            }
            PieceDisposition::Authorized { rejected_serial } => {
                let Some(original) = ledger.piece(rejected_serial) else {
                    return self.stop(WriterAlarm::LedgerViolation {
                        detail: "approved piece missing".into(),
                    });
                };
                if original.payload != piece.payload {
                    tracing::warn!(channel = self.channel, rejected_serial, unit_key = ?piece.unit_key,
                        "[tui_o] rederived payload differs; sending ledger payload");
                }
                (
                    PieceWork {
                        output_origin: ledger.output_origin(rejected_serial).cloned(),
                        unit_key: original.unit_key.clone(),
                        index: original.piece_index,
                        payload: original.payload.clone(),
                    },
                    Some(rejected_serial),
                )
            }
            PieceDisposition::Fresh => (piece.clone(), None),
        };
        let (serial, anchor_id) = (ledger.next_serial(), ledger.anchor());
        let mut retry_owner = OriginalGate::SendLocal;
        if approved.is_none()
            && let Some(mut hook) = self.repost.take()
        {
            let record = PieceRecord {
                local_retry: None,
                outcome_at: None,
                unit_key: piece.unit_key.clone(),
                piece_index: piece.index,
                payload: piece.payload.clone(),
                anchor_id,
                epoch: 0,
                prepared_at: chrono::Utc::now(),
                outcome: None,
            };
            let gate = hook.original(serial, record).await;
            self.repost = Some(hook);
            retry_owner = gate;
            match gate {
                OriginalGate::SendLocal | OriginalGate::SendPgManaged => {}
                // Re-post owns an admitted piece's sends; its original is never sent again.
                OriginalGate::Admitted => return Step::Done,
                // Unknown admission holds the piece and retries it on a later poll.
                OriginalGate::Unknown => return Step::LeaseBusy,
            }
        }
        let local_retry =
            (approved.is_none() && admission.is_none() && retry_owner == OriginalGate::SendLocal)
                .then_some(LocalRetryMark::Budget);
        self.send_piece(&piece, admission, local_retry, approved)
            .await
    }

    /// Originals and the single local retry share the lease, durable prepare and first POST poll.
    pub(super) async fn send_piece(
        &mut self,
        piece: &PieceWork,
        admission: Option<Admission<'_>>,
        local_retry: Option<LocalRetryMark>,
        approved: Option<u64>,
    ) -> Step {
        if let Some(step) = self.exclude_tool_result(&piece.unit_key) {
            return step;
        }
        let (serial, anchor_id) = (
            self.store.ledger().next_serial(),
            self.store.ledger().anchor(),
        );
        let Some(held) = self.lease.try_acquire(self.channel, serial) else {
            return Step::LeaseBusy;
        };
        let (gate, port, channel) = (Arc::clone(&self.gate), Arc::clone(&self.port), self.channel);
        let mut posts = self.posts.clone();
        let stops = self.repost_stops.clone();
        let boot = self.boot.clone();
        let store = &mut self.store;
        // The first poll of the request runs under the gate, so no request starts after a
        // transition; the rest runs outside the lock.
        let admitted = poll_fn(|cx| {
            Poll::Ready(gate.admit(|epoch| {
                if stops.iter().any(|stop| *stop.borrow())
                    || !super::local_retry::may_prepare(store.ledger(), piece, local_retry.as_ref())
                {
                    return Err(Refusal::Later);
                }
                // The exact permit is checked first and its lock held through the first poll.
                let mut start = || {
                    let prepared = || LedgerEntry::Prepared {
                        serial,
                        unit_key: piece.unit_key.clone(),
                        piece_index: piece.index,
                        payload: piece.payload.clone(),
                        anchor_id,
                        epoch,
                        local_retry: local_retry.clone(),
                    };
                    // Returning here comes before `port.post`, so this piece's POST was never created.
                    if let Some(artifact) = &piece.output_origin {
                        store
                            .append_ledger(LedgerEntry::OutputOrigin {
                                serial,
                                artifact: artifact.clone(),
                            })
                            .map_err(|e| Refusal::Violation(format!("output origin: {e:?}")))?;
                    }
                    store
                        .append_ledger(prepared())
                        .map_err(|error| Refusal::Store {
                            detail: format!("{error:?}"),
                            unsent: Unsent::new(prepared()).map(Box::new),
                        })?;
                    if let Some(detail) = store.ledger().violation() {
                        return Err(Refusal::Violation(detail.to_string()));
                    }
                    if let Some(rejected_serial) = approved {
                        tracing::info!(channel, rejected_serial, serial, unit_key = ?piece.unit_key,
                            "[tui_o] operator approval consumed by Prepared");
                    }
                    let running = posts.take().map(Running::start);
                    #[cfg(test)]
                    boot.constructing(serial, epoch);
                    let post = port.post_origin(
                        channel,
                        piece.payload.clone(),
                        piece.output_origin.clone(),
                    );
                    // `map` forwards each poll to the request; the count ends when it is ready or dropped.
                    let mut request: Request = match running {
                        Some(running) => Box::pin(post.map(move |outcome| {
                            drop(running);
                            outcome
                        })),
                        None => Box::pin(post),
                    };
                    boot.first_post(serial, epoch);
                    Ok(match request.as_mut().poll(cx) {
                        Poll::Ready(outcome) => Started::Done(outcome),
                        Poll::Pending => Started::Running(request),
                    })
                };
                match admission {
                    None => start(),
                    Some((record, origin)) => {
                        let started = record.admit(origin, start);
                        started.unwrap_or_else(|reason| Err(Refusal::Withheld(reason)))
                    }
                }
            }))
        })
        .await;
        let started = match admitted {
            None => {
                if !std::mem::replace(&mut self.paused, true) {
                    self.alarms.raise(channel, WriterAlarm::PausedNoGateway);
                }
                return Step::NoGateway;
            }
            Some(Err(Refusal::Store { detail, unsent })) => {
                return self.stop_with(WriterAlarm::Halted { detail }, unsent.map(|u| *u));
            }
            Some(Err(Refusal::Violation(detail))) => {
                return self.stop(WriterAlarm::LedgerViolation { detail });
            }
            Some(Err(Refusal::Withheld(reason))) => {
                tracing::info!(channel, reason, unit_key = ?piece.unit_key,
                    "[tui_o] verified piece withheld before Prepared");
                return Step::Withheld;
            }
            Some(Err(Refusal::Later)) => return Step::LeaseBusy,
            Some(Ok(started)) => started,
        };
        // Admitted under an owned gateway, so the pause is over for health as well.
        if std::mem::take(&mut self.paused) {
            crate::services::tui_o::alarm::gateway_resumed(channel);
        }
        let (outcome, held) = match started {
            Started::Done(outcome) => (outcome, Some(held)),
            Started::Running(request) => {
                let (back, returned) = oneshot::channel();
                let (held, back) = (Some(held), Some(back));
                let guard = LeaseReturn { held, back };
                let mut task = tokio::spawn(async move {
                    let _guard = guard;
                    request.await
                });
                let outcome = match tokio::time::timeout(POST_TIMEOUT, &mut task).await {
                    Ok(Ok(outcome)) => outcome,
                    Ok(Err(join)) => PostOutcome::Uncertain(format!("post task ended: {join}")),
                    Err(_) => {
                        // Settle only after the aborted request is gone.
                        task.abort();
                        let _ = (&mut task).await;
                        PostOutcome::Uncertain("post timed out".into())
                    }
                };
                (outcome, returned.await.ok())
            }
        };
        let step = self.record_outcome(serial, &piece.payload, outcome).await;
        drop(held);
        step
    }

    async fn record_outcome(&mut self, serial: u64, payload: &str, outcome: PostOutcome) -> Step {
        let entry = match outcome {
            PostOutcome::Created(message) if message.author_id == self.port.bot_id() => {
                if message.content != payload {
                    self.alarms
                        .raise(self.channel, WriterAlarm::ContentTransform { serial });
                }
                LedgerEntry::Posted {
                    serial,
                    msg_id: message.id,
                }
            }
            PostOutcome::Withheld(reason) => {
                if let Err(step) = self.record(LedgerEntry::Unresolved {
                    serial,
                    reason: reason.clone(),
                }) {
                    return step;
                }
                return self.stop(WriterAlarm::Unresolved { serial, reason });
            }
            PostOutcome::Refused(status) => {
                let reason = if self.store.ledger().piece(serial).is_some_and(|piece| {
                    matches!(piece.local_retry, Some(LocalRetryMark::Attempt { .. }))
                }) {
                    "local retry budget exhausted"
                } else {
                    "blocked serial; fix the refusal and use /clear for a fresh session"
                };
                tracing::warn!(
                    channel = self.channel,
                    serial,
                    status,
                    "[tui_o] POST rejected; {reason}"
                );
                if let Err(step) = self.record(LedgerEntry::Rejected { serial, status }) {
                    return step;
                }
                return self.stop(WriterAlarm::Blocked { status });
            }
            PostOutcome::Created(_) | PostOutcome::Uncertain(_) => {
                return self
                    .settle_open()
                    .await
                    .map_or_else(|step| step, |()| Step::Done);
            }
        };
        self.record(entry).map_or_else(|step| step, |()| Step::Done)
    }

    /// Settles the one `Prepared` without a result, if any, before anything else is posted.
    pub(super) async fn settle_open(&mut self) -> Result<(), Step> {
        let ledger = self.store.ledger();
        let Some((serial, open)) = ledger.unresolved() else {
            return Ok(());
        };
        let (payload, anchor) = (open.payload.clone(), ledger.anchor());
        let earlier_same_payload =
            (0..serial)
                .filter_map(|earlier| ledger.piece(earlier))
                .any(|piece| {
                    let unsettled = matches!(
                        piece.outcome,
                        Some(
                            PieceOutcome::NotFound
                                | PieceOutcome::Ambiguous(_)
                                | PieceOutcome::Unresolved(_)
                        )
                    );
                    unsettled && piece.payload == payload
                });
        // This bot's latest posts here, newest first; one read back by id proves history readable.
        let posted = (0..serial)
            .rev()
            .filter_map(|earlier| match ledger.piece(earlier)?.outcome {
                Some(PieceOutcome::Posted(msg_id)) => Some(msg_id),
                _ => None,
            });
        let proof_ids: Vec<u64> = posted.take(confirm::PROOF_IDS).collect();
        let verdict = confirm::settle(
            &*self.port,
            self.channel,
            anchor,
            &payload,
            earlier_same_payload,
            &proof_ids,
        )
        .await;
        let (entry, alarm) = match verdict {
            Verdict::Posted(msg_id) => (LedgerEntry::Posted { serial, msg_id }, None),
            Verdict::Ambiguous(candidates) => (
                LedgerEntry::Ambiguous { serial, candidates },
                Some(WriterAlarm::Ambiguous { serial }),
            ),
            Verdict::NotFound => (
                LedgerEntry::NotFound { serial },
                Some(WriterAlarm::NotFound { serial }),
            ),
            Verdict::Unresolved(reason) => {
                let alarm = WriterAlarm::Unresolved {
                    serial,
                    reason: reason.clone(),
                };
                (LedgerEntry::Unresolved { serial, reason }, Some(alarm))
            }
        };
        self.record(entry)?;
        if let Some(alarm) = alarm {
            self.alarms.raise(self.channel, alarm);
        }
        Ok(())
    }
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> RepostWriter for ChannelWriter<P, L, A> {
    type Lease = L::Held;

    async fn settle_open(&mut self) -> bool {
        ChannelWriter::settle_open(self).await.is_ok() && self.store.ledger().unresolved().is_none()
    }

    /// Rechecks the actor's channel and retained-source policy at the final grant boundary.
    fn guard(&self) -> Result<(), String> {
        if let Some(cause) = &self.stopped {
            return Err(format!("writer stopped: {:?}", cause.alarm));
        }
        if !self.repost_live.0.load(Ordering::SeqCst) {
            return Err("re-post is off".into());
        }
        if !self.live()() {
            return Err("re-post authority withdrawn".into());
        }
        Ok(())
    }

    fn try_lease(&self) -> Option<L::Held> {
        let serial = self.store.ledger().next_serial();
        self.lease.try_acquire(self.channel, serial)
    }

    fn gate(&self) -> Arc<OwnershipGate> {
        Arc::clone(&self.gate)
    }

    fn live(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let live = Arc::clone(&self.repost_live.0);
        let stops = self.repost_stops.clone();
        let channel = self.channel;
        let sources: Arc<[SourceId]> = self
            .store
            .cursors()
            .filter(|cursor| !cursor.retired)
            .map(|cursor| cursor.source.clone())
            .collect();
        #[cfg(test)]
        let sources: Arc<[SourceId]> = sources
            .iter()
            .filter(|source| {
                !crate::services::claude_tui::source_verify::n2b_mutant("b1b-live-s")
                    || !self.suspension.contains(source)
            })
            .cloned()
            .collect();
        Arc::new(move || {
            live.load(Ordering::SeqCst)
                && stops.iter().all(|stop| !*stop.borrow())
                && source_delivery_allowed(channel, sources.iter().cloned())
        })
    }

    fn start_request(&self) -> Box<dyn Send> {
        Box::new(
            self.posts
                .as_ref()
                .map(|posts| Running::start(Arc::clone(posts))),
        )
    }

    fn output_origin(
        &self,
        key: &crate::services::tui_o::repost::identity::PieceKey,
    ) -> Option<crate::services::cluster::intake_router_hook::owner_record::output::Artifact> {
        let ledger = self.store.ledger();
        let (serial, _) = ledger.latest_piece(key.unit(), key.piece_index())?;
        ledger.output_origin(serial).cloned()
    }

    fn prepare(&mut self, epoch: u64, permit: &DispatchPermit) -> Result<(), String> {
        if self.exclude_tool_result(permit.key().unit()).is_some() {
            return Err(super::pieces::TOOL_RESULT_INTERNAL.into());
        }
        let ledger = self.store.ledger();
        let (serial, anchor_id) = (ledger.next_serial(), ledger.anchor());
        let origin = self.output_origin(permit.key());
        if let Some(artifact) = origin {
            self.store
                .append_ledger(LedgerEntry::OutputOrigin { serial, artifact })
                .map_err(|e| format!("output origin: {e:?}"))?;
        }
        let prepared = || LedgerEntry::Prepared {
            serial,
            unit_key: permit.key().unit().clone(),
            piece_index: permit.key().piece_index(),
            payload: permit.payload().to_owned(),
            anchor_id,
            epoch,
            local_retry: None,
        };
        // As for an original: a failed append created no request, and recovery may drop it.
        if let Err(error) = self.store.append_ledger(prepared()) {
            let detail = format!("{error:?}");
            let alarm = WriterAlarm::Halted {
                detail: detail.clone(),
            };
            self.stop_with(alarm, Unsent::new(prepared()));
            return Err(detail);
        }
        if let Some(detail) = self.store.ledger().violation().map(str::to_string) {
            self.stop(WriterAlarm::LedgerViolation {
                detail: detail.clone(),
            });
            return Err(detail);
        }
        Ok(())
    }

    fn record(&mut self, report: &DispatchReport) -> impl Future<Output = ()> + Send {
        let outcome = match &report.outcome {
            WireOutcome::Created(created) => PostOutcome::Created(SeenMessage {
                id: created.id,
                author_id: created.author_id,
                content: created.content.clone(),
            }),
            WireOutcome::Refused(status) => PostOutcome::Refused(*status),
            WireOutcome::Withheld(reason) => PostOutcome::Withheld(reason.clone()),
            other => PostOutcome::Uncertain(format!("{other:?}")),
        };
        async move {
            // The re-post's Prepared is the open one; it settles like any other piece.
            let Some((serial, open)) = self.store.ledger().unresolved() else {
                return;
            };
            #[cfg(test)]
            if super::super::ownership::execution::tests::mutant("repost-record")
                && matches!(report.outcome, WireOutcome::Withheld(_))
            {
                let _ = self.record(LedgerEntry::NotSent { serial });
                return;
            }
            if matches!(
                report.outcome,
                WireOutcome::Unsent(_) | WireOutcome::Throttled
            ) && report.counted == 0
                && report.wire == report.throttled
            {
                let _ = self.record(LedgerEntry::NotSent { serial });
                return;
            }
            let payload = open.payload.clone();
            self.record_outcome(serial, &payload, outcome).await;
        }
    }
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> TurnWriter for ChannelWriter<P, L, A> {
    fn channel(&self) -> u64 {
        self.channel
    }

    fn ledger(&mut self) -> &LedgerState {
        self.store.ledger()
    }

    fn set_live(&self, live: bool) {
        let live = live && self.stopped.is_none();
        self.repost_live.0.store(live, Ordering::SeqCst);
    }
}

#[cfg(test)]
pub(super) fn bo_mutant(name: &str) -> bool {
    std::env::var("AGENTDESK_BO_MUTANT").is_ok_and(|value| value == name)
}

#[cfg(test)]
pub(super) fn forget_posts_for_test(channel: u64) {
    with_in_flight(|counts| {
        counts.remove(&channel);
    });
}
