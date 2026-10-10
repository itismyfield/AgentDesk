//! Serialized supervisor commands; dropping a reply receiver never cancels a WAL loan.

use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use super::external::ExternalReceipt;
use super::fence::Failure;
use super::ordering::{
    FetchTicket, PendingOverflow, PendingSource, ScanCapability, ValidatedOrderCapability,
};
use super::receipt::{Deferred, Receipt};
use super::source::Source;
use crate::services::tui_input::rows::receipt_identity::{ReceiptIdentity, Responsibility};

pub(crate) struct ScanCommit {
    pub(crate) receipt: Receipt,
    pub(crate) settlement: Result<(), Failure>,
    pub(crate) capability: ScanCapability,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum FinalDisposition {
    NonTarget,
    TooOld,
    PermanentRefusal,
}

pub(crate) enum SupervisorCmd {
    Clear,
    Close {
        ack: oneshot::Sender<Result<(), Deferred>>,
    },
    FetchTicket {
        reply: oneshot::Sender<Result<FetchTicket, Failure>>,
    },
    PendingSource {
        sources: Vec<u64>,
        reply: oneshot::Sender<Result<PendingSource, Failure>>,
    },
    /// Receipt of an external human input; no production path sends it yet.
    SubmitExternal {
        source: Box<Source>,
        reply: oneshot::Sender<ExternalReceipt>,
    },
    LookupResponsibility {
        identity: ReceiptIdentity,
        reply: oneshot::Sender<Responsibility>,
    },
    BeginScan {
        ticket: FetchTicket,
        sources: Vec<u64>,
        horizon: u64,
        complete_fetch: bool,
        reply: oneshot::Sender<Result<ScanCapability, Failure>>,
    },
    CommitFromScan {
        source: Box<Source>,
        capability: ScanCapability,
        reply: oneshot::Sender<ScanCommit>,
    },
    SettleFromScan {
        source: u64,
        disposition: FinalDisposition,
        capability: ScanCapability,
        reply: oneshot::Sender<Result<ScanCapability, Failure>>,
    },
    CompleteScan {
        capability: ScanCapability,
        reply: oneshot::Sender<Result<ValidatedOrderCapability, Failure>>,
    },
}

/// A failed live notice leaves a scoped catch-up obligation even if the mailbox cannot accept it.
pub(crate) async fn pending_source(
    sender: &mpsc::Sender<SupervisorCmd>,
    overflow: &PendingOverflow,
    sources: Vec<u64>,
) -> Result<PendingSource, Deferred> {
    pending_within(sender, overflow, sources, Duration::from_secs(3)).await
}

struct PendingGuard<'a> {
    overflow: &'a PendingOverflow,
    armed: bool,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        #[cfg(test)]
        if super::mutant("drop_pending_drop") {
            return;
        }
        if self.armed {
            self.overflow.mark_dirty();
        }
    }
}

async fn pending_within(
    sender: &mpsc::Sender<SupervisorCmd>,
    overflow: &PendingOverflow,
    sources: Vec<u64>,
    limit: Duration,
) -> Result<PendingSource, Deferred> {
    let mut obligation = PendingGuard {
        overflow,
        armed: true,
    };
    let (reply, response) = oneshot::channel();
    if sender
        .try_send(SupervisorCmd::PendingSource { sources, reply })
        .is_err()
    {
        return Err(Deferred::SupervisorLost);
    }
    match tokio::time::timeout(limit, response).await {
        Ok(Ok(Ok(pending))) => {
            obligation.armed = false;
            Ok(pending)
        }
        // An answered refusal is the order's verdict on these sources, not a lost notice.
        Ok(Ok(Err(_))) => {
            obligation.armed = false;
            Err(Deferred::Order)
        }
        _ => Err(Deferred::SupervisorLost),
    }
}

/// The bound covers both a full mailbox and a receiver that has not started.
pub(crate) async fn request<T>(
    sender: &mpsc::Sender<SupervisorCmd>,
    make: impl FnOnce(oneshot::Sender<T>) -> SupervisorCmd,
) -> Result<T, Deferred> {
    request_within(sender, make, Duration::from_secs(3)).await
}

