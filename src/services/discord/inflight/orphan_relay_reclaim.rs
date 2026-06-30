//! #3960 — producer-liveness TOCTOU reclaim for orphaned `SessionBoundRelay`
//! TUI-direct rows.
//!
//! #3876 (PR #3953) gates the `SessionBoundRelay` owner stamp on a LIVE
//! per-session relay producer existing AT CLAIM time
//! (`tui_prompt_relay::synthetic_start::claim_tui_direct_synthetic_turn`,
//! `global_relay_producer_registry().get_producer(session).is_some()`). The
//! producer can deregister / die in the window between that claim and the
//! terminal commit/ACK. In that window the row stays owned by
//! `SessionBoundRelay`, so [`super::ownerless_external_input_inflight_is_stale_at`]
//! — which requires `owner == None` — never reclaims it; yet no producer feeds
//! the supervisor StreamRelay, the watcher yields to the dead owner, and the
//! bridge tail is off → the TUI-direct answer black-holes (#3960, the #3876
//! residual deferred from PR #3953).
//!
//! This module closes the residual by RE-CHECKING the orphan condition AT
//! COMMIT time UNDER THE INFLIGHT FLOCK (not just at claim time) and, when the
//! caller has proven the live producer is gone AND the delivery authority
//! covers nothing of this turn's body, downgrading the relay owner back to
//! `None` — the SAME bridge-adapter backstop state #3876 stamps when no producer
//! exists at claim. The row then rejoins the ownerless-recovery population and
//! its uncommitted suffix is re-delivered exactly once (the committed-offset
//! authority on the re-delivery path skips any already-relayed bytes — no
//! double-relay). The in-lock re-check is the TOCTOU closure: if a terminal
//! delivery commits in the window between the caller's unlocked candidate scan
//! and this flock, the reloaded row is no longer quiescent/uncommitted and the
//! downgrade aborts, so a delivered turn is never reclaimed.

use std::path::Path;

use crate::services::provider::ProviderKind;

use super::model::{InflightTurnIdentity, InflightTurnState, RelayOwnerKind, TurnSource};
use super::{
    INFLIGHT_STALENESS_THRESHOLD_SECS, inflight_runtime_root, inflight_state_is_stale,
    inflight_state_path, load_inflight_state_unlocked, lock_inflight_state_path, now_unix,
    persist_under_lock,
};

/// #3960 — the row SHAPE of a `SessionBoundRelay` TUI-direct synthetic claim
/// whose claim-time relay producer has died with the answer still undelivered.
///
/// Mirrors [`super::ownerless_external_input_inflight_is_stale_at`] EXCEPT the
/// relay owner is `SessionBoundRelay` (not `None`): the #3876 producer-gate
/// stamps that owner only when a live producer existed at claim, and that owner
/// makes the ownerless predicate skip the row forever. The remaining conjuncts
/// are the identical zero-progress + stale-by-age quiescence checks — a row that
/// created no Discord placeholder (`current_msg_id == 0`), relayed no bytes
/// (`response_sent_offset == 0`, `full_response` empty, no
/// `last_watcher_relayed_offset`), committed no terminal body
/// (`!terminal_delivery_committed`), and has not advanced for
/// `INFLIGHT_STALENESS_THRESHOLD_SECS`. `restart_mode.is_none()` excludes
/// planned-restart rows (the `recovery_engine` restore path owns those).
///
/// This is ONLY the row-shape half of the reclaim decision. The caller must
/// ALSO prove (a) the live producer is gone and (b) the generation-aware
/// committed-offset authority covers nothing of this turn's body before
/// downgrading — see the gate in `session_relay_sink::orphan_reclaim`.
pub(in crate::services::discord) fn session_bound_relay_external_input_orphan_shape_at(
    state: &InflightTurnState,
    now_unix_secs: i64,
) -> bool {
    state.turn_source == TurnSource::ExternalInput
        && state.effective_relay_owner_kind() == RelayOwnerKind::SessionBoundRelay
        && state.injected_prompt_message_id.is_some()
        && state.current_msg_id == 0
        && state.response_sent_offset == 0
        && state.full_response.trim().is_empty()
        && state.last_watcher_relayed_offset.is_none()
        && !state.terminal_delivery_committed
        && state.restart_mode.is_none()
        && inflight_state_is_stale(state, now_unix_secs, INFLIGHT_STALENESS_THRESHOLD_SECS)
}

