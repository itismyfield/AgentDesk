//! Results from canonical provider runtime stop and repair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeTurnStopResult {
    pub lifecycle_path: &'static str,
    pub had_active_turn: bool,
    pub queue_depth: usize,
    pub inflight: InflightDisposition,
    pub termination_recorded: bool,
    /// #5176 — whether this stop actually took the mailbox foreground anchor.
    /// `true` also covers "the mailbox was already free when we checked": the
    /// contract this field reports is *ownership released*, and the caller only
    /// needs to know whether the channel is still locked.
    pub mailbox_foreground_free: bool,
}

impl RuntimeTurnStopResult {
    pub(crate) fn native_preserved(
        path: &'static str,
        queue_depth: usize,
        mailbox_foreground_free: bool,
    ) -> Self {
        Self {
            lifecycle_path: path,
            had_active_turn: true,
            queue_depth,
            inflight: InflightDisposition::PreservedByNativeStop,
            termination_recorded: false,
            mailbox_foreground_free,
        }
    }

    /// The host guard kept the judged turn: nothing was cancelled, finished or cleared.
    pub(crate) fn preserved_by_host_guard(queue_depth: usize) -> Self {
        Self {
            lifecycle_path: "host-guard-preserved",
            had_active_turn: true,
            queue_depth,
            inflight: InflightDisposition::PreservedByHostGuard,
            termination_recorded: false,
            mailbox_foreground_free: false,
        }
    }

    /// The judged turn ended and another took the channel: the stop left the successor alone.
    pub(crate) fn token_superseded(queue_depth: usize, termination_recorded: bool) -> Self {
        Self {
            lifecycle_path: TOKEN_SUPERSEDED_PATH,
            inflight: InflightDisposition::NotNeeded,
            termination_recorded,
            ..Self::preserved_by_host_guard(queue_depth)
        }
    }

    /// The finish went unobserved: the turn, its row and session are kept as a refused host's.
    pub(crate) fn finish_unobserved(queue_depth: usize) -> Self {
        Self {
            lifecycle_path: FINISH_UNOBSERVED_PATH,
            inflight: InflightDisposition::PreservedByHostGuard,
            ..Self::preserved_by_host_guard(queue_depth)
        }
    }
}

/// A stop whose judged turn was replaced before its finish; the channel stays locked.
pub const TOKEN_SUPERSEDED_PATH: &str = "token-superseded";
/// A stop whose finish the channel's actor did not answer; nothing was cleared.
pub const FINISH_UNOBSERVED_PATH: &str = "finish-unobserved";

/// What a runtime stop did with the channel's persistent inflight row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InflightDisposition {
    Cleared,
    NotNeeded,
    /// The stop was not a legacy one: the row stays for the turn's owner, and no caller clears it.
    PreservedByHostGuard,
    /// A native stop has not published cancellation; its reader and row remain owned.
    PreservedByNativeStop,
}

impl InflightDisposition {
    pub(crate) fn cleared_if(cleared: bool) -> Self {
        if cleared {
            Self::Cleared
        } else {
            Self::NotNeeded
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kept_stop_results_keep_their_path_and_disposition() {
        let kept = RuntimeTurnStopResult::preserved_by_host_guard(3);
        assert_eq!(
            kept,
            RuntimeTurnStopResult {
                lifecycle_path: "host-guard-preserved",
                had_active_turn: true,
                queue_depth: 3,
                inflight: InflightDisposition::PreservedByHostGuard,
                termination_recorded: false,
                mailbox_foreground_free: false,
            }
        );
        assert_eq!(
            RuntimeTurnStopResult::finish_unobserved(3),
            RuntimeTurnStopResult {
                lifecycle_path: "finish-unobserved",
                ..kept
            }
        );
        for termination_recorded in [false, true] {
            assert_eq!(
                RuntimeTurnStopResult::token_superseded(3, termination_recorded),
                RuntimeTurnStopResult {
                    lifecycle_path: "token-superseded",
                    inflight: InflightDisposition::NotNeeded,
                    termination_recorded,
                    ..kept
                }
            );
        }
    }

    #[test]
    fn native_stop_reports_release_without_granting_termination() {
        for mailbox_foreground_free in [false, true] {
            assert_eq!(
                RuntimeTurnStopResult::native_preserved(
                    "native-stop-pending",
                    4,
                    mailbox_foreground_free
                ),
                RuntimeTurnStopResult {
                    lifecycle_path: "native-stop-pending",
                    had_active_turn: true,
                    queue_depth: 4,
                    inflight: InflightDisposition::PreservedByNativeStop,
                    termination_recorded: false,
                    mailbox_foreground_free,
                }
            );
        }
        assert_eq!(
            InflightDisposition::cleared_if(true),
            InflightDisposition::Cleared
        );
        assert_eq!(
            InflightDisposition::cleared_if(false),
            InflightDisposition::NotNeeded
        );
    }
}
