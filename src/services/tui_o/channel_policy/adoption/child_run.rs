//! A verified late child's refused run stays Legacy-owned until its next execution boundary.
use super::*;
use crate::services::tui_o::writer::{adoption::LegacyTurnIdentity, binding::BindingEvent};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyRun {
    pub binding: BindingEvent,
    pub turn: Option<LegacyTurnIdentity>,
    pub(crate) observed_end: u64,
    pub delivery: Option<crate::services::tui_o::writer::adoption::LegacyDelivery>,
}
#[derive(Debug)]
pub(super) struct RunState {
    run: Option<LegacyRun>,
    owed: Vec<LegacyRun>,
    boundary: Option<LateBoundary>,
    epoch: u64,
    changed: tokio::sync::watch::Sender<u64>,
}
impl Default for RunState {
    fn default() -> Self {
        Self {
            run: None,
            owed: Vec::new(),
            boundary: None,
            epoch: 0,
            changed: tokio::sync::watch::channel(0).0,
        }
    }
}
impl Candidate {
    pub(crate) fn owed_runs(&self) -> Vec<LegacyRun> {
        self.child_run
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .owed
            .clone()
    }
    pub fn child_boundary_epoch(&self) -> u64 {
        self.child_run
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .epoch
    }
    pub(crate) fn child_boundary_notice(&self) -> tokio::sync::watch::Receiver<u64> {
        self.child_run
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .changed
            .subscribe()
    }

    pub fn legacy_run(&self) -> Option<LegacyRun> {
        self.child_run
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .run
            .clone()
    }
    pub fn child_parked(&self) -> bool {
        self.child_parked_in(self.peek())
    }
    pub(crate) fn child_parked_in(&self, state: Adoption) -> bool {
        self.late_origin
            && self.child_surface().is_some()
            && state == Adoption::Released
            && self.legacy_run().is_some()
    }
    pub(super) fn child_retry_boundary(&self) -> Option<LateBoundary> {
        self.child_run
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .boundary
    }
    /// Only a source-bound refusal under the parent's boot kind grants this exception.
    pub(crate) fn release_child_run(&self, channel: u64, run: LegacyRun) -> bool {
        use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, CodexTui};
        use crate::services::tui_o::shadow::ShadowProvider;
        let kind = match run.binding.provider {
            ShadowProvider::Claude => ClaudeTui,
            ShadowProvider::Codex => CodexTui,
            ShadowProvider::Antigravity => return false,
        };
        let Some(surface) = self.child_surface() else {
            return false;
        };
        let Ok(scope) = self.init_scope(channel) else {
            return false;
        };
        if !self.late_origin
            || !matches!(
                &run.binding.record,
                crate::services::tui_o::writer::binding::BindingRecord::Bound {
                    new: crate::services::tui_o::writer::binding::BindingTarget::Source(_),
                    ..
                } | crate::services::tui_o::writer::binding::BindingRecord::Resolved { .. }
            )
            || run.binding.channel_id != channel
            || run.binding.tmux_session.is_empty()
            || run
                .turn
                .as_ref()
                .and_then(|turn| turn.2.as_deref())
                .is_some_and(|tmux| tmux != run.binding.tmux_session)
            || !surface.matches(channel, surface.parent(), Some(kind))
            || !scope.configured(surface.parent())
            || scope.kind(surface.parent()) != Some(kind)
        {
            return false;
        }
        let mut state = self.lock();
        if *state != Adoption::Deferred {
            return false;
        }
        self.child_run
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .run = Some(run);
        *state = Adoption::Released;
        self.settle_first();
        tracing::info!(channel, "[tui_o] existing child run stays with Legacy");
        true
    }
    /// Keep every Legacy send fenced while replacing only this run's adoption floor.
    pub(crate) fn retry_child_at_boundary(&self, expected: &LegacyRun, seq: u64) -> bool {
        let mut state = self.lock();
        let mut child = self
            .child_run
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let sends = self.sends();
        if *state != Adoption::Released
            || child.run.as_ref() != Some(expected)
            || sends.0 != sends.1
        {
            return false;
        }
        child.boundary = Some(LateBoundary {
            baseline: seq,
            takeover_after: seq,
            sends,
        });
        if !child.owed.contains(expected) {
            child.owed.push(expected.clone());
        }
        child.run = None;
        child.epoch += 1;
        child.changed.send_replace(child.epoch);
        *self
            .turn_mark
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        *state = Adoption::Deferred;
        true
    }
}

#[cfg(test)]
#[path = "child_run_tests.rs"]
mod tests;
