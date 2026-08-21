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
/// means something paired with [`current_generation_nonzero`]; `older` is
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

/// §9-8's transfer condition needs `legacy_zero` split into "a binary predating
/// the field wrote this row" and "this deployment could not produce a
/// generation", because going fail-closed is only safe for the first.
///
/// PROVES: exactly that split, and nothing else, and only on the far side of
/// this process's own allocation. Once the epoch is allocated and positive,
/// every row this process writes carries that positive `born_generation` — none
/// of the three positive-failure routes below stamps a zero — so a zero-born row
/// it meets after that point is a legacy binary's.
///
/// The one exception is not a failure MODE but a TIMING window: before the
/// allocation lands, `process_generation()` falls back to `load_generation()`,
/// which reads zero on a boot whose counter is absent or unreadable, so a row
/// born in that window carries `born_generation = 0` from THIS binary and does
/// land in the zero bucket. The `Residual (§9-9)` paragraph below is that
/// window, not a fourth failure route. A normal boot closes it before it can
/// produce such a row: `run_bot_build_shared_data` calls
/// `allocate_process_generation` while it builds `SharedData`, so the epoch is
/// fixed before intake opens, and what keeps the fallback reachable at all is
/// constructor-only tooling and tests.
///
/// DOES NOT PROVE — which is why this is no longer spelled
/// `generation_subsystem_available` (#5462 S5 r3): allocation success, an epoch
/// advanced past the previous process's, or a fence worth trusting.
/// `allocate_durable_generation` fails POSITIVE on three routes and every one of
/// them reads `true` here. A `lock_generation_path` failure falls back to
/// `load_generation()` and returns the previous process's counter unchanged, so
/// this process shares that process's generation. An `atomic_write` failure
/// returns the pre-increment `current`, positive whenever the counter already
/// was. And a counter at `u64::MAX` writes successfully while
/// `saturating_add(1)` leaves the epoch exactly where it was. Telling those from
/// a real allocation needs the allocation's own provenance carried into the
/// observation, which means touching `runtime_store` — deferred, not solved.
///
/// The false half is the honest one, and is why this is the allocated epoch
/// rather than a path probe: the same two failures reach ZERO instead when the
/// counter is missing, corrupt, or absent on a first boot, and
/// `generation_path() == None` is a third route to zero. The path is `Some` in
/// the first two, so a probe would call the subsystem available while the epoch
/// it reports on is zero.
///
/// Residual (§9-9) — ONLY WHERE THE COUNTER CANNOT BE READ: the pre-allocation
/// window returns `load_generation()`, the value the PREVIOUS process wrote, so
/// a row born in that window normally carries `born_generation = N-1` and lands
/// in `prior_generation` — the bucket §9-9 itself nominated for observing it.
/// That fallback reads zero wherever the counter cannot be read — absent on a
/// first boot, or present but unparseable, which `load_generation()` collapses
/// into the same `unwrap_or(0)` — and only there does §9-9's population mix into
/// §9-8's `legacy_zero`. Separating even that needs a provenance field this
/// slice does not add.
///
/// Takes the epoch the caller already read instead of reading it again: this is
/// an observation, and `allocate_process_generation` is a `OnceLock` initializer
/// that would move the very epoch the observation reports on.
fn current_generation_nonzero(current_generation: u64) -> bool {
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
    current_generation_nonzero: bool,
}

impl AllowedGenerationFacts {
    fn observe(snapshot: &InflightTurnState, current_generation: u64) -> Self {
        Self {
            relation: generation_relation(snapshot.born_generation, current_generation),
            current_generation_nonzero: current_generation_nonzero(current_generation),
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
///
/// That prefixing left one invariant name — the
/// `reconcile_never_clears_current_generation_row` below — with TWO producers
/// publishing DIFFERENT field sets (#5462 S5 r3). The sibling,
/// `removal::record_loader_generation_gate`, reads its row off disk under its
/// own lock and so names `user_msg_id` / `finalizer_turn_id` /
/// `turn_nonce` / `updated_at` / `save_generation` / `tmux_session_name`
/// unprefixed; here those same six are a stale caller snapshot and must say so.
/// The schemas are deliberately NOT unified — each spelling is true where it
/// stands — so `site` is the discriminator: it reads
/// `load_inflight_states_from_root_stale` there and a
/// `clear_*_for_reconcile` name here. A §9-2-style query filtering an
/// unprefixed field (`details->>'tmux_session_name' IS NOT NULL`) therefore
/// counts loader rows only and drops every row from this site silently.
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
                    "current_generation_nonzero": facts.current_generation_nonzero,
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
                current_generation_nonzero = facts.current_generation_nonzero,
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
        // not "current" — else a zero epoch would read as a fence protecting
        // every row.
        assert_eq!(generation_relation(0, 0), "legacy_zero");
        assert_eq!(generation_relation(5462, 5462), "current_generation");
        assert_eq!(generation_relation(5461, 5462), "prior_generation");
        assert_eq!(generation_relation(5460, 5462), "older");
        // A future-generation row (downgrade / rollback) is not readopt either.
        assert_eq!(generation_relation(5463, 5462), "older");
    }

    // A zero epoch is the only signal §9-8 has that this deployment could not
    // produce a generation, and a path probe cannot see the two failure routes
    // that keep the path `Some`. The true side classifies nothing beyond that —
    // see the predicate's docstring for the three positive allocation failures
    // it cannot separate.
    #[test]
    fn current_generation_nonzero_is_the_allocated_epoch_not_a_path_probe() {
        assert!(current_generation_nonzero(5462));
        assert!(current_generation_nonzero(1));
        assert!(!current_generation_nonzero(0));
    }

    // Both allow-side facts are wired from the caller's arguments, in that
    // order: `generation_relation(current, born)` would still satisfy every
    // assertion in the classifier test above while redefining the bucket that
    // chooses between the α and β repairs.
    //
    // The third case is the discriminating one, and among the three below the
    // only shape where the row's generation and the epoch disagree about being
    // zero: a legacy row in a healthy deployment. The first two are
    // zero-together or positive-together, so both survive reading the flag off
    // the wrong source (`current_generation_nonzero(snapshot.born_generation)`)
    // and both survive collapsing it into `relation != "legacy_zero"` — a
    // tautology that would delete §9-8's discriminator outright. The third
    // kills each.
    #[test]
    fn allow_side_facts_are_wired_from_the_row_then_the_epoch() {
        let facts = AllowedGenerationFacts::observe(&row(1, 54_61), 54_62);
        assert_eq!(facts.relation, "prior_generation");
        assert!(facts.current_generation_nonzero);

        let zero_epoch = AllowedGenerationFacts::observe(&row(2, 0), 0);
        assert_eq!(zero_epoch.relation, "legacy_zero");
        assert!(!zero_epoch.current_generation_nonzero);

        let legacy_row_in_a_live_deployment = AllowedGenerationFacts::observe(&row(3, 0), 54_62);
        assert_eq!(legacy_row_in_a_live_deployment.relation, "legacy_zero");
        assert!(legacy_row_in_a_live_deployment.current_generation_nonzero);
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
