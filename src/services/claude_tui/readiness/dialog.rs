//! Existing startup actions stay separate from transcript readiness evidence.
use super::super::input::{self, PromptReadinessKind};
use super::super::startup_dialog::{
    StartupDialogPlan, detect_claude_startup_dialog, plan_startup_dialog_response,
};
use crate::services::provider::CancelToken;
use std::time::{Duration, Instant};

const MAX_DISMISSALS: usize = 5;
const SETTLE: Duration = Duration::from_millis(500);

pub(super) struct DialogWait {
    readiness: PromptReadinessKind,
    attempts: usize,
    not_before: Option<Instant>,
}
impl DialogWait {
    pub(super) fn new(readiness: PromptReadinessKind) -> Self {
        Self {
            readiness,
            attempts: 0,
            not_before: None,
        }
    }
    pub(super) fn poll(
        &mut self,
        session: &str,
        cancel: Option<&CancelToken>,
    ) -> Result<bool, String> {
        if self.readiness == PromptReadinessKind::ProvenWarmFollowup {
            return Ok(false);
        }
        if self.not_before.is_some_and(|at| Instant::now() < at) {
            return Ok(true);
        }
        let snapshot = input::prompt_readiness_snapshot(session);
        let Some(dialog) = detect_claude_startup_dialog(&snapshot.pane_tail) else {
            return Ok(false);
        };
        match plan_startup_dialog_response(&dialog) {
            StartupDialogPlan::FailUntrustedWorkspace { workspace } => Err(format!(
                "claude tui startup blocked by workspace trust dialog for untrusted path '{workspace}'; refusing to auto-trust; fix the agent/channel workspace mapping so claude does not spawn there"
            )),
            StartupDialogPlan::DismissWithEnter => {
                if self.attempts < MAX_DISMISSALS {
                    self.attempts += 1;
                    tracing::info!(
                        session,
                        dialog = dialog.label(),
                        attempt = self.attempts,
                        "dismissing approved Claude startup dialog"
                    );
                    input::dismiss_startup_dialog(session, self.readiness, &dialog, cancel)?;
                    self.not_before = Some(Instant::now() + SETTLE);
                }
                Ok(true)
            }
        }
    }
}
