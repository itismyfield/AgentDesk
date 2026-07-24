//! #3982 — orphan-at-birth reclaim trigger for the TUI-direct synthetic-start
//! backstop path.
//!
//! A TUI-direct synthetic inflight created AFTER its per-turn StreamRelay
//! producer already exited is stamped `SessionBoundRelay` (the claim-time
//! `get_producer` returns a STALE `Some` because the registry deregisters only
//! on session teardown, never on a per-turn producer exit). That row never
//! commits, so the synthetic-start backstop perpetually misreads it as a live
//! FOREIGN inflight and aborts EVERY later TUI-direct turn (#3960 covers only
//! the producer-death-MID-turn variant, not this orphaned-at-birth one).
//!
//! This module provides the injected
//! [`ReclaimOrphanFn`](super::super::tui_direct_pending_start::ReclaimOrphanFn)
//! the backstop worker consults before its terminal abort: it downgrades an
//! orphan-shaped foreign inflight's relay owner to `None` via the existing
//! identity-guarded [`downgrade_orphaned_session_bound_relay_owner_locked`], so
//! the next view's ownerless-stale filter drops the row and the deferred claim
//! proceeds (and ownerless recovery re-delivers the orphan's uncommitted
//! suffix). It NEVER gates on the proven-stale `get_producer` oracle — the
//! authoritative liveness guard is the in-lock orphan-shape re-check + identity.

use super::super::inflight::{
    InflightTurnIdentity, InflightTurnState, OrphanRelayReclaimOutcome,
    downgrade_orphaned_session_bound_relay_owner_locked, load_inflight_state,
    session_bound_relay_external_input_orphan_shape,
};
use super::super::tui_direct_pending_start::{ReclaimOrphanFn, ReclaimStaleForeignOutcome};
use crate::services::provider::ProviderKind;

/// #3982: PURE reclaimability predicate — is `state` a producer-dead
/// `SessionBoundRelay` orphan (the #3960 orphan-shape: 300s-quiescent,
/// zero-progress, never-delivered, `!session_bound_delivered`) on the given
/// caller session? Split out so the decision is unit-testable with NO I/O and NO
/// `AGENTDESK_ROOT_DIR` dependency; the identity-guarded flock RMW that acts on
/// it is proven separately in `inflight::orphan_relay_reclaim`. The session gate
/// ensures the trigger only ever downgrades an orphan belonging to the tmux
/// session it is deferring for.
fn orphan_row_is_reclaimable(state: &InflightTurnState, require_tmux_session: &str) -> bool {
    session_bound_relay_external_input_orphan_shape(state)
        && state.tmux_session_name.as_deref() == Some(require_tmux_session)
}

/// #3982: the pure (SharedData-free) core of the orphan-at-birth reclaim
/// trigger. Loads the inflight row for `(provider, channel_id)` and, IFF it is a
/// producer-dead `SessionBoundRelay` orphan owned by `require_tmux_session`
/// ([`orphan_row_is_reclaimable`]), downgrades its relay owner to `None` via the
/// identity-guarded flock RMW [`downgrade_orphaned_session_bound_relay_owner_locked`].
/// Returns `true` iff the owner was downgraded.
///
/// Invariants:
/// * The orphan SHAPE and the `InflightTurnIdentity` are captured from the SAME
///   unlocked load — no TOCTOU between the shape observation and the identity the
///   downgrade re-pins (INV-2).
/// * The downgrade primitive RE-validates the orphan shape + identity + lifecycle
///   UNDER the flock, so a row that mutated since the load (a watcher terminal
///   commit → non-quiescent, a fresh replacement turn → identity mismatch, a
///   pinned restart/rebind → lifecycle) is rejected (`Skipped` → `false`).
/// * It NEVER consults the proven-stale `get_producer` oracle — the in-lock
///   orphan-shape re-check IS the liveness authority (a live turn cannot satisfy
///   300s-quiescence + zero-progress + never-delivered + `!session_bound_delivered`).
/// * Only an orphan on THIS worker's own tmux session (the caller session) is
///   reclaimable, so a stray cross-session row is never touched.
fn reclaim_orphan_inflight_owner(
    provider: &ProviderKind,
    channel_id: u64,
    require_tmux_session: &str,
) -> bool {
    let Some(state) = load_inflight_state(provider, channel_id) else {
        return false;
    };
    if !orphan_row_is_reclaimable(&state, require_tmux_session) {
        return false;
    }
    let identity = InflightTurnIdentity::from_state(&state);
    matches!(
        downgrade_orphaned_session_bound_relay_owner_locked(
            provider,
            channel_id,
            &identity,
            require_tmux_session,
        ),
        OrphanRelayReclaimOutcome::Downgraded
    )
}

