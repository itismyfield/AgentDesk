//! Serialized supervisor commands; dropping a reply receiver never cancels a WAL loan.

use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use super::receipt::Deferred;
use crate::services::tui_input::rows::receipt_identity::{ReceiptIdentity, Responsibility};

pub(crate) enum SupervisorCmd {
    Clear,
    Close {
        ack: oneshot::Sender<Result<(), Deferred>>,
    },
    LookupResponsibility {
        identity: ReceiptIdentity,
        reply: oneshot::Sender<Responsibility>,
    },
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
}
