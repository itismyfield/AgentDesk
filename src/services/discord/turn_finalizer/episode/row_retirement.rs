//! Retires this episode's own row whose finalizer id drifted from the mailbox key, only when the
//! claim just released the exact token it observed and the row names the same nonce and runtime.

use super::super::super::inflight::{GuardedClearOutcome, InflightTurnState};
use super::*;

/// Runtime the caller saw for the episode; at least one axis must be present to retire.
#[derive(Clone, Copy, Default)]
pub(in crate::services::discord) struct RuntimeWitness<'a> {
    /// The tmux session the stale-busy probes proved gone.
    pub(in crate::services::discord) probed_tmux: Option<&'a str>,
    /// The producer's claim snapshot of the row it settles.
    pub(in crate::services::discord) claim_snapshot: Option<&'a SyntheticClaimSnapshot>,
}

impl RuntimeWitness<'_> {
    fn matches(&self, row: &InflightTurnState) -> bool {
        if self.probed_tmux.is_none() && self.claim_snapshot.is_none() {
            return false;
        }
        self.probed_tmux
            .is_none_or(|name| row.tmux_session_name.as_deref() == Some(name))
            && self.claim_snapshot.is_none_or(|snapshot| {
                snapshot.turn_nonce == row.turn_nonce
                    && snapshot.tmux_session_name == row.tmux_session_name
                    && snapshot.started_at == row.started_at
                    && snapshot.turn_start_offset == row.turn_start_offset
            })
    }
}

impl TurnFinalizer {
    /// Stale-busy cancel of the observed episode, with the tmux session its probes proved gone.
    pub(in crate::services::discord) async fn submit_stale_busy_cancel(
        &self,
        key: TurnKey,
        provider: ProviderKind,
        turn_nonce: Option<String>,
        probed_tmux_session: String,
        shared: Arc<SharedData>,
    ) -> FinalizeOutcome {
        let evidence = TerminalEvidence {
            episode_captured: true,
            turn_nonce,
            claim_snapshot: None,
            probed_tmux_session: Some(probed_tmux_session),
        };
        let (event, ctx) = (TerminalEvent::Cancel, FinalizeContext::stale_busy_mailbox());
        self.submit_terminal_evidence(key, provider, event, ctx, evidence, shared)
            .await
    }
}

/// Why the released episode does not own `row`; `None` once every proof axis holds.
fn proof_refusal(
    provider: &ProviderKind,
    key: TurnKey,
    observed: Option<&crate::services::turn_orchestrator::ChannelMailboxSnapshot>,
    row: &InflightTurnState,
    finish: &crate::services::turn_orchestrator::FinishTurnResult,
    witness: RuntimeWitness<'_>,
) -> Option<&'static str> {
    let Some(removed) = finish.removed_token.as_ref() else {
        return Some("mailbox_not_released");
    };
    let observed_token = observed.and_then(|snapshot| snapshot.cancel_token.as_ref());
    if !observed_token.is_some_and(|token| Arc::ptr_eq(token, removed)) {
        return Some("actor_changed");
    }
    let Some(nonce) = row.turn_nonce.as_deref().filter(|nonce| !nonce.is_empty()) else {
        return Some("nonce_absent");
    };
    let observed_nonce = observed.and_then(|snapshot| snapshot.active_turn_nonce.as_deref());
    if observed_nonce != Some(nonce)
        || removed.turn_nonce() != Some(nonce)
        || !key.matches_episode_nonce(Some(nonce))
    {
        return Some("nonce_mismatch");
    }
    if row.provider_kind().as_ref() != Some(provider) || row.channel_id != key.channel_id.get() {
        return Some("provider_channel_mismatch");
    }
    if !witness.matches(row) {
        return Some("runtime_identity_mismatch");
    }
    None
}