/// #3982: the worker's per-escalation-cycle orphan-reclaim action (the injected
/// [`ReclaimOrphanFn`]). Delegates to the pure [`reclaim_orphan_inflight_owner`];
/// returns `true` iff a producer-dead `SessionBoundRelay` orphan blocking this
/// synthetic start was downgraded to ownerless. On `true` the worker re-evaluates
/// immediately so the deferred claim proceeds instead of aborting; on `false` it
/// keeps the existing bounded escalation/abort.
pub(super) fn pending_start_reclaim_orphan_fn() -> ReclaimOrphanFn {
    Box::new(|shared, record| {
        Box::pin(async move {
            if super::super::tui_direct_pending_start::demote_stale_foreign_inflight_if_current(
                shared, record,
            )
            .await
            {
                return ReclaimStaleForeignOutcome::StaleForeignDemoted;
            }
            let Some(provider) = ProviderKind::from_str(&record.provider) else {
                return ReclaimStaleForeignOutcome::None;
            };
            let reclaimed = reclaim_orphan_inflight_owner(
                &provider,
                record.channel_id,
                &record.tmux_session_name,
            );
            if reclaimed {
                tracing::info!(
                    provider = %record.provider,
                    channel_id = record.channel_id,
                    tmux_session_name = %record.tmux_session_name,
                    anchor_message_id = record.anchor_message_id,
                    "tui_direct_pending_start: downgraded a producer-dead SessionBoundRelay orphan to ownerless on the backstop path; the deferred claim can now proceed and ownerless recovery re-delivers the orphan's uncommitted suffix (#3982)"
                );
            }
            if reclaimed {
                ReclaimStaleForeignOutcome::SessionBoundOrphanReclaimed
            } else {
                ReclaimStaleForeignOutcome::None
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::orphan_row_is_reclaimable;
    use crate::services::discord::inflight::{
        InflightTurnState, RelayOwnerKind, TurnSource, load_inflight_state,
        session_bound_relay_external_input_orphan_shape,
    };
    use crate::services::discord::tui_direct_pending_start::{
        PendingStartState, ReclaimStaleForeignOutcome, TuiDirectPendingStart,
    };
    use crate::services::provider::ProviderKind;

    fn isolated_runtime_root() -> (tempfile::TempDir, crate::config::TestEnvVarGuard) {
        let root = tempfile::TempDir::new().expect("runtime root");
        let env = crate::config::set_agentdesk_root_for_test(root.path());
        (root, env)
    }

    fn write_inflight_fixture(
        root: &std::path::Path,
        provider: &ProviderKind,
        state: &InflightTurnState,
    ) {
        let path = root
            .join("runtime")
            .join("discord_inflight")
            .join(provider.as_str())
            .join(format!("{}.json", state.channel_id));
        std::fs::create_dir_all(path.parent().expect("inflight parent"))
            .expect("create inflight fixture dir");
        std::fs::write(
            path,
            serde_json::to_string_pretty(state).expect("serialize inflight fixture"),
        )
        .expect("write inflight fixture");
    }

    /// A local-time timestamp string in the inflight `updated_at` format, `age`
    /// seconds in the past.
    fn local_string(age_secs: i64) -> String {
        (chrono::Local::now() - chrono::Duration::seconds(age_secs))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    /// Build (IN MEMORY — no I/O, no `AGENTDESK_ROOT_DIR`) a `SessionBoundRelay`
    /// TUI-direct synthetic row. When `stale`, its `started_at`/`updated_at` are
    /// far enough in the past to trip the 300s staleness gate, so it matches the
    /// producer-dead orphan-at-birth shape; otherwise it is a fresh
    /// (recently-advanced) row that must NEVER reclaim.
    fn session_bound_row(channel_id: u64, tmux: &str, stale: bool) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            None,
            0,
            7001, // user_msg_id (the injected anchor — never 0)
            0,    // current_msg_id (no placeholder posted)
            "typed in TUI".to_string(),
            None,
            Some(tmux.to_string()),
            None,
            None,
            0,
        );
        state.turn_source = TurnSource::ExternalInput;
        state.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
        state.injected_prompt_message_id = Some(8001);
        let ts = local_string(if stale { 100_000 } else { 0 });
        state.started_at = ts.clone();
        state.updated_at = ts;
        state
    }

    fn pending_record(channel_id: u64, tmux: &str) -> TuiDirectPendingStart {
        TuiDirectPendingStart {
            provider: "claude".to_string(),
            channel_id,
            tmux_session_name: tmux.to_string(),
            prompt_text: "/loop tick".to_string(),
            anchor_message_id: 9001,
            lease_relay_owner: "bridge_adapter".to_string(),
            lease_runtime_kind: Some("claude_tui".to_string()),
            lease_turn_id: None,
            lease_session_key: None,
            generation: 0,
            created_at_ms: 0,
            observed_at_ms: 0,
            state: PendingStartState::Waiting,
            attempt_count: 0,
        }
    }

    /// #3982 (test plan item 3): the pure reclaimability decision matches ONLY a
    /// producer-dead `SessionBoundRelay` orphan on the caller session. This tests
    /// the trigger's DECISION with no I/O; the identity-guarded flock RMW it gates
    /// (`downgrade_orphaned_session_bound_relay_owner_locked`) is proven in
    /// `inflight::orphan_relay_reclaim::tests`, and the worker wiring in
    /// `tui_direct_pending_start::tests::backstop_orphan_reclaim_*`.
    #[test]
    fn orphan_row_is_reclaimable_matches_stale_session_bound_orphan_on_caller_session() {
        let (_root, _env) = isolated_runtime_root();
        let tmux = "AgentDesk-claude-adk-cc";
        let orphan = session_bound_row(4242, tmux, true);
        // Sanity: this is a genuine producer-dead SessionBoundRelay orphan.
        assert!(
            session_bound_relay_external_input_orphan_shape(&orphan),
            "the fixture is orphan-shaped (300s-quiescent, zero-progress, never-delivered)"
        );
        assert!(
            orphan_row_is_reclaimable(&orphan, tmux),
            "an orphan-shaped row on the caller session is reclaimable (#3982)"
        );
    }

    /// #3982: a FRESH (recently-advanced) `SessionBoundRelay` row is NOT
    /// orphan-shaped — it could be a genuinely live turn — so it is NEVER
    /// reclaimable. Guards over-reclaim of a slow-but-live turn.
    #[test]
    fn orphan_row_is_reclaimable_skips_fresh_row() {
        let (_root, _env) = isolated_runtime_root();
        let tmux = "AgentDesk-claude-adk-cc";
        let fresh = session_bound_row(4343, tmux, false);
        assert!(
            !session_bound_relay_external_input_orphan_shape(&fresh),
            "a fresh row is not orphan-shaped"
        );
        assert!(
            !orphan_row_is_reclaimable(&fresh, tmux),
            "a fresh (non-stale) SessionBoundRelay row is never reclaimed (over-reclaim guard)"
        );
    }

    /// #3982: an orphan on a DIFFERENT tmux session than the worker's own (caller)
    /// session is never reclaimable — the trigger only downgrades an orphan
    /// belonging to the session it is deferring for, even though the row is
    /// otherwise orphan-shaped.
    #[test]
    fn orphan_row_is_reclaimable_skips_foreign_session() {
        let (_root, _env) = isolated_runtime_root();
        let orphan = session_bound_row(4444, "AgentDesk-claude-adk-cc", true);
        assert!(
            session_bound_relay_external_input_orphan_shape(&orphan),
            "the fixture is orphan-shaped"
        );
        assert!(
            !orphan_row_is_reclaimable(&orphan, "AgentDesk-claude-OTHER"),
            "an orphan on a foreign session must not be reclaimed by this worker (#3982)"
        );
    }

    #[test]
    fn pending_start_reclaim_orphan_fn_uses_real_ordering_for_session_bound_orphan() {
        let (root, _env) = isolated_runtime_root();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let channel_id = 4_030_401;
            let tmux = "AgentDesk-claude-4030-session-bound-orphan";
            let state = session_bound_row(channel_id, tmux, true);
            write_inflight_fixture(root.path(), &ProviderKind::Claude, &state);
            let record = pending_record(channel_id, tmux);

            let reclaim = super::pending_start_reclaim_orphan_fn();
            let outcome = reclaim(&shared, &record).await;

            assert_eq!(
                outcome,
                ReclaimStaleForeignOutcome::SessionBoundOrphanReclaimed,
                "SessionBoundRelay must bypass stale-foreign demotion and then reclaim via #3982 orphan downgrade"
            );
            let reloaded = load_inflight_state(&ProviderKind::Claude, channel_id)
                .expect("orphan row remains for ownerless recovery");
            assert_eq!(reloaded.effective_relay_owner_kind(), RelayOwnerKind::None);
        });
    }
}
