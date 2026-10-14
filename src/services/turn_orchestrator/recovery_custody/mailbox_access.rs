//! The one request adapter between a channel's binding and its mailbox actor, and the actor side of
//! a carried scoped install: what may be sent for each binding, and what an install answers.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;
use tokio::sync::oneshot;

use super::super::installed_actor_handoff::Installation;
use super::super::scoped_claim::{self, InstalledPending, ScopedClaimOutcome, ScopedMsg};
use super::super::{
    ChannelMailboxHandle, ChannelMailboxMsg, ChannelMailboxState, MailboxUnreachable,
    QueuePersistenceContext, RecoveryDoneSignal,
};
use super::recovery_custody::capability::{ApplyGuard, AttemptKey, RevokeHandle};
use super::recovery_custody::model::Namespace;
#[cfg(unix)]
use super::{
    mailbox_transport,
    recovery_custody::CustodyStore,
    recovery_custody::capability::{Carrier, Unapplied, Unsent},
    recovery_custody::model::CanonicalSeal,
    recovery_custody::operation::{OutcomeReport, StepOutcome},
    recovery_registry::Binding,
    recovery_registry::entry::AttemptHandle,
};
#[cfg(unix)]
use crate::services::cluster::channel_home_port::scoped_restore::RestoreStatus;
use crate::services::discord::input_runtime::fence;

/// A scoped claim carried under an `ActorInstall` arm; its answer comes back on `reply`.
pub(crate) struct Install {
    pub(crate) proof: Installation,
    pub(crate) persistence: QueuePersistenceContext,
    pub(crate) reply: oneshot::Sender<InstallAnswer>,
}

impl Install {
    pub(crate) fn persistence(&self) -> &QueuePersistenceContext {
        &self.persistence
    }
}

pub(crate) enum InstallAnswer {
    /// The claim installed the actor and its pending record in one step.
    Installed(Installed),
    /// The claim installed nothing: observed, inherited or refused, as it answered.
    Claim(ScopedClaimOutcome),
    /// The claim answered an install the actor's pending record does not hold.
    Unconfirmed,
    /// The entered apply is not this channel's and namespace's; nothing ran.
    WrongTarget,
}

/// One install's single result: the actor that holds it, the arm and attempt it ran under, and the
/// event ids its pending record owes.
pub(crate) struct Installed {
    actor: Arc<RecoveryDoneSignal>,
    operation_id: String,
    arm_id: String,
    attempt: Option<AttemptKey>,
    event_ids: Vec<String>,
}

impl Installed {
    pub(crate) fn same_actor(&self, handle: &ChannelMailboxHandle) -> bool {
        Arc::ptr_eq(&self.actor, handle.recovery_done())
    }

    pub(crate) fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub(crate) fn arm_id(&self) -> &str {
        &self.arm_id
    }

    pub(crate) fn event_ids(&self) -> &[String] {
        &self.event_ids
    }
}

/// Why an install was not recorded; nothing was written.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NotRecorded {
    /// Its arm was not issued to this attempt, or the attempt is no longer the slot's current one.
    OtherAttempt,
    Superseded,
}

#[cfg(unix)]
impl Installed {
    /// Records the install as its arm's result: the attempt's witness, then custody's `Applied`.
    /// A refusal returns the result and the report untouched.
    pub(crate) fn record(
        self,
        attempt: &AttemptHandle,
        report: OutcomeReport,
        after: CanonicalSeal,
    ) -> Result<(), (NotRecorded, Self, OutcomeReport)> {
        if !attempt.marks(self.attempt.as_ref()) {
            return Err((NotRecorded::OtherAttempt, self, report));
        }
        if attempt.witness().record(RestoreStatus::Installed).is_err() {
            return Err((NotRecorded::Superseded, self, report));
        }
        let after = Box::new(after);
        report.send(StepOutcome::Applied {
            after,
            handoff: None,
        });
        Ok(())
    }
}

/// The installed identity a pending record must carry for `proof`.
fn holds(pending: &InstalledPending, proof: &Installation) -> bool {
    let token = pending.token.upgrade();
    token.is_some_and(|token| Arc::ptr_eq(&token, &proof.token))
        && (&pending.birth, &pending.episode) == (&proof.birth, &proof.episode)
        && (pending.owner, pending.message) == (proof.owner, proof.message)
}

fn event_ids(pending: &InstalledPending) -> Vec<String> {
    let exits = pending.exits.iter();
    exits.map(|exit| exit.debt.event_id.clone()).collect()
}

