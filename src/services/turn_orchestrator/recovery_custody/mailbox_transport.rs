//! Carries an issued capability inside its request, whole, into either branch of the mailbox
//! actor's step; every refusal on the way hands the same carrier back.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;
use tokio::sync::oneshot;

use super::super::{
    ChannelMailboxHandle, ChannelMailboxMsg, ChannelMailboxState, QueuePersistenceContext,
    RecoveryDoneSignal,
};
use super::mailbox_access::{self, Install};
use super::recovery_custody::CustodyStore;
use super::recovery_custody::capability::{Carrier, Unapplied, Unsent};
use crate::services::discord::input_runtime::fence::{self, Failure};

/// A carried install on its way through the actor; until the actor's index entry takes the carrier,
/// any end of the request sends it back.
pub(crate) struct Carried {
    custody: CustodyStore,
    carrier: Option<Carrier<Install>>,
    back: Option<oneshot::Sender<Unapplied<Install>>>,
}

impl Carried {
    /// The namespace its own request names, which the actor's input fence judges.
    pub(crate) fn persistence(&self) -> Option<&QueuePersistenceContext> {
        let carrier = self.carrier.as_ref()?;
        carrier.payload().map(Install::persistence)
    }

    fn give_back(&mut self, refusal: Unsent) {
        if let (Some(carrier), Some(back)) = (self.carrier.take(), self.back.take()) {
            let _ = back.send(carrier.refuse(refusal));
        }
    }

    /// A purge-closed actor's refusal; returns the arm name the closed gate logs.
    pub(crate) fn refuse_closed(mut self: Box<Self>) -> &'static str {
        self.give_back(Unsent::Closed);
        "Carried"
    }

    pub(crate) fn refuse_fenced(mut self: Box<Self>, failure: Failure) {
        self.give_back(Unsent::Fenced(failure));
    }

    /// The actor's step: the index entry turns the queued carrier into one counted apply, which
    /// runs and ends inside this call; a refused entry sends the carrier back.
    pub(crate) fn apply(
        mut self: Box<Self>,
        state: &mut ChannelMailboxState,
        channel: ChannelId,
        actor: &Arc<RecoveryDoneSignal>,
    ) {
        let (Some(carrier), Some(back)) = (self.carrier.take(), self.back.take()) else {
            return;
        };
        match self
            .custody
            .admit_carried(&channel.get().to_string(), carrier)
        {
            Ok((guard, install)) => mailbox_access::apply(state, channel, actor, &guard, install),
            Err(refused) => {
                let _ = back.send(refused);
            }
        }
    }
}

impl Drop for Carried {
    /// A request dropped before the actor took it, by a failed send or with a gone actor's queue.
    fn drop(&mut self) {
        self.give_back(Unsent::SendFailed);
    }
}

/// Sends the carrier to `handle`'s actor and waits until the actor took it or handed it back.
pub(super) async fn carry(
    handle: &ChannelMailboxHandle,
    custody: &CustodyStore,
    carrier: Carrier<Install>,
) -> Result<(), Unapplied<Install>> {
    let (back, returned) = oneshot::channel();
    let carried = Carried {
        custody: custody.clone(),
        carrier: Some(carrier),
        back: Some(back),
    };
    let msg = ChannelMailboxMsg::Carried(Box::new(carried));
    // A failed send drops the request here, and its carrier comes back as `SendFailed`.
    drop(handle.sender.send((msg, fence::effect::current())));
    returned.await.map_or(Ok(()), Err)
}
