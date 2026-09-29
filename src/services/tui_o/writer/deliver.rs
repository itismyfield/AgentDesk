//! One channel's delivery. For each piece: take the delivery lease, then under the ownership gate
//! fsync `Prepared` and start the POST, record the result, and settle unclear ones.

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use tokio::sync::oneshot;

use super::confirm::{self, Verdict};
use super::pieces::{Derived, PieceWork};
use super::{AlarmSink, DeliveryLease, DiscordPort, PostOutcome, WriterAlarm};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::store::ChannelStore;
use crate::services::tui_o::store::ledger::{LedgerEntry, PieceOutcome};

/// A POST still unanswered by then is treated as uncertain and settled from history.
pub const POST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Posted, settled, already delivered or excluded; go on to the next item.
    Done,
    /// Another holder has the delivery lease; retry this item later.
    LeaseBusy,
    /// The gateway is not Owned; retry this item once ownership returns.
    NoGateway,
    /// The channel stays stopped until an operator acts; its alarm is raised.
    Stopped,
}

type Request = Pin<Box<dyn Future<Output = PostOutcome> + Send>>;

/// A POST admitted under the gate: finished on its first poll, or still running.
enum Started {
    Done(PostOutcome),
    Running(Request),
}

enum Refusal {
    Store(String),
    Violation(String),
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
    channel: u64,
    store: ChannelStore,
    gate: Arc<OwnershipGate>,
    port: Arc<P>,
    lease: L,
    alarms: A,
    stopped: bool,
    paused: bool,
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> ChannelWriter<P, L, A> {
    /// A recovered ledger with a violation or a refused POST stops the channel before any POST.
    pub fn new(
        store: ChannelStore,
        gate: Arc<OwnershipGate>,
        port: Arc<P>,
        lease: L,
        alarms: A,
    ) -> Self {
        let channel = store.init().channel;
        let (stopped, paused) = (false, false);
        let mut writer = Self {
            channel,
            store,
            gate,
            port,
            lease,
            alarms,
            stopped,
            paused,
        };
        let ledger = writer.store.ledger();
        let refused =
            (0..ledger.next_serial()).find_map(|serial| match ledger.piece(serial)?.outcome {
                Some(PieceOutcome::Rejected(status)) => Some(status),
                _ => None,
            });
        if let Some(detail) = ledger.violation().map(str::to_string) {
            writer.stop(WriterAlarm::LedgerViolation { detail });
        } else if let Some(status) = refused {
            writer.stop(WriterAlarm::Blocked { status });
        }
        writer
    }

    pub fn store(&mut self) -> &mut ChannelStore {
        &mut self.store
    }

    pub fn channel(&self) -> u64 {
        self.channel
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    pub fn alarm(&self, alarm: WriterAlarm) {
        self.alarms.raise(self.channel, alarm);
    }

    /// Stops the channel once; only the first stop raises its alarm.
    pub fn stop(&mut self, alarm: WriterAlarm) -> Step {
        if !self.stopped {
            self.stopped = true;
            self.alarms.raise(self.channel, alarm);
        }
        Step::Stopped
    }

    /// Appends one ledger entry; a store error or a violation the entry exposed stops the channel.
    fn record(&mut self, entry: LedgerEntry) -> Result<(), Step> {
        if let Err(error) = self.store.append_ledger(entry) {
            return Err(self.stop(WriterAlarm::Halted {
                detail: format!("{error:?}"),
            }));
        }
        match self.store.ledger().violation().map(str::to_string) {
            Some(detail) => Err(self.stop(WriterAlarm::LedgerViolation { detail })),
            None => Ok(()),
        }
    }

    pub async fn deliver(&mut self, item: &Derived) -> Step {
        if self.stopped {
            return Step::Stopped;
        }
        if let Err(step) = self.settle_open().await {
            return step;
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
            Derived::Piece(piece) => self.deliver_piece(piece).await,
        }
    }

    async fn deliver_piece(&mut self, piece: &PieceWork) -> Step {
        let ledger = self.store.ledger();
        if ledger.excluded(&piece.unit_key).is_some() {
            return Step::Done;
        }
        let earlier = ledger.latest_piece(&piece.unit_key, piece.index);
        if let Some(outcome) = earlier.map(|(_, earlier)| earlier.outcome.clone()) {
            return match outcome {
                Some(PieceOutcome::Rejected(status)) => self.stop(WriterAlarm::Blocked { status }),
                Some(_) => Step::Done,
                None => self.stop(WriterAlarm::LedgerViolation {
                    detail: "open piece after settling".into(),
                }),
            };
        }
        let (serial, anchor_id) = (ledger.next_serial(), ledger.anchor());
        let Some(held) = self.lease.try_acquire(self.channel, serial) else {
            return Step::LeaseBusy;
        };
        let (gate, port, channel) = (Arc::clone(&self.gate), Arc::clone(&self.port), self.channel);
        let store = &mut self.store;
        // The first poll of the request runs under the gate, so no request starts after a
        // transition; the rest runs outside the lock.
        let admitted = poll_fn(|cx| {
            Poll::Ready(gate.admit(|epoch| {
                let prepared = LedgerEntry::Prepared {
                    serial,
                    unit_key: piece.unit_key.clone(),
                    piece_index: piece.index,
                    payload: piece.payload.clone(),
                    anchor_id,
                    epoch,
                };
                store
                    .append_ledger(prepared)
                    .map_err(|error| Refusal::Store(format!("{error:?}")))?;
                if let Some(detail) = store.ledger().violation() {
                    return Err(Refusal::Violation(detail.to_string()));
                }
                let mut request: Request = Box::pin(port.post(channel, piece.payload.clone()));
                Ok(match request.as_mut().poll(cx) {
                    Poll::Ready(outcome) => Started::Done(outcome),
                    Poll::Pending => Started::Running(request),
                })
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
            Some(Err(Refusal::Store(detail))) => return self.stop(WriterAlarm::Halted { detail }),
            Some(Err(Refusal::Violation(detail))) => {
                return self.stop(WriterAlarm::LedgerViolation { detail });
            }
            Some(Ok(started)) => started,
        };
        self.paused = false;
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
            PostOutcome::Refused(status) => {
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
    async fn settle_open(&mut self) -> Result<(), Step> {
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
        let verdict = confirm::settle(
            &*self.port,
            self.channel,
            anchor,
            &payload,
            earlier_same_payload,
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
