//! A reader owns its close; observed EOF alone cannot certify a completed attempt.

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider::ProviderKind;
use crate::services::provider_hosting::ProviderSessionDriver;

pub(crate) fn can_activate(
    provider: ProviderKind,
    driver: ProviderSessionDriver,
    runtime: Option<RuntimeHandoffKind>,
) -> bool {
    driver == ProviderSessionDriver::LegacyPrompt
        && runtime.is_none()
        && matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
}

#[derive(Debug, Clone, Default)]
pub(super) struct Seal {
    claimed: bool,
    pub(super) closed: bool,
    supported: bool,
}

impl Seal {
    pub(super) fn claim(&mut self, supported: bool) -> bool {
        if self.claimed {
            self.supported = false;
            return false;
        }
        self.claimed = true;
        self.supported = supported;
        true
    }

    pub(super) fn complete(&self) -> bool {
        self.claimed && self.closed && self.supported
    }

    #[cfg(test)]
    pub(super) fn fixture() -> Self {
        Self {
            claimed: true,
            closed: true,
            supported: true,
        }
    }
}
