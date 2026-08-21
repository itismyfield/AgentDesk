//! Generation fence for boot/startup reconcile row removal (#5462 S2).
//!
//! Reconcile callers must not unlink a row authored by the running process. The
//! root-explicit helpers below keep the decisive read, generation check, identity
//! check, and unlink inside one inflight sidecar flock critical section.

use super::*;

/// Result of a reconcile-owned clear attempt. This is deliberately separate
/// from [`GuardedClearOutcome`] so the normal turn-owner cleanup contract does
/// not gain a reconcile-only variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum ReconcileClearOutcome {
    /// The locked, freshly-read row was authored by the running process.
    LiveGenerationSkipped,
    /// The generation fence allowed delegation to the ordinary guarded clear.
    Delegated(GuardedClearOutcome),
}

/// The generation fence is fail-open for legacy rows (`born_generation == 0`).
pub(in crate::services::discord) fn row_is_current_generation(
    state: &InflightTurnState,
    current_generation: u64,
) -> bool {
    state.born_generation != 0 && state.born_generation == current_generation
}

/// Which population a row that the fence ALLOWED belongs to (#5462 S5 §7.2-2).
///
/// The allow counter has to be a distribution: the fence refuses exactly one
/// bucket, so a bare "how many passed" says nothing about whether it blocks too
/// much. The other three are the populations §9-1 / §9-8 / §9-9 deferred to
/// free observation. `prior_generation` is where BOTH the readopt hole (§9-1)
/// and the pre-allocation skew (§9-9) land, and its rate is what chooses between
/// the α and β repairs; `legacy_zero` is fail-open by decision (§9-8) and only
/// means something paired with [`generation_subsystem_available`]; `older` is
/// ordinary reconcile work, kept separate so it cannot inflate the other two.
fn generation_relation(born_generation: u64, current_generation: u64) -> &'static str {
    if born_generation == 0 {
        "legacy_zero"
    } else if born_generation == current_generation {
        "current_generation"
    } else if born_generation.saturating_add(1) == current_generation {
        "prior_generation"
    } else {
        "older"
    }
}

/// §9-8's transfer condition: `legacy_zero` conflates "a binary predating the
/// field wrote this row" with "this deployment could not open the generation
/// file", and going fail-closed is only safe for the first. Reads the path and
/// never allocates — allocating from an observation would move the very epoch
/// that observation reports on.
fn generation_subsystem_available() -> bool {
    crate::services::discord::runtime_store::generation_path().is_some()
}

/// Clear a normal reconcile row after a locked fresh-read generation fence.
pub(in crate::services::discord) fn clear_inflight_state_for_reconcile(
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
) -> ReconcileClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::Missing);
    };
    let current_generation = crate::services::discord::runtime_store::process_generation();
    let outcome =
        clear_inflight_state_for_reconcile_in_root(&root, provider, snapshot, current_generation);
    observe_reconcile_outcome(
        provider,
        snapshot,
        current_generation,
        "clear_inflight_state_for_reconcile",
        &inflight_state_path(&root, provider, snapshot.channel_id),
        outcome,
    );
    outcome
}

/// Clear a reconcile-owned rebind-origin row after the same generation fence.
pub(in crate::services::discord) fn clear_rebind_origin_for_reconcile(
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
) -> ReconcileClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return ReconcileClearOutcome::Delegated(GuardedClearOutcome::Missing);
    };
    let current_generation = crate::services::discord::runtime_store::process_generation();
    let outcome =
        clear_rebind_origin_for_reconcile_in_root(&root, provider, snapshot, current_generation);
    observe_reconcile_outcome(
        provider,
        snapshot,
        current_generation,
        "clear_rebind_origin_for_reconcile",
        &inflight_state_path(&root, provider, snapshot.channel_id),
        outcome,
    );
    outcome
}

fn observe_reconcile_outcome(
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
    current_generation: u64,
    site: &'static str,
    path: &std::path::Path,
    outcome: ReconcileClearOutcome,
) {
    match outcome {
        ReconcileClearOutcome::LiveGenerationSkipped => {
            record_inflight_invariant_with_severity(
                false,
                snapshot,
                "reconcile_never_clears_current_generation_row",
                "src/services/discord/inflight/clear_store/reconcile_gate.rs",
                "reconcile must preserve a row authored by the running process",
                serde_json::json!({
                    "site": site,
                    "born_generation": snapshot.born_generation,
                    "current_generation": current_generation,
                    "user_msg_id": snapshot.user_msg_id,
                    "finalizer_turn_id": snapshot.finalizer_turn_id,
                    "turn_nonce": snapshot.turn_nonce,
                    "updated_at": snapshot.updated_at,
                    "save_generation": snapshot.save_generation,
                    "tmux_session_name": snapshot.tmux_session_name,
                    "path": path.display().to_string(),
                }),
                ObsSeverity::Warn,
            );
        }
        ReconcileClearOutcome::Delegated(delegated) => {
            // The delegated outcome rides along because "allowed" alone cannot
            // separate a fence passing real work through from one whose callers
            // all bounce off the identity guard behind it — a regression that
            // looks healthy in a bare allow count.
            let generation_relation =
                generation_relation(snapshot.born_generation, current_generation);
            let delegated_outcome = format!("{delegated:?}");
            crate::services::observability::emit_inflight_lifecycle_event(
                provider.as_str(),
                snapshot.channel_id,
                snapshot.dispatch_id.as_deref(),
                snapshot.session_key.as_deref(),
                None,
                "reconcile_generation_gate_allowed",
                serde_json::json!({
                    "site": site,
                    "born_generation": snapshot.born_generation,
                    "current_generation": current_generation,
                    "generation_relation": generation_relation,
                    "generation_subsystem_available": generation_subsystem_available(),
                    "delegated_outcome": delegated_outcome,
                    "user_msg_id": snapshot.user_msg_id,
                    "path": path.display().to_string(),
                }),
            );
            tracing::info!(
                provider = %provider.as_str(),
                channel_id = snapshot.channel_id,
                user_msg_id = snapshot.user_msg_id,
                born_generation = snapshot.born_generation,
                current_generation,
                generation_relation,
                generation_subsystem_available = generation_subsystem_available(),
                delegated_outcome = ?delegated,
                site,
                path = %path.display(),
                "reconcile generation gate allowed delegated inflight clear"
            );
        }
    }
}

