use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::services::discord) enum CompletionAdmissionPlan {
    Immediate,
    AfterTerminalProjectionSettled,
    AfterTerminalProjectionAndDispositionSettled,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct CompletionAdmission {
    pub(super) plan: CompletionAdmissionPlan,
    pub(super) mailbox_released: bool,
    pub(super) terminal_projection_settled: bool,
    pub(super) terminal_projection_allows_queue: bool,
    pub(super) terminal_disposition_settled: bool,
    pub(super) terminal_disposition_allows_queue: bool,
    pub(super) queue_eligible_published: bool,
}

impl CompletionAdmission {
    pub(super) fn new(plan: CompletionAdmissionPlan) -> Self {
        Self {
            plan,
            mailbox_released: false,
            terminal_projection_settled: false,
            terminal_projection_allows_queue: false,
            terminal_disposition_settled: false,
            terminal_disposition_allows_queue: false,
            queue_eligible_published: false,
        }
    }

    pub(super) fn update_plan(&mut self, plan: CompletionAdmissionPlan) {
        if plan > self.plan {
            self.plan = plan;
        }
    }

    pub(super) fn note_mailbox_released(&mut self) {
        self.mailbox_released = true;
    }

    pub(super) fn note_terminal_projection_settled(&mut self, allow_queue: bool) {
        if self.terminal_projection_settled {
            return;
        }
        self.terminal_projection_settled = true;
        self.terminal_projection_allows_queue = allow_queue;
    }

    pub(super) fn note_terminal_disposition_settled(&mut self, allow_queue: bool) {
        if self.terminal_disposition_settled {
            return;
        }
        self.terminal_disposition_settled = true;
        self.terminal_disposition_allows_queue = allow_queue;
    }

    pub(super) fn claim_queue_eligible(&mut self) -> bool {
        let barrier_satisfied = self.mailbox_released
            && match self.plan {
                CompletionAdmissionPlan::Immediate => true,
                CompletionAdmissionPlan::AfterTerminalProjectionSettled => {
                    self.terminal_projection_settled && self.terminal_projection_allows_queue
                }
                CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled => {
                    self.terminal_projection_settled
                        && self.terminal_projection_allows_queue
                        && self.terminal_disposition_settled
                        && self.terminal_disposition_allows_queue
                }
            };
        if !barrier_satisfied || self.queue_eligible_published {
            return false;
        }
        self.queue_eligible_published = true;
        true
    }
}

pub(super) fn publish_claimed_queue_eligible(shared: &SharedData, entry: &mut LedgerEntry) -> bool {
    if !entry.completion_admission.claim_queue_eligible() {
        return false;
    }
    super::super::turn_completion_events::publish_queue_eligible_completion_event(
        shared,
        entry.turn_key.channel_id,
        Some(entry.turn_key.user_msg_id),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_candidate_releases_without_busy_outcome_after_projection_settles_4888() {
        let mut admission =
            CompletionAdmission::new(CompletionAdmissionPlan::AfterTerminalProjectionSettled);
        admission.note_mailbox_released();
        assert!(!admission.claim_queue_eligible());
        admission.note_terminal_projection_settled(true);
        assert!(admission.claim_queue_eligible());
    }

    #[test]
    fn deferred_resume_failure_settle_first_releases_when_mailbox_arrives_4888() {
        let mut admission =
            CompletionAdmission::new(CompletionAdmissionPlan::AfterTerminalProjectionSettled);
        admission.note_terminal_projection_settled(true);
        assert!(!admission.claim_queue_eligible());
        admission.note_mailbox_released();
        assert!(admission.claim_queue_eligible());
    }

    #[test]
    fn no_projection_path_releases_after_explicit_settle_4888() {
        let mut admission =
            CompletionAdmission::new(CompletionAdmissionPlan::AfterTerminalProjectionSettled);
        admission.note_mailbox_released();
        assert!(!admission.claim_queue_eligible());
        admission.note_terminal_projection_settled(true);
        assert!(admission.claim_queue_eligible());
    }

    #[test]
    fn terminal_projection_success_or_failure_share_one_settled_edge_4888() {
        for _delivery_succeeded in [true, false] {
            let mut admission =
                CompletionAdmission::new(CompletionAdmissionPlan::AfterTerminalProjectionSettled);
            admission.note_mailbox_released();
            admission.note_terminal_projection_settled(true);
            assert!(admission.claim_queue_eligible());
            assert!(!admission.claim_queue_eligible());
            admission.note_mailbox_released();
            admission.note_terminal_projection_settled(true);
            assert!(!admission.claim_queue_eligible());
        }
    }

    #[test]
    fn capped_or_failed_retry_settles_without_queue_admission_4888() {
        let mut admission = CompletionAdmission::new(
            CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
        );
        admission.note_mailbox_released();
        admission.note_terminal_projection_settled(true);
        admission.note_terminal_disposition_settled(false);
        assert!(admission.terminal_projection_settled);
        assert!(admission.terminal_disposition_settled);
        assert!(!admission.claim_queue_eligible());
    }

    #[test]
    fn watcher_projection_cannot_publish_before_bridge_disposition_4888() {
        let mut admission = CompletionAdmission::new(
            CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
        );
        admission.note_mailbox_released();
        admission.note_terminal_projection_settled(true);
        assert!(!admission.claim_queue_eligible());
        admission.note_terminal_disposition_settled(false);
        assert!(!admission.claim_queue_eligible());
    }

    #[test]
    fn normal_bridge_disposition_releases_after_watcher_projection_4888() {
        let mut admission = CompletionAdmission::new(
            CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
        );
        admission.note_mailbox_released();
        admission.note_terminal_projection_settled(true);
        admission.note_terminal_disposition_settled(true);
        assert!(admission.claim_queue_eligible());
    }

    #[test]
    fn immediate_refresh_cannot_downgrade_deferred_admission_4888() {
        let mut admission =
            CompletionAdmission::new(CompletionAdmissionPlan::AfterTerminalProjectionSettled);
        admission.update_plan(CompletionAdmissionPlan::Immediate);
        admission.note_mailbox_released();
        assert!(!admission.claim_queue_eligible());
        admission.note_terminal_projection_settled(true);
        assert!(admission.claim_queue_eligible());
    }
}