/// Retires the loaded row through the snapshot CAS once the release proves it is this episode's.
pub(super) fn retire_drifted_row(
    provider: &ProviderKind,
    key: TurnKey,
    observed: Option<&crate::services::turn_orchestrator::ChannelMailboxSnapshot>,
    row: &InflightTurnState,
    finish: &crate::services::turn_orchestrator::FinishTurnResult,
    witness: RuntimeWitness<'_>,
) {
    if let Some(reason) = proof_refusal(provider, key, observed, row, finish, witness) {
        warn_declined(provider, key, row, finish, "proof", reason, None);
        return;
    }
    #[cfg(test)]
    before_cas(key.channel_id.get());
    let (outcome, detail) = GuardedClearOutcome::for_snapshot_with_detail(provider, row);
    match (outcome, detail.reason) {
        (GuardedClearOutcome::Cleared, _) | (GuardedClearOutcome::Missing, "not_found") => {
            tracing::info!(
                provider = provider.as_str(),
                channel_id = key.channel_id.get(),
                user_msg_id = key.user_msg_id,
                row_finalizer_turn_id = row.effective_finalizer_turn_id(),
                nonce_fp = %nonce_fingerprint(row.turn_nonce.as_deref()),
                outcome = ?outcome,
                "episode_row_retired: released episode's drifted row removed"
            );
        }
        _ => {
            let observed = detail.observed_save_generation;
            warn_declined(provider, key, row, finish, "cas", detail.reason, observed);
        }
    }
}

/// The drifted row stays; `reason` names the proof axis or the lock-held CAS check that kept it.
pub(super) fn warn_declined(
    provider: &ProviderKind,
    key: TurnKey,
    row: &InflightTurnState,
    finish: &crate::services::turn_orchestrator::FinishTurnResult,
    gate: &str,
    reason: &str,
    observed_save_generation: Option<u64>,
) {
    tracing::warn!(
        provider = provider.as_str(),
        channel_id = key.channel_id.get(),
        user_msg_id = key.user_msg_id,
        row_finalizer_turn_id = row.effective_finalizer_turn_id(),
        nonce_fp = %nonce_fingerprint(row.turn_nonce.as_deref()),
        gate,
        reason,
        expected_save_generation = row.save_generation,
        observed_save_generation,
        mailbox_released = finish.removed_token.is_some(),
        "episode_row_retirement_declined: drifted row of the released episode kept"
    );
}

/// Finalize without episode evidence no longer unlinks rows by id; name the row it leaves.
pub(in crate::services::discord::turn_finalizer) fn warn_no_episode_evidence(
    provider: &ProviderKind,
    key: TurnKey,
    site: &str,
) {
    let channel_id = key.channel_id.get();
    let row = super::super::super::inflight::load_inflight_state_read_only(provider, channel_id);
    let Some(row) = row
        .filter(|row| key.user_msg_id == 0 || row.effective_finalizer_turn_id() == key.user_msg_id)
    else {
        return;
    };
    tracing::warn!(
        provider = provider.as_str(),
        channel_id,
        user_msg_id = key.user_msg_id,
        row_finalizer_turn_id = row.effective_finalizer_turn_id(),
        site,
        reason = "no_episode_evidence",
        "episode_row_cleanup_skipped: finalize without episode evidence leaves the row"
    );
}

fn nonce_fingerprint(nonce: Option<&str>) -> String {
    let digest = episode_fingerprint(nonce);
    digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
type CasHook = Box<dyn FnOnce() + Send>;
#[cfg(test)]
static BEFORE_CAS: std::sync::Mutex<Vec<(u64, CasHook)>> = std::sync::Mutex::new(Vec::new());

/// Runs `hook` once, between the proven release and the snapshot CAS on `channel`.
#[cfg(test)]
pub(super) fn set_before_cas(channel: u64, hook: CasHook) {
    BEFORE_CAS.lock().unwrap().push((channel, hook));
}

#[cfg(test)]
fn before_cas(channel: u64) {
    let hook = {
        let mut hooks = BEFORE_CAS.lock().unwrap();
        let at = hooks.iter().position(|(id, _)| *id == channel);
        at.map(|at| hooks.remove(at).1)
    };
    if let Some(hook) = hook {
        hook();
    }
}
