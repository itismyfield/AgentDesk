//! Exact prior-process Herdr row clear after notice, O evidence, or a durable body archive.

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
        self.clear_restarted_herdr(provider, current_generation, silent, None)
    }

    pub(in crate::services::discord) fn clear_restarted_herdr_body(
        &self,
        provider: &ProviderKind,
        current_generation: u64,
        debt: &super::super::herdr_body_debt::Archived,
    ) -> GuardedClearOutcome {
        self.clear_restarted_herdr(provider, current_generation, false, Some(debt))
    }

    fn clear_restarted_herdr(
        &self,
        provider: &ProviderKind,
        current_generation: u64,
        silent: bool,
        debt: Option<&super::super::herdr_body_debt::Archived>,
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
        if raw_before_start_receipt(&data) {
            return GuardedClearOutcome::IoError;
        }
        let Ok(fresh) = serde_json::from_str::<InflightTurnState>(&data) else {
            return GuardedClearOutcome::Missing;
        };
        if !matches_restarted(self, &fresh, current_generation) {
            return GuardedClearOutcome::UserMsgMismatch;
        }
        if let Some(debt) = debt {
            if !debt_eligible(&fresh) || debt.verify(provider, &data).is_err() {
                return GuardedClearOutcome::IoError;
            }
        }
        if silent && o_still_owns(&fresh) != Ok(true) {
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
        let removed = super::remove_row(&path).unwrap_or(GuardedClearOutcome::IoError);
        if debt.is_some() && removed == GuardedClearOutcome::Cleared {
            super::super::super::runtime_store::sync_removed(&path);
        }
        removed
    }
}

/// A silent clear never falls back to a notice: an O owner that changed or does not read keeps it.
fn o_still_owns(
    fresh: &InflightTurnState,
) -> Result<bool, crate::services::tui_o::cutover::IdentityError> {
    let peek = crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel;
    peek(fresh.channel_id, fresh.runtime_kind)
}

fn debt_eligible(row: &InflightTurnState) -> bool {
    use crate::services::tui_o::cutover::IdentityError::{AdoptionRefused, AwaitingAdoption};
    matches!(
        o_still_owns(row),
        Ok(true) | Err(AwaitingAdoption | AdoptionRefused)
    )
}

impl InflightTurnState {
    /// Snapshots only this exact restarted row while its canonical file lock remains held.
    pub(in crate::services::discord) fn archive_restarted_herdr_body(
        &self,
        provider: &ProviderKind,
        current: u64,
        evidence: serde_json::Value,
        recovery_notice: String,
    ) -> Result<super::super::herdr_body_debt::Archived, String> {
        let root = inflight_runtime_root().ok_or("body archive has no inflight root")?;
        let path = inflight_state_path(&root, provider, self.channel_id);
        let lock = lock_inflight_state_path(&path)?;
        let result = (|| {
            let data = fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let fresh: InflightTurnState =
                serde_json::from_str(&data).map_err(|e| e.to_string())?;
            if raw_before_start_receipt(&data)
                || !matches_restarted(self, &fresh, current)
                || !debt_eligible(&fresh)
            {
                return Err("body archive episode or output observation changed".into());
            }
            super::super::herdr_body_debt::archive(
                provider,
                &fresh,
                &data,
                evidence,
                recovery_notice,
            )
        })();
        drop(lock);
        result
    }
}

fn matches_restarted(
    expected_row: &InflightTurnState,
    fresh: &InflightTurnState,
    current_generation: u64,
) -> bool {
    let expected = InflightTurnIdentity::from_state(expected_row);
    !(fresh.herdr_before_start_stop.is_some()
        || expected_row.turn_nonce.as_deref().is_none_or(str::is_empty)
        || fresh.turn_nonce != expected_row.turn_nonce
        || fresh.save_generation != expected_row.save_generation
        || !InflightEpisodePin::from_state(expected_row).matches_state(fresh)
        || expected.is_unnameable()
        || !expected.matches_state(fresh)
        || super::reconcile_gate::row_is_current_generation(&fresh, current_generation)
        || fresh.tui_terminal_kind.is_some()
        || fresh.output_path.as_deref().is_some_and(|p| !p.is_empty())
        || fresh.restart_mode == Some(InflightRestartMode::HotSwapHandoff)
        || fresh.rebind_origin
        || fresh.replay_rerun_blocked())
}