/// `now`-bound wrapper over [`session_bound_relay_external_input_orphan_shape_at`].
pub(in crate::services::discord) fn session_bound_relay_external_input_orphan_shape(
    state: &InflightTurnState,
) -> bool {
    session_bound_relay_external_input_orphan_shape_at(state, now_unix())
}

/// Outcome of [`downgrade_orphaned_session_bound_relay_owner_locked`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum OrphanRelayReclaimOutcome {
    /// The orphaned `SessionBoundRelay` owner was downgraded to `None` (the
    /// bridge-adapter backstop). The row now rejoins ownerless recovery.
    Downgraded,
    /// The in-lock reload no longer matches the orphan shape / caller identity —
    /// a concurrent terminal commit landed, a fresh turn replaced the row, or a
    /// restart/rebind lifecycle marker is pinned. The downgrade was aborted.
    /// This is the TOCTOU/no-double-relay guard.
    Skipped,
    /// Filesystem or lock acquisition failure.
    IoError,
}

/// #3960: single-flock read-modify-write that downgrades an orphaned
/// `SessionBoundRelay` TUI-direct row's relay owner to `None`. Acquires the
/// sidecar flock ONCE, reloads the on-disk row, re-checks the caller's identity
/// AND the orphan shape against the freshly reloaded row, then flips the owner
/// and persists via [`persist_under_lock`] — never re-entering
/// [`super::save_inflight_state`] (which would re-acquire the same non-reentrant
/// flock and self-deadlock).
///
/// The in-lock orphan re-check is the claim→commit TOCTOU closure: if a terminal
/// delivery committed (or the row otherwise advanced) between the caller's
/// unlocked candidate scan and this flock, the reload fails
/// [`session_bound_relay_external_input_orphan_shape_at`] → `Skipped`, so a
/// delivered turn is NEVER downgraded (no double-relay).
pub(in crate::services::discord) fn downgrade_orphaned_session_bound_relay_owner_locked(
    provider: &ProviderKind,
    channel_id: u64,
    require_identity: &InflightTurnIdentity,
    require_tmux_session_name: &str,
) -> OrphanRelayReclaimOutcome {
    let Some(root) = inflight_runtime_root() else {
        return OrphanRelayReclaimOutcome::IoError;
    };
    downgrade_orphaned_session_bound_relay_owner_locked_in_root(
        &root,
        provider,
        channel_id,
        require_identity,
        require_tmux_session_name,
    )
}

