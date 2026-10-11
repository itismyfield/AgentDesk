//! One granted re-post send: the writer seam it needs, the permit that owns the spent slot, and
//! the dispatch that starts its request under the gate and then owns the request's timeout.

use std::future::{Future, poll_fn};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;

use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::identity::marker;
use super::o_piece_attempts::{self, AttemptResult, SlotGrant};
use super::o_piece_delivery::{self, LedgerError, PieceKey, Receipt, ReceiptMethod};
use super::send::{
    BoundedTransport, DispatchCounts, DispatchReport, RepostEnvelope, RepostIds, WireOutcome,
    hand_over,
};
use crate::services::tui_o::ownership::{GatewayOwnership, OwnershipGate};
use crate::services::tui_o::writer::deliver::POST_TIMEOUT;

/// What the channel's writer lends a re-post turn. The original path, Auto and an operator
/// resume all go through the same writer, so one channel has one open Prepared at a time.
pub(crate) trait RepostWriter {
    /// The channel's delivery lease; dropping it releases the lease.
    type Lease: Send + 'static;
    /// Settles an open Prepared of the channel; `false` while one is still open.
    fn settle_open(&mut self) -> impl Future<Output = bool> + Send;
    /// Writer stop, violation, exclusion, delivery_allowed, s3act availability, switch, drain.
    fn guard(&self) -> Result<(), String>;
    fn try_lease(&self) -> Option<Self::Lease>;
    fn gate(&self) -> Arc<OwnershipGate>;
    /// Whether switch, holder and run still let a request leave; read without the gate lock.
    fn live(&self) -> Arc<dyn Fn() -> bool + Send + Sync>;
    /// Appends this send's ordinary Prepared at `epoch`. Runs under the gate lock: no await.
    fn prepare(&mut self, epoch: u64, permit: &DispatchPermit) -> Result<(), String>;
    /// Records how a prepared send ended; `Unsent` withdraws the Prepared.
    fn record(&mut self, report: &DispatchReport) -> impl Future<Output = ()> + Send;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GuardReason {
    Writer(String),
    LeaseBusy,
    NoGateway,
    Approval(String),
}

/// What a turn may do next. `Granted` is the only one that sends, and only through `start`.
#[derive(Debug)]
pub(crate) enum Admission<L> {
    Granted {
        permit: DispatchPermit,
        lease: L,
    },
    /// Not admitted: the existing single-shot path, once.
    LegacyOff,
    WaitForOpen,
    /// Wait for fresh evidence or recovery; nothing is sent.
    ObserveOnly,
    AlreadyResolved,
    CapReached,
    CapUnknown,
    /// Admitted while the switch is off: no send, budget kept.
    Parked,
    GuardRejected(GuardReason),
}

/// The right to send one spent slot once. It owns the grant, so neither a re-read row nor a
/// copy can make another; only the runner builds it from a committed grant.
#[derive(Debug)]
pub(crate) struct DispatchPermit {
    grant: SlotGrant,
    payload: String,
    /// The wire deadline, counted from before the grant was asked.
    deadline: Instant,
}

impl DispatchPermit {
    pub(super) fn new(grant: SlotGrant, payload: String, deadline: Instant) -> Self {
        Self {
            grant,
            payload,
            deadline,
        }
    }

    pub(crate) fn key(&self) -> &PieceKey {
        self.grant.key()
    }

    pub(crate) fn slot(&self) -> u8 {
        self.grant.slot()
    }

