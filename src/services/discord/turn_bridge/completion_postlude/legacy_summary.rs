use super::*;
use crate::services::discord::turn_end_summary::{LiveSummary, outcome, post_live};

pub(super) struct Completion {
    shared: Arc<SharedData>,
    gateway: Arc<dyn TurnGateway>,
    provider: ProviderKind,
    summary: LiveSummary,
    terminal_committed: bool,
}

#[cfg(test)]
#[path = "legacy_summary_tests.rs"]
mod tests;

impl Completion {
    pub(super) fn capture(
        ctx: &CompletionPostludeContext,
        state: &CompletionPostludeState,
    ) -> Self {
        Self {
            shared: ctx.shared_owned.clone(),
            gateway: ctx.gateway.clone(),
            provider: ctx.provider.clone(),
            summary: LiveSummary::bridge(
                ctx.channel_id,
                &state.inflight_state,
                ctx.shared_owned.restart.current_generation,
                outcome(state.cancelled, state.provider_failure),
                u64::try_from(turn_duration_ms(ctx.turn_start)).ok(),
            ),
            terminal_committed: state.terminal_delivery_committed,
        }
    }

    pub(super) async fn complete(
        self,
        projection: impl std::future::Future<Output = bool>,
        permitted: impl std::future::Future<Output = bool>,
    ) -> bool {
        let summary = self.summary.with_context(&self.shared);
        let committed = projection.await;
        if self.terminal_committed && permitted.await {
            post_live(self.gateway.as_ref(), &self.provider, summary).await;
        }
        committed
    }
}