/// Root-explicit variant of [`downgrade_orphaned_session_bound_relay_owner_locked`]
/// for unit tests (avoids `AGENTDESK_ROOT_DIR` env-var races).
pub(super) fn downgrade_orphaned_session_bound_relay_owner_locked_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    require_identity: &InflightTurnIdentity,
    require_tmux_session_name: &str,
) -> OrphanRelayReclaimOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return OrphanRelayReclaimOutcome::IoError;
    };
    let Some(mut state) = load_inflight_state_unlocked(&path) else {
        return OrphanRelayReclaimOutcome::Skipped;
    };
    // A pinned restart/rebind marker means another lifecycle owns the row; the
    // reclaim must not touch it (mirrors the watcher-state RMW guards).
    if state.restart_mode.is_some() || state.rebind_origin {
        return OrphanRelayReclaimOutcome::Skipped;
    }
    // Strong identity guard (user_msg_id + started_at + tmux_session +
    // turn_start_offset) plus the caller-supplied session: reject a downgrade
    // onto a fresh row B that replaced the orphan this scan observed.
    if !require_identity.matches_state(&state)
        || state.tmux_session_name.as_deref() != Some(require_tmux_session_name)
    {
        return OrphanRelayReclaimOutcome::Skipped;
    }
    // TOCTOU closure: re-validate the orphan shape against the in-lock reload. A
    // terminal delivery that committed in the candidate-scan→flock window leaves
    // the row no longer quiescent/uncommitted → abort, never reclaim a delivered
    // turn (no double-relay).
    if !session_bound_relay_external_input_orphan_shape_at(&state, now_unix()) {
        return OrphanRelayReclaimOutcome::Skipped;
    }
    state.set_relay_owner_kind(RelayOwnerKind::None);
    match persist_under_lock(
        root,
        &path,
        &state,
        "src/services/discord/inflight/orphan_relay_reclaim.rs:downgrade_orphaned_session_bound_relay_owner_locked_in_root",
    ) {
        Ok(()) => OrphanRelayReclaimOutcome::Downgraded,
        Err(_) => OrphanRelayReclaimOutcome::IoError,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn to_local(unix: i64) -> String {
        use chrono::TimeZone;
        chrono::Local
            .timestamp_opt(unix, 0)
            .single()
            .expect("valid local timestamp")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    /// Write a row to the inflight sidecar VERBATIM (bypassing
    /// `save_inflight_state`, which rewrites `updated_at = now`), so the fixture's
    /// stale `updated_at` survives to disk — the only way to exercise the
    /// staleness-gated reclaim in a unit test.
    fn write_row_verbatim(root: &std::path::Path, state: &InflightTurnState) {
        let path = inflight_state_path(root, &ProviderKind::Claude, state.channel_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create inflight dir");
        }
        let json = serde_json::to_string_pretty(state).expect("serialize row");
        std::fs::write(&path, json).expect("write row verbatim");
    }

    /// An orphaned SessionBoundRelay TUI-direct row: producer-stamped owner,
    /// quiescent zero-progress, no committed terminal body, stale by `updated_at`.
    fn orphan_row(now_unix: i64) -> InflightTurnState {
        let stale_unix = now_unix - (INFLIGHT_STALENESS_THRESHOLD_SECS as i64) - 1;
        serde_json::from_value(json!({
            "version": 9,
            "provider": "claude",
            "channel_id": 4242,
            "channel_name": "adk-cc",
            "request_owner_user_id": 7,
            "user_msg_id": 7001,
            "current_msg_id": 0,
            "current_msg_len": 0,
            "user_text": "typed in TUI",
            "source": "text",
            "session_id": null,
            "tmux_session_name": "AgentDesk-claude-adk-cc",
            "output_path": "/tmp/claude-transcript.jsonl",
            "input_fifo_path": null,
            "last_offset": 0,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": to_local(stale_unix),
            "updated_at": to_local(stale_unix),
            "terminal_delivery_committed": false,
            "relay_owner_kind": "session_bound_relay",
            "turn_source": "external_input",
            "injected_prompt_message_id": 8001
        }))
        .expect("deserialize orphan SessionBoundRelay external-input row")
    }

    #[test]
    fn orphan_shape_matches_dead_producer_session_bound_row() {
        let now_unix = 1_900_000_000;
        let mut state = orphan_row(now_unix);
        assert!(
            session_bound_relay_external_input_orphan_shape_at(&state, now_unix),
            "a stale, quiescent, uncommitted SessionBoundRelay TUI-direct claim is orphan-reclaimable"
        );

        // A fresh (recently-advanced) row still protects a live turn.
        state.updated_at = to_local(now_unix);
        assert!(
            !session_bound_relay_external_input_orphan_shape_at(&state, now_unix),
            "a fresh SessionBoundRelay row is still actively owned — never reclaimed"
        );
    }

    #[test]
    fn orphan_shape_rejects_committed_and_progressed_and_foreign_owner_rows() {
        let now_unix = 1_900_000_000;

        // Committed terminal body → already delivered → never reclaim (the
        // double-relay guard at the shape level).
        let mut committed = orphan_row(now_unix);
        committed.terminal_delivery_committed = true;
        assert!(!session_bound_relay_external_input_orphan_shape_at(
            &committed, now_unix
        ));

        // A created Discord placeholder → terminal recovery paths own it.
        let mut placeheld = orphan_row(now_unix);
        placeheld.current_msg_id = 9100;
        assert!(!session_bound_relay_external_input_orphan_shape_at(
            &placeheld, now_unix
        ));

        // Already relayed some bytes → not quiescent.
        let mut progressed = orphan_row(now_unix);
        progressed.response_sent_offset = 12;
        assert!(!session_bound_relay_external_input_orphan_shape_at(
            &progressed,
            now_unix
        ));

        // Owner None is the ownerless predicate's job, not this one.
        let mut ownerless = orphan_row(now_unix);
        ownerless.set_relay_owner_kind(RelayOwnerKind::None);
        assert!(!session_bound_relay_external_input_orphan_shape_at(
            &ownerless, now_unix
        ));

        // Owner Watcher is a live relay owner.
        let mut watcher_owned = orphan_row(now_unix);
        watcher_owned.set_relay_owner_kind(RelayOwnerKind::Watcher);
        assert!(!session_bound_relay_external_input_orphan_shape_at(
            &watcher_owned,
            now_unix
        ));
    }

    /// #3960 TOCTOU happy path: producer claimed (SessionBoundRelay) → producer
    /// dies before commit → the locked downgrade reclaims the row to the bridge
    /// backstop (owner `None`), so the existing ownerless recovery re-delivers it.
    #[test]
    fn locked_downgrade_reclaims_orphaned_row() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let now_unix = now_unix();
        let state = orphan_row(now_unix);
        let provider = ProviderKind::Claude;
        let channel_id = state.channel_id;
        write_row_verbatim(root, &state);

        let identity = InflightTurnIdentity::from_state(&state);
        let outcome = downgrade_orphaned_session_bound_relay_owner_locked_in_root(
            root,
            &provider,
            channel_id,
            &identity,
            state.tmux_session_name.as_deref().expect("session"),
        );
        assert_eq!(outcome, OrphanRelayReclaimOutcome::Downgraded);

        let path = inflight_state_path(root, &provider, channel_id);
        let reloaded = load_inflight_state_unlocked(&path).expect("row survives downgrade");
        assert_eq!(
            reloaded.effective_relay_owner_kind(),
            RelayOwnerKind::None,
            "the orphaned SessionBoundRelay owner is downgraded to the bridge backstop"
        );
    }

    /// #3960 TOCTOU race: a terminal delivery COMMITS in the window between the
    /// caller's unlocked candidate scan and the locked downgrade. The in-lock
    /// orphan re-check must observe the committed body and ABORT — proving a
    /// delivered turn is never reclaimed (no double-relay).
    #[test]
    fn locked_downgrade_aborts_when_delivery_committed_in_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let now_unix = now_unix();
        let candidate = orphan_row(now_unix);
        let provider = ProviderKind::Claude;
        let channel_id = candidate.channel_id;
        // The caller captured this identity from its unlocked orphan scan.
        let identity = InflightTurnIdentity::from_state(&candidate);

        // ... then the live producer committed the terminal body before the
        // downgrade acquired the flock. Persist the committed row to disk.
        let mut committed = candidate.clone();
        committed.terminal_delivery_committed = true;
        committed.current_msg_id = 9300;
        committed.full_response = "delivered answer".to_string();
        committed.response_sent_offset = committed.full_response.len();
        write_row_verbatim(root, &committed);

        let outcome = downgrade_orphaned_session_bound_relay_owner_locked_in_root(
            root,
            &provider,
            channel_id,
            &identity,
            committed.tmux_session_name.as_deref().expect("session"),
        );
        assert_eq!(
            outcome,
            OrphanRelayReclaimOutcome::Skipped,
            "a terminal delivery that commits in the window must abort the reclaim"
        );

        let path = inflight_state_path(root, &provider, channel_id);
        let reloaded = load_inflight_state_unlocked(&path).expect("row intact");
        assert_eq!(
            reloaded.effective_relay_owner_kind(),
            RelayOwnerKind::SessionBoundRelay,
            "an in-window-committed row keeps its owner — never reclaimed, never double-relayed"
        );
    }

    /// No false reclaim: a fresh turn B replaced the orphan A the scan observed.
    /// The identity guard rejects the downgrade.
    #[test]
    fn locked_downgrade_aborts_on_identity_mismatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let now_unix = now_unix();
        let row_a = orphan_row(now_unix);
        let provider = ProviderKind::Claude;
        let channel_id = row_a.channel_id;
        let identity_a = InflightTurnIdentity::from_state(&row_a);

        // A fresh turn B (different user_msg_id) now owns the channel row.
        let mut row_b = orphan_row(now_unix);
        row_b.user_msg_id = row_a.user_msg_id + 500;
        write_row_verbatim(root, &row_b);

        let outcome = downgrade_orphaned_session_bound_relay_owner_locked_in_root(
            root,
            &provider,
            channel_id,
            &identity_a,
            row_b.tmux_session_name.as_deref().expect("session"),
        );
        assert_eq!(outcome, OrphanRelayReclaimOutcome::Skipped);

        let path = inflight_state_path(root, &provider, channel_id);
        let reloaded = load_inflight_state_unlocked(&path).expect("row B intact");
        assert_eq!(
            reloaded.effective_relay_owner_kind(),
            RelayOwnerKind::SessionBoundRelay,
            "a downgrade keyed to turn A must not touch turn B"
        );
    }
}