    pub(crate) fn payload(&self) -> &str {
        &self.payload
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DispatchEnd {
    /// The gate was not owned at admission: no Prepared and no request. The slot stays spent.
    NotAdmitted,
    /// Withdrawn before any request; no Prepared unless the writer took one.
    Withdrawn(String),
    Sent(DispatchReport),
}

enum Run {
    Done(DispatchEnd),
    /// The request after its first poll, in a task that also holds the lease and the timeout.
    Running(JoinHandle<WireOutcome>, DispatchCounts),
}

/// A started dispatch. Dropping it stops only the wait: the task still ends the request.
pub(crate) struct Started {
    grant: SlotGrant,
    envelope: Option<RepostEnvelope>,
    run: Run,
}

fn withdrawn(permit: DispatchPermit, reason: &str) -> Started {
    let run = Run::Done(DispatchEnd::Withdrawn(reason.into()));
    Started {
        grant: permit.grant,
        envelope: None,
        run,
    }
}

/// Under the gate: rechecks, appends the Prepared and polls the request once. The rest of the
/// request, its 429 waits and its timeout run in one task that holds the lease until it ends.
pub(crate) async fn start<W: RepostWriter>(
    writer: &mut W,
    permit: DispatchPermit,
    lease: W::Lease,
    transport: &impl BoundedTransport,
) -> Started {
    let Some(ids) = RepostIds::for_piece(&marker(permit.key())) else {
        return withdrawn(permit, "the marker cannot travel");
    };
    if permit.deadline.saturating_duration_since(Instant::now()) < POST_TIMEOUT {
        return withdrawn(permit, "too little time left before the deadline");
    }
    let channel = permit.key().unit().channel_id;
    let envelope = RepostEnvelope::additional(channel, permit.payload.clone(), ids);
    let gate = writer.gate();
    let admitted = Arc::new(AtomicU64::new(0));
    let (observed, writer_live, deadline) = (gate.subscribe(), writer.live(), permit.deadline);
    let epoch = Arc::clone(&admitted);
    // Checked before every request leaves; the gate is observed, never locked again.
    let live = Arc::new(move || {
        let owned = GatewayOwnership::Owned {
            epoch: epoch.load(Ordering::SeqCst),
        };
        *observed.borrow() == owned && writer_live() && Instant::now() < deadline
    });
    let (request, counts) = hand_over(transport, &envelope, live);
    let mut request = Box::pin(request);
    let still_live = writer.live();
    let first = poll_fn(|cx| {
        Poll::Ready(gate.admit(|epoch| {
            if !still_live() {
                return Err("withdrawn before the Prepared".to_owned());
            }
            writer.prepare(epoch, &permit)?;
            admitted.store(epoch, Ordering::SeqCst);
            Ok(request.as_mut().poll(cx))
        }))
    })
    .await;
    let run = match first {
        None => Run::Done(DispatchEnd::NotAdmitted),
        Some(Err(reason)) => Run::Done(DispatchEnd::Withdrawn(reason)),
        Some(Ok(Poll::Ready(outcome))) => Run::Done(DispatchEnd::Sent(counts.report(outcome))),
        Some(Ok(Poll::Pending)) => {
            let task = tokio::spawn(async move {
                let ended = tokio::time::timeout(POST_TIMEOUT, request).await;
                // The request is dropped, so aborted, before the lease goes.
                drop(lease);
                ended.unwrap_or(WireOutcome::TimedOut)
            });
            Run::Running(task, counts)
        }
    };
    Started {
        grant: permit.grant,
        envelope: Some(envelope),
        run,
    }
}

impl Started {
    /// Waits for the request, records it with the writer and settles the slot in PostgreSQL. A
    /// response that kept the marker also becomes the piece's receipt.
    pub(crate) async fn finish<W: RepostWriter>(
        self,
        writer: &mut W,
        pool: &PgPool,
    ) -> Result<DispatchEnd, LedgerError> {
        let end = match self.run {
            Run::Done(end) => end,
            Run::Running(task, counts) => {
                let outcome = task
                    .await
                    .unwrap_or_else(|join| WireOutcome::Uncertain(join.to_string()));
                DispatchEnd::Sent(counts.report(outcome))
            }
        };
        let result = match &end {
            DispatchEnd::NotAdmitted | DispatchEnd::Withdrawn(_) => AttemptResult::NotSent,
            DispatchEnd::Sent(report) => match &report.outcome {
                WireOutcome::Created(_) => AttemptResult::Created,
                WireOutcome::Refused(_) => AttemptResult::Rejected,
                WireOutcome::Throttled | WireOutcome::Unsent(_) => AttemptResult::NotSent,
                WireOutcome::Uncertain(_) | WireOutcome::TimedOut => AttemptResult::Uncertain,
            },
        };
        if let DispatchEnd::Sent(report) = &end {
            writer.record(report).await;
        }
        o_piece_attempts::settle(pool, &self.grant, result).await?;
        if let (DispatchEnd::Sent(report), Some(envelope)) = (&end, &self.envelope)
            && let WireOutcome::Created(created) = &report.outcome
            && envelope.carries_marker(created)
        {
            let receipt = Receipt {
                key: self.grant.key().clone(),
                message_id: created.id,
                author_id: created.author_id,
                slot: Some(self.grant.slot()),
                method: ReceiptMethod::PostResponse,
            };
            o_piece_delivery::record_receipt(pool, &receipt).await?;
        }
        Ok(end)
    }
}