/// The actor side, inside the entered apply: `scoped_claim` runs unchanged under the apply lent
/// for the request's namespace, and only an install its pending record holds becomes [`Installed`].
pub(super) fn apply(
    state: &mut ChannelMailboxState,
    channel: ChannelId,
    actor: &Arc<RecoveryDoneSignal>,
    guard: &ApplyGuard,
    install: Install,
) {
    let Install {
        proof,
        persistence,
        reply,
    } = install;
    let namespace = Namespace {
        provider: persistence.provider.as_str().to_string(),
        token_hash: persistence.token_hash.clone(),
    };
    let Ok(lent) = guard.lend(&channel.get().to_string(), &namespace) else {
        let _ = reply.send(InstallAnswer::WrongTarget);
        return;
    };
    // A caller that already left gets no install, as a plain claim's would not.
    if reply.is_closed() {
        return;
    }
    let outcome = lent.effect(|| {
        let (claimed, mut answer) = oneshot::channel();
        let claim = ScopedMsg::Claim {
            proof: proof.clone(),
            persistence,
            reply: claimed,
        };
        scoped_claim::step(state, channel, claim);
        answer.try_recv().ok()
    });
    let pending = state.installed_pending.as_ref();
    let recorded = pending
        .filter(|pending| holds(pending, &proof))
        .map(event_ids);
    let answer = match outcome {
        Some(ScopedClaimOutcome::Installed(ids)) if recorded.as_ref() == Some(&ids) => {
            InstallAnswer::Installed(Installed {
                actor: actor.clone(),
                operation_id: lent.operation_id().to_string(),
                arm_id: lent.arm_id().to_string(),
                attempt: lent.attempt().cloned(),
                event_ids: ids,
            })
        }
        Some(ScopedClaimOutcome::Installed(_)) | None => InstallAnswer::Unconfirmed,
        Some(outcome) => InstallAnswer::Claim(outcome),
    };
    let _ = reply.send(answer);
}

/// After a lost answer: the same actor's pending record for `proof`, read only, as `arm`'s install,
/// so a retry issues no new debt; an actor that cannot be asked is an error, never "not installed".
pub(crate) async fn recover(
    handle: &ChannelMailboxHandle,
    proof: &Installation,
    arm: &RevokeHandle,
) -> Result<Option<Installed>, MailboxUnreachable> {
    let read = |reply| ChannelMailboxMsg::ScopedClaim(ScopedMsg::Pending { reply });
    let Some(pending) = handle.request(read).await? else {
        return Ok(None);
    };
    Ok(holds(&pending, proof).then(|| Installed {
        actor: handle.recovery_done().clone(),
        operation_id: arm.operation_id().to_string(),
        arm_id: arm.arm_id().to_string(),
        attempt: arm.attempt().cloned(),
        event_ids: event_ids(&pending),
    }))
}

/// What a custody contact asks of a channel's actor.
#[cfg(unix)]
pub(crate) enum Ask {
    /// A scoped claim message sent as it is.
    Plain(ScopedMsg),
    /// An install under an issued arm, with its capability.
    Carried(Carrier<Install>),
}

#[cfg(unix)]
pub(crate) enum NotSent {
    /// The binding refuses a plain install; the message comes back unsent.
    Refused(ScopedMsg),
    /// The actor was gone; the message's reply is dropped with it.
    Unreachable,
    /// The same carrier, unused, with why.
    Carrier(Unapplied<Install>),
}

#[cfg(unix)]
enum Class {
    Read,
    Install,
}

/// Every scoped subcommand, classified here so `scoped_claim` stays as it is.
#[cfg(unix)]
fn class(msg: &ScopedMsg) -> Class {
    match msg {
        ScopedMsg::Pending { .. } => Class::Read,
        ScopedMsg::Claim { .. } => Class::Install,
    }
}

/// The one adapter: a read goes to the actor on any binding; a plain install only where no custody
/// answers; a carried install only where one does. Unread and undecided bindings are never legacy.
#[cfg(unix)]
pub(crate) async fn request(
    handle: &ChannelMailboxHandle,
    binding: &Binding,
    custody: &CustodyStore,
    ask: Ask,
) -> Result<(), NotSent> {
    let msg = match (ask, binding) {
        (Ask::Carried(carrier), Binding::Attached(_)) => {
            let carried = mailbox_transport::carry(handle, custody, carrier);
            return carried.await.map_err(NotSent::Carrier);
        }
        (Ask::Carried(carrier), Binding::Unmanaged) => {
            return Err(NotSent::Carrier(carrier.refuse(Unsent::Unmanaged)));
        }
        (Ask::Carried(carrier), Binding::Unread) => {
            return Err(NotSent::Carrier(carrier.refuse(Unsent::Unread)));
        }
        (Ask::Plain(msg), binding) => match (class(&msg), binding) {
            (Class::Read, Binding::Unmanaged | Binding::Attached(_) | Binding::Unread)
            | (Class::Install, Binding::Unmanaged) => msg,
            (Class::Install, Binding::Attached(_) | Binding::Unread) => {
                return Err(NotSent::Refused(msg));
            }
        },
    };
    let sent = (
        ChannelMailboxMsg::ScopedClaim(msg),
        fence::effect::current(),
    );
    handle.sender.send(sent).map_err(|_| NotSent::Unreachable)
}
