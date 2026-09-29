//! One channel's delivery. For each piece: take the delivery lease, then under the ownership gate
//! fsync `Prepared` and hand the POST to the client, record the result, and settle unclear ones.

use std::sync::Arc;
use std::time::Duration;

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

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    fn stop(&mut self, alarm: WriterAlarm) -> Step {
        if !self.stopped {
            self.stopped = true;
            self.alarms.raise(self.channel, alarm);
        }
        Step::Stopped
    }

    fn record(&mut self, entry: LedgerEntry) -> Result<(), Step> {
        let appended = self.store.append_ledger(entry);
        appended.map_err(|error| {
            self.stop(WriterAlarm::Halted {
                detail: format!("{error:?}"),
            })
        })
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
        let (unit_key, piece_index, payload) =
            (piece.unit_key.clone(), piece.index, piece.payload.clone());
        let admitted = gate.admit(|epoch| {
            let prepared = LedgerEntry::Prepared {
                serial,
                unit_key,
                piece_index,
                payload: payload.clone(),
                anchor_id,
                epoch,
            };
            let appended = self.store.append_ledger(prepared);
            appended.map(|()| tokio::spawn(port.post(channel, payload)))
        });
        let mut request = match admitted {
            None => {
                if !std::mem::replace(&mut self.paused, true) {
                    self.alarms.raise(channel, WriterAlarm::PausedNoGateway);
                }
                return Step::NoGateway;
            }
            Some(Err(error)) => {
                return self.stop(WriterAlarm::Halted {
                    detail: format!("{error:?}"),
                });
            }
            Some(Ok(request)) => request,
        };
        self.paused = false;
        let outcome = match tokio::time::timeout(POST_TIMEOUT, &mut request).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(join)) => PostOutcome::Uncertain(format!("post task ended: {join}")),
            Err(_) => {
                request.abort();
                PostOutcome::Uncertain("post timed out".into())
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