fn clear_inflight_state_for_reconcile_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
    current_generation: u64,
) -> ReconcileClearOutcome {
    super::identity::clear_inflight_state_if_matches_identity_turn_nonce_for_reconcile_in_root(
        root,
        provider,
        snapshot.channel_id,
        &InflightTurnIdentity::from_state(snapshot),
        snapshot.turn_nonce.as_deref(),
        current_generation,
    )
}

fn clear_rebind_origin_for_reconcile_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
    current_generation: u64,
) -> ReconcileClearOutcome {
    super::identity::clear_rebind_origin_inflight_state_if_matches_identity_for_reconcile_in_root(
        root,
        provider,
        snapshot.channel_id,
        &InflightTurnIdentity::from_state(snapshot),
        snapshot.turn_nonce.as_deref(),
        current_generation,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn row(channel_id: u64, born_generation: u64) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            Some("adk-claude".to_string()),
            7,
            8,
            9,
            "live reconcile row".to_string(),
            Some("session-5462".to_string()),
            Some(format!("AgentDesk-claude-gate-{channel_id}")),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        state.born_generation = born_generation;
        state.turn_nonce = Some(format!("nonce-{channel_id}"));
        state
    }

    fn seed(root: &std::path::Path, state: &InflightTurnState) {
        super::super::save_inflight_state_in_root(root, state).expect("seed inflight row");
    }

    #[test]
    fn current_generation_predicate_is_nonzero_and_exact() {
        let current = 5462;
        let current_row = row(1, current);
        let legacy_row = row(2, 0);
        let prior_row = row(3, current - 1);
        assert!(row_is_current_generation(&current_row, current));
        assert!(!row_is_current_generation(&legacy_row, 0));
        assert!(!row_is_current_generation(&prior_row, current));
    }

    // Collapsing `prior_generation` into `older` would hide the readopt hole's
    // rate, which is the one number that chooses between the α and β repairs.
    #[test]
    fn allowed_generation_buckets_separate_the_deferred_populations() {
        assert_eq!(generation_relation(0, 5462), "legacy_zero");
        // Zero row in a deployment whose counter is also zero stays fail-open,
        // not "current" — else an unavailable generation subsystem would read as
        // a fence protecting every row.
        assert_eq!(generation_relation(0, 0), "legacy_zero");
        assert_eq!(generation_relation(5462, 5462), "current_generation");
        assert_eq!(generation_relation(5461, 5462), "prior_generation");
        assert_eq!(generation_relation(5460, 5462), "older");
        // A future-generation row (downgrade / rollback) is not readopt either.
        assert_eq!(generation_relation(5463, 5462), "older");
    }

    #[test]
    fn normal_reconcile_clear_uses_fresh_locked_generation() {
        let temp = TempDir::new().expect("temp root");
        let snapshot = row(54_620, 54_61);
        let live_rewrite = row(54_620, 54_62);
        seed(temp.path(), &live_rewrite);

        let outcome = clear_inflight_state_for_reconcile_in_root(
            temp.path(),
            &ProviderKind::Claude,
            &snapshot,
            54_62,
        );
        assert_eq!(outcome, ReconcileClearOutcome::LiveGenerationSkipped);
        assert!(inflight_state_path(temp.path(), &ProviderKind::Claude, 54_620).exists());
    }

    #[test]
    fn legacy_zero_generation_remains_fail_open() {
        let temp = TempDir::new().expect("temp root");
        let legacy = row(54_621, 0);
        seed(temp.path(), &legacy);

        let outcome = clear_inflight_state_for_reconcile_in_root(
            temp.path(),
            &ProviderKind::Claude,
            &legacy,
            54_62,
        );
        assert_eq!(
            outcome,
            ReconcileClearOutcome::Delegated(GuardedClearOutcome::Cleared)
        );
        assert!(!inflight_state_path(temp.path(), &ProviderKind::Claude, 54_621).exists());
    }

    #[test]
    fn current_generation_rebind_origin_is_protected() {
        let temp = TempDir::new().expect("temp root");
        let mut live = row(54_622, 54_62);
        live.rebind_origin = true;
        seed(temp.path(), &live);

        let outcome = clear_rebind_origin_for_reconcile_in_root(
            temp.path(),
            &ProviderKind::Claude,
            &live,
            54_62,
        );
        assert_eq!(outcome, ReconcileClearOutcome::LiveGenerationSkipped);
        assert!(inflight_state_path(temp.path(), &ProviderKind::Claude, 54_622).exists());
    }
}
