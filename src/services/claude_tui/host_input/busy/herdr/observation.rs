use super::*;

impl Context {
    pub(in crate::services::claude_tui::host_input::busy) fn read(&self) -> Reading {
        if !self.target.execution_alive() {
            return Reading::held();
        }
        if self.cleared() || self.idle_ready() {
            return Reading::idle();
        }
        if !(self.output_ready)(self.channel)
            || !matches!(self.target.capture_ansi(-80), Some(capture) if !busy_inject::modal(&capture))
        {
            return Reading::held();
        }
        crate::services::tmux_common::try_with_tmux_source_authority(&self.logical, |authority| {
            // A clear delegates before a new file exists; no pane socket is read under this lock.
            if self.cleared() {
                return Some(Reading::idle());
            }
            self.transcript(|| self.current_source(authority))
        })
        .flatten()
        .unwrap_or_else(Reading::held)
    }

    #[cfg(test)]
    pub(crate) fn read_host(&self, output_ready: bool) -> Reading {
        if !output_ready || !self.target.execution_alive() {
            return Reading::held();
        }
        crate::services::tmux_common::try_with_tmux_source_authority(&self.logical, |authority| {
            self.transcript(|| {
                let (pin, history) = binding_events::published_claude_history(
                    self.channel,
                    &self.logical,
                    &self.nonce,
                )?;
                self.resolve_source(authority, pin, history)
            })
        })
        .flatten()
        .unwrap_or_else(Reading::held)
    }

    fn transcript(&self, current: impl Fn() -> Option<Source>) -> Option<Reading> {
        use crate::services::tui_turn_state::{
            TuiTurnState, observe_claude_jsonl_running_turn_state,
        };

        let source = current()?;
        let state = observe_claude_jsonl_running_turn_state(&source.id.path);
        if current().is_none_or(|again| again.id != source.id) {
            return None;
        }
        let state = match state {
            TuiTurnState::Idle => BusyState::Idle,
            state if state.is_busy() => BusyState::Busy,
            _ => BusyState::Held,
        };
        Some(Reading {
            state,
            source: Some(source),
            idle_verified: false,
        })
    }

    // This only delegates; the Idle driver reserves and checks again before its first key.
    fn idle_ready(&self) -> bool {
        use crate::services::claude_tui::readiness;
        use crate::services::tui_prompt_dedupe::binding_context::{
            SpawnNonceMarker, observe_spawn_nonce_marker,
        };
        let current = || {
            self.target.execution_alive()
                && matches!(observe_spawn_nonce_marker(&self.logical),
                    SpawnNonceMarker::Known(nonce) if nonce == self.nonce)
        };
        if !current() {
            return false;
        }
        let Some(binding) = tui_prompt_dedupe::runtime_binding_for_tmux_session(&self.logical)
            .filter(|binding| binding.runtime_kind == RuntimeHandoffKind::ClaudeTui)
        else {
            return false;
        };
        let path = Some(std::path::Path::new(&binding.output_path));
        let Some(proof) = readiness::observe(&self.logical, path)
            .or_else(|| readiness::observe_restored_reply(&self.logical, path))
        else {
            return false;
        };
        if proof.restart()
            && !self
                .target
                .capture_ansi(-80)
                .as_deref()
                .is_some_and(crate::services::claude::herdr_turn::composer::restored_screen_idle)
        {
            return false;
        }
        let Some(reservation) = readiness::reserve(proof) else {
            return false;
        };
        let valid = reservation.current()
            && current()
            && tui_prompt_dedupe::runtime_binding_for_tmux_session(&self.logical).is_some_and(
                |present| {
                    present.runtime_kind == binding.runtime_kind
                        && present.output_path == binding.output_path
                },
            );
        reservation.not_sent();
        valid
    }

    fn cleared(&self) -> bool {
        crate::services::claude::herdr_turn::awaited_clear(self.channel, &self.logical, &self.nonce)
            .is_some()
    }
}
