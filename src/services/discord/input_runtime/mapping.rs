//! Reads every runtime's parent → dispatch-thread map without retaining a reference across await.

use std::sync::{Arc, OnceLock};

use poise::serenity_prelude::ChannelId;

use super::Registry;
use super::reconcile::HoldCause;
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Check {
    Empty,
    Mapped(Edge),
    /// No runtime has been handed over yet.
    Pending,
    /// The handed-over runtimes do not cover every expected provider exactly once.
    Unavailable,
}

/// An incident edge and the provider whose map holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Edge {
    pub(crate) writer: String,
    pub(crate) parent: u64,
    pub(crate) thread: u64,
}

/// Shares the handed-over runtimes; the first adoption is final, so a later one cannot narrow it.
#[derive(Clone, Default)]
pub(crate) struct Probe(Arc<OnceLock<Option<Vec<Arc<SharedData>>>>>);

impl Probe {
    pub(crate) fn adopt(&self, expected: &[ProviderKind], runtimes: Vec<Arc<SharedData>>) -> bool {
        let covered = runtimes.len() == expected.len()
            && expected
                .iter()
                .all(|provider| runtimes.iter().filter(|r| &r.provider == provider).count() == 1);
        self.0.set(covered.then_some(runtimes)).is_ok()
    }

    /// After covered writers drain, only deletion can change this channel's incident edges.
    pub(crate) fn check(&self, channel: u64) -> Check {
        let Some(view) = self.0.get() else {
            return Check::Pending;
        };
        let Some(runtimes) = view else {
            return Check::Unavailable;
        };
        let channel = ChannelId::new(channel);
        for shared in runtimes {
            if let Some(edge) = (shared.dispatch.thread_parents.iter())
                .find(|edge| *edge.key() == channel || *edge.value() == channel)
            {
                return Check::Mapped(Edge {
                    writer: shared.provider.as_str().to_owned(),
                    parent: edge.key().get(),
                    thread: edge.value().get(),
                });
            }
        }
        Check::Empty
    }
}

/// A channel's mapping check at one effect boundary; it moves into blocking workers.
#[derive(Clone)]
pub(crate) struct Guard {
    pub(super) registry: &'static Registry,
    pub(super) key: (String, u64),
    pub(super) probe: Probe,
    pub(super) ledger: bool,
}

impl Guard {
    /// A found edge latches for the process; a pending or partial view holds only this pass.
    pub(crate) fn check(&self, boundary: &'static str) -> Result<(), HoldCause> {
        if let Some(cause) = self.registry.latched(&self.key) {
            return Err(cause);
        }
        if !self.ledger {
            return Ok(());
        }
        match self.probe.check(self.key.1) {
            Check::Empty => Ok(()),
            Check::Mapped(edge) => {
                let cause = HoldCause::MappingPresent(edge, boundary);
                Err(self.registry.latch(&self.key, cause))
            }
            Check::Pending => Err(HoldCause::RuntimeViewPending),
            Check::Unavailable => Err(HoldCause::MappingUnavailable),
        }
    }
}
