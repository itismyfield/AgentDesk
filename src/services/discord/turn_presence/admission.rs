//! Dormant typing admission; wake events carry no Busy or ownership authority.
#![allow(dead_code)]

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use super::activity::Reading;
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

    pub(super) fn replace(&self, identity: Identity) -> bool {
        let owner = (identity.channel != 0 && identity.bot_id != 0)
            .then(|| Owner::current(&identity))
            .flatten();
        let mut current = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *current = owner.map(|owner| Current {
            token: Arc::new(()),
            identity,
            owner,
        });
        current.is_some()
    }

    pub(super) fn approve(self: &Arc<Self>, reading: Reading) -> Option<Approval> {
        let current = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let current = current.as_ref()?;
        let token = reading.with_busy(&current.identity, || current.token.clone())?;
        Some(Approval {
            incarnation: self.clone(),
            token,
            reading,
        })
    }
}

/// Never serialized or restored: a receiving process approves its own fresh Reading.
pub(super) struct Approval {
    incarnation: Arc<Incarnation>,
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
    /// First poll occurs under mode, incarnation, Reading and owner locks; responses await outside.
    pub(super) async fn start<F: Future>(
        self,
        bot_id: u64,
        actual_channel: u64,
        request: impl FnOnce() -> F,
    ) -> Option<Started<F>> {
        let mut request = Some(request);
        poll_fn(|cx| {
            Poll::Ready(
                turn_mode::admit_effect(actual_channel, true, || {
                    let current = self.incarnation.0.lock().unwrap_or_else(|e| e.into_inner());
                    let current = current.as_ref()?;
                    if !Arc::ptr_eq(&current.token, &self.token)
                        || bot_id != current.identity.bot_id
                        || actual_channel != current.identity.channel
                    {
                        return None;
                    }
                    let request = request.take()?;
                    self.reading
                        .with_busy(&current.identity, || {
                            current.owner.admit(actual_channel, || {
                                let mut future = Box::pin(request());
                                match future.as_mut().poll(cx) {
                                    Poll::Ready(output) => Started::Ready(output),
                                    Poll::Pending => Started::Pending(future),
                                }
                            })
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
