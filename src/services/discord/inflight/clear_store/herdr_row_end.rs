//! Exact clear of a prior process's path-less Herdr row once recovery has shown its end notice.

use super::*;

impl InflightTurnState {
    /// Clears this row only while the locked fresh row is still this episode from a prior process,
    /// path-less and without kind or receipt; a planned drain restart does not keep it. A `silent`
    /// clear, with no notice sent, also needs O to still own the output under the row's lock.
    pub(in crate::services::discord) fn clear_restarted_herdr_row(
        &self,
        provider: &ProviderKind,
        current_generation: u64,
        silent: bool,
    ) -> GuardedClearOutcome {
        let Some(root) = inflight_runtime_root() else {
            return GuardedClearOutcome::Missing;
        };
        let path = inflight_state_path(&root, provider, self.channel_id);
        let Ok(lock) = lock_inflight_state_path(&path) else {
            return GuardedClearOutcome::IoError;
        };
        let Ok(data) = fs::read_to_string(&path) else {
            return GuardedClearOutcome::Missing;
        };
        // A receipt written after recovery chose the row belongs to receipt settlement.
        if receipt_rechecked() && raw_before_start_receipt(&data) {
            return GuardedClearOutcome::IoError;
        }
        let Ok(fresh) = serde_json::from_str::<InflightTurnState>(&data) else {
            return GuardedClearOutcome::Missing;
        };
        let expected = InflightTurnIdentity::from_state(self);
        let receipt = fresh.herdr_before_start_stop.is_some() && receipt_rechecked();
        if receipt
            || self.turn_nonce.as_deref().is_none_or(str::is_empty)
            || fresh.turn_nonce != self.turn_nonce
            || fresh.save_generation != self.save_generation
            || !InflightEpisodePin::from_state(self).matches_state(&fresh)
            || expected.is_unnameable()
            || !expected.matches_state(&fresh)
            || super::reconcile_gate::row_is_current_generation(&fresh, current_generation)
            || fresh.tui_terminal_kind.is_some()
            || fresh.output_path.as_deref().is_some_and(|p| !p.is_empty())
            || fresh.restart_mode == Some(InflightRestartMode::HotSwapHandoff)
            || fresh.rebind_origin
            || fresh.replay_rerun_blocked()
        {
            return GuardedClearOutcome::UserMsgMismatch;
        }
        if silent && !o_still_owns(&fresh) {
            return GuardedClearOutcome::UserMsgMismatch;
        }
        // Pending circuit evidence outlives the row, as the plain clear keeps it.
        if handoff_before_discard_locked(&lock).is_err() {
            return GuardedClearOutcome::IoError;
        }
        log_inflight_remove(
            provider,
            self.channel_id,
            fresh.user_msg_id,
            "clear_restarted_herdr_row",
            &path,
        );
        super::remove_row(&path).unwrap_or(GuardedClearOutcome::IoError)
    }
}

/// A silent clear never falls back to a notice: an O owner that changed or does not read keeps it.
fn o_still_owns(fresh: &InflightTurnState) -> bool {
    let peek = crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel;
    peek(fresh.channel_id, fresh.runtime_kind) == Ok(true)
}

/// The receipt recheck, which only a test build's mutant turns off.
fn receipt_rechecked() -> bool {
    !cfg!(test) || std::env::var("ADK_HERDR_ROW_END_MUTANT").as_deref() != Ok("receipt")
}
