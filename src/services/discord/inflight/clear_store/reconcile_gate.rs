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
    ///
    /// Carries that row's `born_generation` (#5462 S5 r2): the fence judges the
    /// row it re-read under the lock, and the caller's snapshot can already be
    /// stale by then, so the refusal observation has no other way to name the
    /// generation it actually protected.
    LiveGenerationSkipped { fresh_born_generation: u64 },
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
///
/// `older` also absorbs FUTURE-generation rows (`born > current`, i.e. a counter
/// rollback or a downgrade), which is the one anomalous series in the set. It is
/// pooled with ordinary work on purpose — nothing in this slice acts on it — but
/// that means the distribution cannot surface it, so a rollback has to be found
/// from the raw `born_generation` / `current_generation` pair instead.
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
/// field wrote this row" with "this deployment could not produce a generation",
/// and going fail-closed is only safe for the first.
///
/// The flag is the allocated epoch itself, not a path probe: a healthy
/// subsystem allocates `>= 1`, so a zero epoch is the subsystem failing.
/// `allocate_durable_generation` reaches zero by THREE routes and a path probe
/// only sees the first — `generation_path() == None`; a `lock_generation_path`
/// failure falling back to `load_generation()` on a missing/corrupt counter; and
/// an `atomic_write` failure returning the pre-increment `current`, which is
/// zero on a first boot. The path is `Some` in the last two, so a probe would
/// report the subsystem available while the epoch it reports on is zero.
///
/// Residual (§9-9): the pre-allocation window is the fourth way to observe a
/// zero epoch, and it is indistinguishable here from the failure routes. A row
/// born in that window carries `born_generation = 0` and lands in `legacy_zero`
/// rather than `prior_generation`, so §9-9's population mixes into §9-8's.
/// Separating them needs a provenance field this slice does not add.
///
/// Takes the epoch the caller already read instead of reading it again: this is
/// an observation, and `allocate_process_generation` is a `OnceLock` initializer
/// that would move the very epoch the observation reports on.
fn generation_subsystem_available(current_generation: u64) -> bool {
    current_generation > 0
}

/// The generation facts one allow-side observation reports.
///
/// Evaluated once per observation and shared by both sinks, so the analytics
/// event and the stdout line cannot disagree, and built here rather than at each
/// sink so a test can pin the argument order into [`generation_relation`] —
/// swapping those two arguments silently redefines `prior_generation` as
/// "future-generation row" while the classifier's own tests all still pass.
struct AllowedGenerationFacts {
    relation: &'static str,
    subsystem_available: bool,
}

