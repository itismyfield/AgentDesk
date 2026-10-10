//! Dormant typing admission; wake events carry no Busy or ownership authority.

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use super::activity::Reading;
use super::lifecycle::Ticket;
use crate::services::cluster::channel_home::{self, HomeGate, HomeOwnership};
use crate::services::tui_o::ownership::{self, GatewayOwnership, OwnershipGate};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::turn_mode;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Identity {
    pub provider: ShadowProvider,
    pub channel: u64,
    pub session: String,
    pub source: SourceId,
    pub binding_seq: u64,
    pub bot_id: u64,
}

enum Owner {
    Home(Arc<HomeGate>, u64),
    Gateway(Arc<OwnershipGate>, u64),
}

impl Owner {
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Home(a, x), Self::Home(b, y)) => Arc::ptr_eq(a, b) && x == y,
            (Self::Gateway(a, x), Self::Gateway(b, y)) => Arc::ptr_eq(a, b) && x == y,
            _ => false,
        }
    }

    fn current(identity: &Identity) -> Option<Self> {
        if let Some(home) = channel_home::registered_channel(identity.channel) {
            return match home.ownership() {
                HomeOwnership::Owned { gate_epoch, .. } => Some(Self::Home(home, gate_epoch)),
                HomeOwnership::Lost => None,
            };
        }
        let provider = match identity.provider {
            ShadowProvider::Claude => "claude",
            ShadowProvider::Codex => "codex",
        };
        let gate = ownership::gate(provider);
        match gate.current() {
            GatewayOwnership::Owned { epoch } => Some(Self::Gateway(gate, epoch)),
            GatewayOwnership::Unknown | GatewayOwnership::Lost => None,
        }
    }

    fn admit<R>(&self, channel: u64, hand_off: impl FnOnce() -> R) -> Option<R> {
        match (self, channel_home::registered_channel(channel)) {
            (Self::Home(home, expected), Some(current)) if Arc::ptr_eq(home, &current) => home
                .admit_post(|epoch| (epoch == *expected).then(hand_off))
                .flatten(),
            (Self::Gateway(gate, expected), None) => gate
                .admit(|epoch| (epoch == *expected).then(hand_off))
                .flatten(),
            _ => None,
        }
    }
}

struct Current {
    token: Arc<()>,
    identity: Identity,
    owner: Owner,
}

/// A local runtime starts empty; invalidation replaces a token even for Busy-to-Busy changes.
#[derive(Default)]
pub(super) struct Incarnation(Mutex<Option<Current>>);