async fn request_within<T>(
    sender: &mpsc::Sender<SupervisorCmd>,
    make: impl FnOnce(oneshot::Sender<T>) -> SupervisorCmd,
    limit: Duration,
) -> Result<T, Deferred> {
    tokio::time::timeout(limit, async {
        let (reply, response) = oneshot::channel();
        sender
            .send(make(reply))
            .await
            .map_err(|_| Deferred::SupervisorLost)?;
        response.await.map_err(|_| Deferred::SupervisorLost)
    })
    .await
    .unwrap_or(Err(Deferred::SupervisorLost))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn g1a_request_bound_covers_unstarted_receiver_and_full_mailbox() {
        let (sender, _receiver) = mpsc::channel(1);
        let identity = ReceiptIdentity::new(10, vec![10], 7, 8, 9).unwrap();
        let request = |reply| SupervisorCmd::LookupResponsibility {
            identity: identity.clone(),
            reply,
        };
        assert!(matches!(
            request_within(&sender, request, Duration::from_millis(5)).await,
            Err(Deferred::SupervisorLost)
        ));
        assert!(matches!(
            request_within(&sender, request, Duration::from_millis(5)).await,
            Err(Deferred::SupervisorLost)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn g1a_pending_try_send_full_and_closed_dirties_without_queue_wait() {
        let mut order = super::super::ordering::AdmissionOrder::new(
            crate::services::provider::ProviderKind::Claude,
            6_325_760,
            7,
            0,
        );
        let overflow = order.pending_overflow();
        let ticket = order.fetch_ticket(7).unwrap();
        let (sender, receiver) = mpsc::channel(1);
        sender.try_send(SupervisorCmd::Clear).unwrap();
        let at = tokio::time::Instant::now();
        assert!(matches!(
            pending_within(&sender, &overflow, vec![11], Duration::from_secs(60)).await,
            Err(Deferred::SupervisorLost)
        ));
        assert_eq!(at.elapsed(), Duration::ZERO);
        assert_eq!(overflow.generation(), 1);
        assert!(order.begin_scan(ticket, 7, vec![11], 11, true).is_err());
        drop(receiver);
        assert!(matches!(
            pending_within(&sender, &overflow, vec![12], Duration::from_secs(60)).await,
            Err(Deferred::SupervisorLost)
        ));
        assert_eq!(at.elapsed(), Duration::ZERO);
        assert_eq!(overflow.generation(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn g1a_pending_unstarted_ack_timeout_is_bounded_and_marks_dirty() {
        let order = super::super::ordering::AdmissionOrder::new(
            crate::services::provider::ProviderKind::Claude,
            6_325_761,
            7,
            0,
        );
        let overflow = order.pending_overflow();
        let (sender, _receiver) = mpsc::channel(1);
        let at = tokio::time::Instant::now();
        assert!(matches!(
            pending_within(&sender, &overflow, vec![11], Duration::from_secs(1)).await,
            Err(Deferred::SupervisorLost)
        ));
        assert_eq!(at.elapsed(), Duration::from_secs(1));
        assert_eq!(overflow.generation(), 1);
    }

    #[tokio::test]
    async fn g1a_pending_success_ack_keeps_the_same_scoped_handle_clean() {
        let mut order = super::super::ordering::AdmissionOrder::new(
            crate::services::provider::ProviderKind::Claude,
            6_325_762,
            7,
            0,
        );
        let overflow = order.pending_overflow();
        let (sender, mut receiver) = mpsc::channel(1);
        let service = tokio::spawn(async move {
            if let SupervisorCmd::PendingSource { sources, reply } = receiver.recv().await.unwrap()
            {
                reply.send(order.admit_pending(&sources)).unwrap();
            } else {
                panic!("wrong command");
            }
        });
        let pending = pending_source(&sender, &overflow, vec![11]).await.unwrap();
        assert_eq!(pending.sources, [11]);
        assert_eq!(pending.dirty_generation, 1);
        assert_eq!(pending.overflow.generation(), 0);
        assert_eq!(overflow.generation(), 0);
        service.await.unwrap();
    }

    #[tokio::test]
    async fn pending_order_error_is_not_supervisor_lost() {
        let mut order = super::super::ordering::AdmissionOrder::new(
            crate::services::provider::ProviderKind::Claude,
            6_325_763,
            7,
            0,
        );
        let overflow = order.pending_overflow();
        let (sender, mut receiver) = mpsc::channel(1);
        let service = tokio::spawn(async move {
            while let Some(SupervisorCmd::PendingSource { sources, reply }) = receiver.recv().await
            {
                reply.send(order.admit_pending(&sources)).unwrap();
            }
            order
        });
        let external = crate::services::tui_input::input_key::EXTERNAL_KEY_BASE;
        let refused = pending_source(&sender, &overflow, vec![11, external]).await;
        assert!(matches!(refused, Err(Deferred::Order)));
        assert_eq!(overflow.generation(), 0);
        let pending = pending_source(&sender, &overflow, vec![12]).await.unwrap();
        assert_eq!(pending.sources, [12]);
        drop(sender);
        assert_eq!(service.await.unwrap().pending_snapshot().sources, [12]);
    }

    #[tokio::test]
    async fn g1a_pending_abort_and_receiver_exit_after_try_send_preserve_dirty_obligation() {
        for (index, abort) in [true, false].into_iter().enumerate() {
            let mut order = super::super::ordering::AdmissionOrder::new(
                crate::services::provider::ProviderKind::Claude,
                6_325_770 + index as u64,
                7,
                0,
            );
            let overflow = order.pending_overflow();
            let ticket = order.fetch_ticket(7).unwrap();
            let (sender, mut receiver) = mpsc::channel(1);
            let task = tokio::spawn({
                let overflow = overflow.clone();
                async move { pending_source(&sender, &overflow, vec![11]).await }
            });
            let command = receiver.recv().await.unwrap();
            if abort {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
                drop(command);
            } else {
                drop(command);
                drop(receiver);
                assert!(matches!(task.await.unwrap(), Err(Deferred::SupervisorLost)));
            }
            assert_eq!(overflow.generation(), 1);
            assert!(order.begin_scan(ticket, 7, vec![11], 11, true).is_err());
        }
    }
}
