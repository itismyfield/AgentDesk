use std::sync::Arc;

use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::ShadowProvider;

/// One offer retains the lease epoch that admitted it, never a reacquired epoch.
#[derive(Clone)]
pub(crate) struct Offer {
    pub(crate) provider: ShadowProvider,
    gate: Arc<OwnershipGate>,
    epoch: u64,
}

impl Offer {
    pub(crate) fn begin(provider: ShadowProvider, gate: Arc<OwnershipGate>) -> Option<Self> {
        #[cfg(test)]
        if crate::services::tui_input::transition::mutant("offer_admit") {
            return Some(Self {
                provider,
                gate,
                epoch: 0,
            });
        }
        gate.admit(|epoch| Self {
            provider,
            gate: Arc::clone(&gate),
            epoch,
        })
    }

    /// The closure hands off an effect synchronously; waiting belongs outside admission.
    pub(crate) fn admit<T>(&self, hand_off: impl FnOnce() -> T) -> Option<T> {
        self.gate
            .admit(|epoch| (epoch == self.epoch).then(hand_off))
            .flatten()
    }
}