impl Incarnation {
    pub(super) fn invalidate(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Adopts a strict reading's identity only while the ticket it was observed under is current;
    /// the reading then carries the advanced ticket, so no other reading can borrow it.
    pub(super) fn adopt(&self, reading: &mut Reading, bot_id: u64) -> bool {
        let ticket = reading.observed_under().cloned();
        let (Some(ticket), Some(identity)) = (ticket, reading.identity(bot_id)) else {
            return false;
        };
        let Some(successor) = self.seat(&ticket, identity) else {
            return false;
        };
        reading.rebind(successor);
        true
    }

    fn replace(&self, ticket: &Ticket, identity: Identity) -> bool {
        self.seat(ticket, identity).is_some()
    }

    fn seat(&self, ticket: &Ticket, identity: Identity) -> Option<Ticket> {
        let owner = (identity.channel != 0 && identity.bot_id != 0)
            .then(|| Owner::current(&identity))
            .flatten();
        ticket
            .with_current(|registration| {
                if ticket.channel() != identity.channel
                    || !std::ptr::eq(self, registration.incarnation.as_ref())
                {
                    return None;
                }
                let mut current = self.0.lock().unwrap_or_else(|e| e.into_inner());
                if let (Some(current), Some(owner)) = (current.as_ref(), owner.as_ref())
                    && current.identity == identity
                    && current.owner.same(owner)
                {
                    return Some(ticket.clone());
                }
                registration.advance_ticket();
                *current = owner.map(|owner| Current {
                    token: Arc::new(()),
                    identity,
                    owner,
                });
                current.as_ref()?;
                Some(ticket.successor(registration))
            })
            .flatten()
    }

    /// Only the ticket the reading was observed under, or the one its adoption advanced to.
    pub(super) fn approve(self: &Arc<Self>, reading: Reading) -> Option<Approval> {
        let ticket = reading.observed_under()?.clone();
        ticket
            .admit(self, || self.approve_current(&ticket, reading))
            .flatten()
    }

    /// Seats an approval on a Gateway witness the caller names, as a provider gate would.
    pub(super) fn seat_gateway_for_tests(
        &self,
        ticket: &Ticket,
        identity: Identity,
        gate: Arc<OwnershipGate>,
    ) -> bool {
        ticket
            .with_current(|registration| {
                registration.advance_ticket();
                *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(Current {
                    token: Arc::new(()),
                    identity,
                    owner: Owner::Gateway(gate, 1),
                });
            })
            .is_some()
    }

    pub(super) fn rests_on(&self, gate: &Arc<OwnershipGate>) -> bool {
        let current = self.0.lock().unwrap_or_else(|e| e.into_inner());
        matches!(
            current.as_ref().map(|current| &current.owner),
            Some(Owner::Gateway(owner, _)) if Arc::ptr_eq(owner, gate)
        )
    }

    fn approve_current(self: &Arc<Self>, ticket: &Ticket, reading: Reading) -> Option<Approval> {
        let current = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let current = current.as_ref()?;
        let token = reading.with_busy(&current.identity, || current.token.clone())?;
        Some(Approval {
            incarnation: self.clone(),
            ticket: ticket.clone(),
            token,
            reading,
        })
    }
}

/// Never serialized or restored: a receiving process approves its own fresh Reading.
pub(super) struct Approval {
    incarnation: Arc<Incarnation>,
    ticket: Ticket,
    token: Arc<()>,
    reading: Reading,
}

pub(super) enum Started<F: Future> {
    Ready(F::Output),
    Pending(Pin<Box<F>>),
}

impl<F: Future> Started<F> {
    pub(super) async fn finish(self) -> F::Output {
        match self {
            Self::Ready(output) => output,
            Self::Pending(request) => request.await,
        }
    }
}

impl Approval {
    /// First poll holds registry, mode, lifecycle, incarnation, source-log, Reading and owner fences.
    pub(super) async fn start<F: Future>(
        self,
        bot_id: u64,
        actual_channel: u64,
        request: impl FnOnce() -> F,
    ) -> Option<Started<F>> {
        let mut request = Some(request);
        poll_fn(|cx| {
            // A watcher change and its withdrawal land under this lock, never between the checks.
            let _registry = crate::services::discord::lock_tmux_watcher_registry();
            Poll::Ready(
                turn_mode::admit_effect(actual_channel, true, || {
                    self.ticket
                        .admit(&self.incarnation, || {
                            let current =
                                self.incarnation.0.lock().unwrap_or_else(|e| e.into_inner());
                            let current = current.as_ref()?;
                            if !Arc::ptr_eq(&current.token, &self.token)
                                || bot_id != current.identity.bot_id
                                || actual_channel != current.identity.channel
                            {
                                return None;
                            }
                            let request = request.take()?;
                            crate::services::tui_prompt_dedupe::binding_events::admit_committed_seq(
                                current.identity.channel,
                                current.identity.binding_seq,
                                || {
                                    self.reading
                                        .with_busy(&current.identity, || {
                                            current.owner.admit(current.identity.channel, || {
                                                let mut future = Box::pin(request());
                                                match future.as_mut().poll(cx) {
                                                    Poll::Ready(output) => Started::Ready(output),
                                                    Poll::Pending => Started::Pending(future),
                                                }
                                            })
                                        })
                                        .flatten()
                                },
                            )
                            .ok()
                            .flatten()
                            .flatten()
                        })
                        .flatten()
                })
                .flatten(),
            )
        })
        .await
    }
}

#[cfg(all(test, unix))]
#[path = "admission_tests.rs"]
mod tests;
