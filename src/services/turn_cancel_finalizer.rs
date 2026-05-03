use chrono::{DateTime, Utc};
use poise::serenity_prelude::ChannelId;

use crate::services::observability::turn_lifecycle::TurnCancellationDetails;
use crate::services::provider::ProviderKind;
use crate::services::turn_lifecycle::TurnLifecycleStopResult;

pub(crate) const CANCELLED_TURN_STATUS: &str = "cancelled";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TurnCancelCorrelation {
    pub provider: Option<ProviderKind>,
    pub channel_id: Option<ChannelId>,
    pub dispatch_id: Option<String>,
    pub session_key: Option<String>,
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct FinalizeTurnCancelRequest {
    pub correlation: TurnCancelCorrelation,
    pub reason: String,
    pub surface: String,
    pub lifecycle_path: String,
    pub tmux_killed: bool,
    pub inflight_cleared: bool,
    pub queue_depth: Option<usize>,
    pub queue_preserved: bool,
    pub termination_recorded: bool,
    pub completed_at: DateTime<Utc>,
}

impl FinalizeTurnCancelRequest {
    pub(crate) fn from_lifecycle_result(
        correlation: TurnCancelCorrelation,
        reason: impl Into<String>,
        surface: impl Into<String>,
        result: &TurnLifecycleStopResult,
    ) -> Self {
        Self {
            correlation,
            reason: reason.into(),
            surface: surface.into(),
            lifecycle_path: result.lifecycle_path.to_string(),
            tmux_killed: result.tmux_killed,
            inflight_cleared: result.inflight_cleared,
            queue_depth: result.queue_depth,
            queue_preserved: result.queue_preserved,
            termination_recorded: result.termination_recorded,
            completed_at: Utc::now(),
        }
    }

    #[cfg(test)]
    fn with_completed_at(mut self, completed_at: DateTime<Utc>) -> Self {
        self.completed_at = completed_at;
        self
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct FinalizedTurnCancel {
    pub status: &'static str,
    pub completed_at: DateTime<Utc>,
    pub details: TurnCancellationDetails,
    pub correlation: TurnCancelCorrelation,
}

pub(crate) fn finalize_turn_cancel(request: FinalizeTurnCancelRequest) -> FinalizedTurnCancel {
    let details = TurnCancellationDetails::new(
        &request.reason,
        &request.surface,
        &request.lifecycle_path,
        request.tmux_killed,
        request.inflight_cleared,
        request.queue_depth,
        request.queue_preserved,
        request.termination_recorded,
    );

    crate::services::observability::emit_turn_cancelled(
        request
            .correlation
            .provider
            .as_ref()
            .map(ProviderKind::as_str),
        request
            .correlation
            .channel_id
            .map(|channel_id| channel_id.get()),
        request.correlation.dispatch_id.as_deref(),
        request.correlation.session_key.as_deref(),
        request.correlation.turn_id.as_deref(),
        details.clone(),
    );

    FinalizedTurnCancel {
        status: CANCELLED_TURN_STATUS,
        completed_at: request.completed_at,
        details,
        correlation: request.correlation,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[tokio::test]
    async fn finalizer_builds_canonical_cancel_record_and_event() {
        let _guard = crate::services::observability::test_runtime_lock();
        crate::services::observability::reset_for_tests();
        crate::services::observability::init_observability(None);

        let lifecycle = TurnLifecycleStopResult {
            lifecycle_path: "mailbox_canonical",
            tmux_killed: false,
            inflight_cleared: true,
            queue_depth: Some(3),
            queue_preserved: true,
            termination_recorded: true,
        };
        let completed_at = Utc.with_ymd_and_hms(2026, 5, 3, 9, 30, 0).unwrap();
        let result = finalize_turn_cancel(
            FinalizeTurnCancelRequest::from_lifecycle_result(
                TurnCancelCorrelation {
                    provider: Some(ProviderKind::Codex),
                    channel_id: Some(ChannelId::new(1479671301387059200)),
                    dispatch_id: Some("dispatch-1633".to_string()),
                    session_key: Some("codex/session".to_string()),
                    turn_id: Some("turn-1633".to_string()),
                },
                " operator stop ",
                " text_stop ",
                &lifecycle,
            )
            .with_completed_at(completed_at),
        );

        assert_eq!(result.status, CANCELLED_TURN_STATUS);
        assert_eq!(result.completed_at, completed_at);
        assert_eq!(result.details.reason, "operator stop");
        assert_eq!(result.details.surface, "text_stop");
        assert_eq!(result.details.lifecycle_path, "mailbox_canonical");
        assert_eq!(result.details.queue_depth, Some(3));
        assert_eq!(
            result.correlation.dispatch_id.as_deref(),
            Some("dispatch-1633")
        );

        let event = crate::services::observability::events::recent(10)
            .into_iter()
            .find(|event| event.event_type == "turn_cancelled")
            .expect("turn_cancelled event should be recorded");
        assert_eq!(event.channel_id, Some(1479671301387059200));
        assert_eq!(event.provider.as_deref(), Some("codex"));
        assert_eq!(event.payload["reason"], "operator stop");
        assert_eq!(event.payload["surface"], "text_stop");
        assert_eq!(event.payload["lifecyclePath"], "mailbox_canonical");
        assert_eq!(event.payload["inflightCleared"], true);
        assert_eq!(event.payload["queueDepth"], 3);
        assert_eq!(event.payload["queuePreserved"], true);
        assert_eq!(event.payload["terminationRecorded"], true);
        assert_eq!(event.payload["dispatch_id"], "dispatch-1633");
        assert_eq!(event.payload["session_key"], "codex/session");
        assert_eq!(event.payload["turn_id"], "turn-1633");
    }
}
