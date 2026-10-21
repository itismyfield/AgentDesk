use super::*;

impl TurnLifecycleStopResult {
    /// A kept stop leaves further cleanup and state changes to the current owner.
    pub(crate) fn host_guard_kept(&self) -> bool {
        matches!(
            self.lifecycle_path,
            HOST_GUARD_KEPT_PATH
                | "stop-unobserved-kept"
                | "actor-unreachable-kept"
                | "native-stop-pending"
                | "native-stop-unconfirmed"
                | "native-stop-completed"
        )
    }

    pub(crate) fn refusal_reason(&self) -> &'static str {
        match self.lifecycle_path {
            "actor-unreachable-kept" => "actor_unreachable",
            "stop-unobserved-kept" => "stop_unobserved",
            "native-stop-pending" => "native_stop_pending",
            "native-stop-unconfirmed" => "native_stop_unconfirmed",
            "native-stop-completed" => "native_stop_completed",
            _ => "session host is not legacy tmux",
        }
    }

    pub(crate) fn queue_depth_if_observed(&self) -> Option<usize> {
        self.queue_depth
            .filter(|_| self.queue_depth_after.is_some())
    }
}

pub(super) fn native_preserved(
    runtime: crate::services::discord::health::RuntimeTurnStopResult,
    observed: Option<String>,
) -> TurnLifecycleStopResult {
    TurnLifecycleStopResult {
        lifecycle_path: runtime.lifecycle_path,
        queue_depth: Some(runtime.queue_depth),
        mailbox_foreground_free: Some(runtime.mailbox_foreground_free),
        ..kept_by_host_guard(observed)
    }
}