impl AllowedGenerationFacts {
    fn observe(snapshot: &InflightTurnState, current_generation: u64) -> Self {
        Self {
            relation: generation_relation(snapshot.born_generation, current_generation),
            subsystem_available: generation_subsystem_available(current_generation),
        }
    }
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

/// §7.2-1's refusal payload, describing the row the refusal PROTECTED.
///
/// `born_generation` is the fence's own — the row it re-read inside the sidecar
/// flock. Every field taken from the caller's snapshot carries a `snapshot_`
/// prefix instead of standing in for the protected row: that snapshot is ~91ms
/// old in the dominant refusal shape (#5462 S1b measured that window in the
/// accident), so its `born_generation` reads `current - 1` and its turn identity
/// names the turn reconcile tried to DELETE, not the live one. Publishing those
/// unprefixed made this stream report a "blocked readopt" population that does
/// not exist, next to §7.2-2's allow-side buckets that measure the real one.
///
/// The correlation fields `record_inflight_invariant_with_severity` derives
/// (`provider` / `channel_id` / `dispatch_id` / `session_key` / `turn_id`) are
/// still the snapshot's; the outcome carries only the generation, and channel is
/// the join key both rows share.
fn refusal_details(
    site: &'static str,
    snapshot: &InflightTurnState,
    fresh_born_generation: u64,
    current_generation: u64,
    path: &std::path::Path,
) -> serde_json::Value {
    serde_json::json!({
        "site": site,
        "born_generation": fresh_born_generation,
        "current_generation": current_generation,
        "snapshot_born_generation": snapshot.born_generation,
        "snapshot_user_msg_id": snapshot.user_msg_id,
        "snapshot_finalizer_turn_id": snapshot.finalizer_turn_id,
        "snapshot_turn_nonce": snapshot.turn_nonce,
        "snapshot_updated_at": snapshot.updated_at,
        "snapshot_save_generation": snapshot.save_generation,
        "snapshot_tmux_session_name": snapshot.tmux_session_name,
        "path": path.display().to_string(),
    })
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
        ReconcileClearOutcome::LiveGenerationSkipped {
            fresh_born_generation,
        } => {
            record_inflight_invariant_with_severity(
                false,
                snapshot,
                "reconcile_never_clears_current_generation_row",
                "src/services/discord/inflight/clear_store/reconcile_gate.rs",
                "reconcile must preserve a row authored by the running process",
                refusal_details(
                    site,
                    snapshot,
                    fresh_born_generation,
                    current_generation,
                    path,
                ),
                ObsSeverity::Warn,
            );
        }
        ReconcileClearOutcome::Delegated(delegated) => {
            // The delegated outcome rides along because "allowed" alone cannot
            // separate a fence passing real work through from one whose callers
            // all bounce off the identity guard behind it — a regression that
            // looks healthy in a bare allow count.
            let facts = AllowedGenerationFacts::observe(snapshot, current_generation);
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
                    "generation_relation": facts.relation,
                    "generation_subsystem_available": facts.subsystem_available,
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
                generation_relation = facts.relation,
                generation_subsystem_available = facts.subsystem_available,
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

    // A zero epoch is the only signal §9-8 has that the subsystem failed, and a
    // path probe cannot see the two failure routes that keep the path `Some`.
    #[test]
    fn subsystem_availability_is_the_allocated_epoch_not_a_path_probe() {
        assert!(generation_subsystem_available(5462));
        assert!(generation_subsystem_available(1));
        assert!(!generation_subsystem_available(0));
    }

    // Both allow-side facts are wired from the caller's arguments, in that
    // order: `generation_relation(current, born)` would still satisfy every
    // assertion in the classifier test above while redefining the bucket that
    // chooses between the α and β repairs.
    #[test]
    fn allow_side_facts_are_wired_from_the_row_then_the_epoch() {
        let facts = AllowedGenerationFacts::observe(&row(1, 54_61), 54_62);
        assert_eq!(facts.relation, "prior_generation");
        assert!(facts.subsystem_available);

        let unavailable = AllowedGenerationFacts::observe(&row(2, 0), 0);
        assert_eq!(unavailable.relation, "legacy_zero");
        assert!(!unavailable.subsystem_available);
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
        assert_eq!(
            outcome,
            ReconcileClearOutcome::LiveGenerationSkipped {
                fresh_born_generation: 54_62
            }
        );
        assert!(inflight_state_path(temp.path(), &ProviderKind::Claude, 54_620).exists());
    }

    // The dominant refusal shape: the reconcile scan snapshotted generation
    // N-1, a live turn rewrote the row at N inside the scan window, and the
    // fence refused. The WARN has to describe the row it protected (N) — the
    // stale N-1 it used to publish contradicted its own message and invented a
    // "blocked readopt" population for §9-1 / §9-8 to read.
    #[test]
    fn refusal_warn_describes_the_fresh_row_not_the_stale_snapshot() {
        let temp = TempDir::new().expect("temp root");
        let snapshot = row(54_623, 54_61);
        let live_rewrite = row(54_623, 54_62);
        seed(temp.path(), &live_rewrite);
        let path = inflight_state_path(temp.path(), &ProviderKind::Claude, 54_623);

        let outcome = clear_inflight_state_for_reconcile_in_root(
            temp.path(),
            &ProviderKind::Claude,
            &snapshot,
            54_62,
        );
        let ReconcileClearOutcome::LiveGenerationSkipped {
            fresh_born_generation,
        } = outcome
        else {
            panic!("the fence must refuse the live row: {outcome:?}");
        };

        let details = refusal_details(
            "clear_inflight_state_for_reconcile",
            &snapshot,
            fresh_born_generation,
            54_62,
            &path,
        );
        assert_eq!(details["born_generation"], 54_62);
        assert_eq!(details["current_generation"], 54_62);
        assert_eq!(details["snapshot_born_generation"], 54_61);
        // Every remaining snapshot field is named as such, so nothing in this
        // payload can be read as belonging to the protected row.
        for key in [
            "snapshot_user_msg_id",
            "snapshot_finalizer_turn_id",
            "snapshot_turn_nonce",
            "snapshot_updated_at",
            "snapshot_save_generation",
            "snapshot_tmux_session_name",
        ] {
            assert!(details.get(key).is_some(), "missing {key}");
            assert!(
                details.get(key.trim_start_matches("snapshot_")).is_none(),
                "{key} must not also be published unprefixed"
            );
        }
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
        assert_eq!(
            outcome,
            ReconcileClearOutcome::LiveGenerationSkipped {
                fresh_born_generation: 54_62
            }
        );
        assert!(inflight_state_path(temp.path(), &ProviderKind::Claude, 54_622).exists());
    }
}
