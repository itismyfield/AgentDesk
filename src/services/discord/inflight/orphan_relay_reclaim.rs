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
//! This module closes the residual by RE-CHECKING producer liveness + the
//! delivery authority at the idle-relay tick (not just at claim time) and, when
//! the caller has proven the live producer is gone AND the committed-offset
//! authority covers nothing of this turn's body, downgrading the relay owner
//! back to `None` — the SAME bridge-adapter backstop state #3876 stamps when no
//! producer exists at claim. The row then rejoins the ownerless-recovery
//! population and its uncommitted suffix is re-delivered.
//!
//! WHERE THE NO-DOUBLE-RELAY GUARANTEE LIVES — the SEND-POINT committed re-gate,
//! NOT this in-lock shape re-check. The real `SessionBoundRelay` TUI-direct
//! terminal route (`session_relay_sink.rs:1066-1124`, `advance_after_confirmed_post`
//! at :1116) advances ONLY the shared `relay_coord.confirmed_end_offset`
//! watermark and writes NOTHING to the inflight row (no
//! `current_msg_id`/`response_sent_offset`/`full_response`/`terminal_delivery_committed`).
//! So a delivered-but-unmirrored row STAYS orphan-shaped under the flock and the
//! downgrade PROCEEDS — that is expected and correct. Single delivery is then
//! guaranteed because EVERY re-delivery path re-reads `effective_committed_offset`
//! FRESH and `idle_relay_range_action` returns `SkipAlreadyRelayed` (whole body
//! already past the watermark) or `SendSuffixFrom(committed)` (only the
//! uncommitted tail). The caller's unlocked `committed <= turn_floor` gate is a
//! first-line filter for the already-advanced case; the send-point re-gate is
//! the authority.
//!
//! What the in-lock re-check (and the identity/lifecycle guards) DO catch:
//! row-MUTATING in-window commits — the watcher terminal-commit route
//! (`commit_watcher_terminal_delivery_locked`) sets
//! `terminal_delivery_committed`/`full_response`/`response_sent_offset` ON the
//! row, so a watcher commit landing between the candidate scan and this flock is
//! detected (shape no longer matches → abort) — plus a fresh turn B that
//! replaced the orphan (identity guard) and a pinned restart/rebind lifecycle.

use std::path::Path;

use crate::services::provider::ProviderKind;

use super::model::{InflightTurnIdentity, InflightTurnState, RelayOwnerKind, TurnSource};
use super::{
    INFLIGHT_STALENESS_THRESHOLD_SECS, inflight_runtime_root, inflight_state_is_stale,
    inflight_state_path, load_inflight_state_unlocked, lock_inflight_state_path, now_unix,
    persist_under_lock, persist_under_lock_preserving_updated_at,
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
        // #3976: a DELIVERED-but-unmirrored row (the NewMessage/short-replace
        // confirmed-POST route advances only the resettable
        // `confirmed_end_offset` watermark and writes nothing else) is byte-for-byte
        // identical to a never-delivered black-hole row, so on a watermark reset
        // below the turn body it would be wrongly downgraded to ownerless and the
        // already-delivered tail re-emitted. The durable `session_bound_delivered`
        // marker — stamped ONLY on a genuinely confirmed delivery — is the
        // discriminator: a delivered row is NOT orphan-shaped, while a genuine
        // black-hole (flag still `false`) STILL is.
        && !state.session_bound_delivered
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
    /// a row-MUTATING commit landed (the watcher terminal-commit route sets
    /// `terminal_delivery_committed`/`full_response`/`response_sent_offset`), a
    /// fresh turn replaced the row (identity), or a restart/rebind lifecycle
    /// marker is pinned. The downgrade was aborted. (This does NOT fire for the
    /// watermark-only NewMessage commit, which leaves the row orphan-shaped —
    /// that case is covered by the send-point committed re-gate, not here.)
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
/// The in-lock orphan re-check catches a ROW-MUTATING in-window commit (the
/// watcher terminal-commit route writes `terminal_delivery_committed` etc. to the
/// row → shape no longer matches → `Skipped`) plus the identity / restart-lifecycle
/// races. It does NOT — and need not — catch the watermark-only NewMessage commit
/// (`session_relay_sink.rs:1066-1124`), which leaves the row orphan-shaped: that
/// row is downgraded and single delivery is guaranteed downstream by the
/// send-point committed re-gate (`idle_relay_range_action` over a FRESH
/// `effective_committed_offset`). See this module's header.
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
    // Re-validate the orphan shape against the in-lock reload. This catches a
    // ROW-MUTATING in-window commit — the watcher terminal-commit route sets
    // `terminal_delivery_committed`/`full_response`/`response_sent_offset` on the
    // row, so a watcher commit landing between the caller's candidate scan and
    // this flock leaves the row non-quiescent → abort. It does NOT catch the
    // watermark-only NewMessage commit (which leaves the row orphan-shaped); that
    // row IS downgraded, and the send-point committed re-gate guarantees single
    // delivery. See this module's header.
    if !session_bound_relay_external_input_orphan_shape_at(&state, now_unix()) {
        return OrphanRelayReclaimOutcome::Skipped;
    }
    state.set_relay_owner_kind(RelayOwnerKind::None);
    // #3982: preserve the row's existing (already-stale) `updated_at` instead of
    // bumping it. Downgrading a confirmed-dead orphan to `owner=None` is an owner
    // *correction*, not new lifecycle activity, so the quiescence clock must not
    // reset. A fresh bump would make the triggering TUI-direct turn's next
    // fresh re-read see a "0 s-old" ownerless row → `ownerless_external_input_
    // inflight_is_stale=false` → the view builder KEEPS it → `inflight_present=true`
    // → the deferred claim keeps aborting for another ~300 s. Preserving the old
    // stale timestamp lets the very next read drop the row so the turn claims now.
    match persist_under_lock_preserving_updated_at(
        root,
        &path,
        &state,
        "src/services/discord/inflight/orphan_relay_reclaim.rs:downgrade_orphaned_session_bound_relay_owner_locked_in_root",
    ) {
        Ok(()) => OrphanRelayReclaimOutcome::Downgraded,
        Err(_) => OrphanRelayReclaimOutcome::IoError,
    }
}

/// Outcome of [`mark_session_bound_relay_delivered_locked`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum MarkDeliveredOutcome {
    /// The durable `session_bound_delivered` marker is now set on the row
    /// (either freshly stamped this call or already set — idempotent).
    Marked,
    /// The in-lock reload no longer matches the caller's identity (a fresh turn
    /// replaced the row, or the row was cleared) — the marker was NOT stamped, so
    /// a wrong/replaced turn is never marked delivered.
    Skipped,
    /// Filesystem or lock acquisition failure.
    IoError,
}

/// #3976: single-flock read-modify-write that stamps the durable
/// `session_bound_delivered` marker on a `SessionBoundRelay` TUI-direct row AFTER
/// a genuinely confirmed terminal delivery. Acquires the sidecar flock ONCE,
/// reloads the on-disk row, re-checks the caller's identity against the freshly
/// reloaded row, then sets the flag and persists via [`persist_under_lock`] —
/// never re-entering [`super::save_inflight_state`] (which would re-acquire the
/// same non-reentrant flock and self-deadlock).
///
/// The identity re-gate is the SAFETY boundary: the confirmed-POST path already
/// matched the frame identity to the inflight it loaded UNLOCKED, but a fresh
/// turn B could have replaced the row between that load and this flock. Re-gating
/// under the lock guarantees the marker only ever lands on the SAME turn whose
/// delivery was confirmed — never on a replacement turn. Idempotent: a row that
/// already carries the marker returns `Marked` without a redundant write.
pub(in crate::services::discord) fn mark_session_bound_relay_delivered_locked(
    provider: &ProviderKind,
    channel_id: u64,
    require_identity: &InflightTurnIdentity,
    require_tmux_session_name: &str,
) -> MarkDeliveredOutcome {
    let Some(root) = inflight_runtime_root() else {
        return MarkDeliveredOutcome::IoError;
    };
    mark_session_bound_relay_delivered_locked_in_root(
        &root,
        provider,
        channel_id,
        require_identity,
        require_tmux_session_name,
    )
}

/// Root-explicit variant of [`mark_session_bound_relay_delivered_locked`] for
/// unit tests (avoids `AGENTDESK_ROOT_DIR` env-var races).
pub(super) fn mark_session_bound_relay_delivered_locked_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    require_identity: &InflightTurnIdentity,
    require_tmux_session_name: &str,
) -> MarkDeliveredOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return MarkDeliveredOutcome::IoError;
    };
    let Some(mut state) = load_inflight_state_unlocked(&path) else {
        return MarkDeliveredOutcome::Skipped;
    };
    // Strong identity guard (user_msg_id + started_at + tmux_session +
    // turn_start_offset) plus the caller-supplied session: reject stamping the
    // marker onto a fresh row B that replaced the turn whose delivery confirmed.
    if !require_identity.matches_state(&state)
        || state.tmux_session_name.as_deref() != Some(require_tmux_session_name)
    {
        return MarkDeliveredOutcome::Skipped;
    }
    if state.session_bound_delivered {
        // Idempotent: already stamped, no redundant write.
        return MarkDeliveredOutcome::Marked;
    }
    state.session_bound_delivered = true;
    match persist_under_lock(
        root,
        &path,
        &state,
        "src/services/discord/inflight/orphan_relay_reclaim.rs:mark_session_bound_relay_delivered_locked_in_root",
    ) {
        Ok(()) => MarkDeliveredOutcome::Marked,
        Err(_) => MarkDeliveredOutcome::IoError,
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

    /// #3960 — a ROW-MUTATING in-window commit aborts the downgrade. The watcher
    /// terminal-commit route (`commit_watcher_terminal_delivery_locked`) writes
    /// `terminal_delivery_committed`/`full_response`/`response_sent_offset` ONTO
    /// the row, so a watcher commit landing between the caller's unlocked
    /// candidate scan and the locked downgrade leaves the reload non-quiescent →
    /// the in-lock shape re-check observes it and ABORTS. (This is what the
    /// in-lock re-check is FOR — NOT the watermark-only NewMessage route, which is
    /// covered by the separate test below + the send-point re-gate.)
    #[test]
    fn locked_downgrade_aborts_when_row_mutating_watcher_commit_lands_in_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let now_unix = now_unix();
        let candidate = orphan_row(now_unix);
        let provider = ProviderKind::Claude;
        let channel_id = candidate.channel_id;
        // The caller captured this identity from its unlocked orphan scan.
        let identity = InflightTurnIdentity::from_state(&candidate);

        // ... then the WATCHER terminal-commit route mutated the row (sets the
        // row's terminal fields) before the downgrade acquired the flock.
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
            "a row-mutating watcher commit in the window must abort the reclaim"
        );

        let path = inflight_state_path(root, &provider, channel_id);
        let reloaded = load_inflight_state_unlocked(&path).expect("row intact");
        assert_eq!(
            reloaded.effective_relay_owner_kind(),
            RelayOwnerKind::SessionBoundRelay,
            "a row-mutating-committed row keeps its owner — never reclaimed"
        );
    }

    /// #3960 — a WATERMARK-ONLY NewMessage commit in the window does NOT abort the
    /// downgrade, and that is correct. The real `SessionBoundRelay` TUI-direct
    /// terminal route (`session_relay_sink.rs:1066-1124`) advances ONLY the shared
    /// `confirmed_end_offset` watermark and writes NOTHING to the inflight row, so
    /// a delivered-but-unmirrored row STAYS orphan-shaped under the flock and the
    /// in-lock shape re-check CANNOT see the delivery → the downgrade PROCEEDS.
    /// No double-relay results: single delivery is then guaranteed by the
    /// send-point committed re-gate (asserted in
    /// `session_relay_sink::orphan_reclaim::tests::send_point_re_gate_*`).
    #[test]
    fn locked_downgrade_proceeds_for_watermark_only_newmessage_commit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let now_unix = now_unix();
        // The NewMessage route delivered the body but mirrored NOTHING back onto
        // the row — it remains byte-for-byte orphan-shaped.
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
        assert_eq!(
            outcome,
            OrphanRelayReclaimOutcome::Downgraded,
            "the in-lock shape re-check cannot see a watermark-only commit — downgrade proceeds \
             (the send-point committed re-gate is what prevents the double-relay)"
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

    /// #3976 — the EXACT uncovered case. A `SessionBoundRelay` TUI-direct row that
    /// was genuinely DELIVERED via the confirmed-POST route carries the durable
    /// `session_bound_delivered` marker. Even after the resettable
    /// `confirmed_end_offset` watermark is reset to 0 below the turn body
    /// (generation change / output regression / restart), the row is otherwise
    /// byte-for-byte identical to a never-delivered black-hole — but the marker
    /// makes the orphan-shape predicate return FALSE, so it is NOT reclaimed and
    /// recovery cannot re-emit the already-delivered tail.
    #[test]
    fn orphan_shape_rejects_delivered_marker_row() {
        let now_unix = 1_900_000_000;
        let mut delivered = orphan_row(now_unix);
        // Sanity: with the marker unset this is the genuine black-hole shape.
        assert!(
            session_bound_relay_external_input_orphan_shape_at(&delivered, now_unix),
            "an unmarked stale quiescent SessionBoundRelay row is orphan-shaped (the black-hole)"
        );
        // The confirmed POST stamped the durable delivered marker; the watermark
        // has since reset to 0, so every other conjunct still holds.
        delivered.session_bound_delivered = true;
        assert!(
            !session_bound_relay_external_input_orphan_shape_at(&delivered, now_unix),
            "a confirmed-delivered SessionBoundRelay row is NOT orphan-shaped — no tail re-emit"
        );
    }

    /// #3976 — no over-suppression: a GENUINE never-delivered black-hole row
    /// (`session_bound_delivered == false`, producer died before any confirmed
    /// POST) is STILL orphan-shaped and STILL reclaimable.
    #[test]
    fn orphan_shape_still_matches_genuine_black_hole_without_delivered_marker() {
        let now_unix = 1_900_000_000;
        let black_hole = orphan_row(now_unix);
        assert!(
            !black_hole.session_bound_delivered,
            "the genuine black-hole never confirmed a delivery"
        );
        assert!(
            session_bound_relay_external_input_orphan_shape_at(&black_hole, now_unix),
            "a never-delivered black-hole is still reclaimable (no over-suppression)"
        );
    }

    /// #3976 — the marker writer stamps `session_bound_delivered` only when the
    /// reloaded row matches the caller's identity, and is idempotent.
    #[test]
    fn mark_delivered_sets_marker_on_identity_match_and_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let now_unix = now_unix();
        let state = orphan_row(now_unix);
        let provider = ProviderKind::Claude;
        let channel_id = state.channel_id;
        write_row_verbatim(root, &state);
        let identity = InflightTurnIdentity::from_state(&state);
        let session = state.tmux_session_name.as_deref().expect("session");

        let outcome = mark_session_bound_relay_delivered_locked_in_root(
            root, &provider, channel_id, &identity, session,
        );
        assert_eq!(outcome, MarkDeliveredOutcome::Marked);

        let path = inflight_state_path(root, &provider, channel_id);
        let reloaded = load_inflight_state_unlocked(&path).expect("row survives mark");
        assert!(
            reloaded.session_bound_delivered,
            "a confirmed delivery durably stamps the delivered marker"
        );
        // The marked row is no longer orphan-shaped.
        assert!(!session_bound_relay_external_input_orphan_shape_at(
            &reloaded, now_unix
        ));

        // Idempotent: a second stamp is a no-op success, marker stays set.
        let again = mark_session_bound_relay_delivered_locked_in_root(
            root, &provider, channel_id, &identity, session,
        );
        assert_eq!(again, MarkDeliveredOutcome::Marked);
        let reloaded2 = load_inflight_state_unlocked(&path).expect("row intact");
        assert!(reloaded2.session_bound_delivered);
    }

    /// #3976 — the marker writer NEVER stamps a row whose identity no longer
    /// matches the caller (a fresh turn B replaced turn A during the POST), so a
    /// replacement turn is never wrongly marked delivered.
    #[test]
    fn mark_delivered_skips_on_identity_mismatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let now_unix = now_unix();
        let row_a = orphan_row(now_unix);
        let provider = ProviderKind::Claude;
        let channel_id = row_a.channel_id;
        let identity_a = InflightTurnIdentity::from_state(&row_a);

        // Turn B replaced the row before the (turn-A) delivery confirmation wrote.
        let mut row_b = orphan_row(now_unix);
        row_b.user_msg_id = row_a.user_msg_id + 500;
        write_row_verbatim(root, &row_b);

        let outcome = mark_session_bound_relay_delivered_locked_in_root(
            root,
            &provider,
            channel_id,
            &identity_a,
            row_b.tmux_session_name.as_deref().expect("session"),
        );
        assert_eq!(outcome, MarkDeliveredOutcome::Skipped);

        let path = inflight_state_path(root, &provider, channel_id);
        let reloaded = load_inflight_state_unlocked(&path).expect("row B intact");
        assert!(
            !reloaded.session_bound_delivered,
            "turn B must not inherit turn A's delivered marker"
        );
    }

    /// #3976 — serde back-compat: a legacy on-disk row lacking the
    /// `session_bound_delivered` field deserializes with the marker `false`
    /// (orphan-shaped), and a round-trip with the field present preserves `true`.
    #[test]
    fn delivered_marker_serde_back_compat() {
        let now_unix = 1_900_000_000;
        // Legacy row: the orphan_row fixture JSON omits session_bound_delivered.
        let legacy = orphan_row(now_unix);
        assert!(
            !legacy.session_bound_delivered,
            "a legacy row missing the field deserializes the marker as false"
        );

        // Round-trip with the field set preserves it.
        let mut marked = legacy;
        marked.session_bound_delivered = true;
        let json = serde_json::to_string(&marked).expect("serialize");
        let round: InflightTurnState = serde_json::from_str(&json).expect("deserialize");
        assert!(
            round.session_bound_delivered,
            "a serialized delivered marker survives a round-trip"
        );
    }

    /// #3982 efficacy regression — the orphan downgrade must PRESERVE the row's
    /// existing (already-stale) `updated_at` instead of bumping it to now. This is
    /// what makes the triggering TUI-direct turn claim IMMEDIATELY: after the
    /// downgrade to `owner=None` the view builder re-reads the row FRESH, and only
    /// a preserved-old `updated_at` keeps `ownerless_external_input_inflight_is_stale`
    /// true so the `.filter` drops the row → `inflight_present=false` → the deferred
    /// claim proceeds on the very next window (no extra ~300 s wait).
    ///
    /// Under the old unconditional bump this test is RED at assertions (ii)/(iii):
    /// the fresh `updated_at` makes the row look ~0 s old → not stale → kept → the
    /// turn keeps aborting. Root-explicit (no `AGENTDESK_ROOT_DIR` race) and writes
    /// the fixture verbatim so the stale timestamp survives to disk.
    #[test]
    fn downgrade_preserves_updated_at_so_row_stays_ownerless_droppable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let now_unix = now_unix();
        let provider = ProviderKind::Claude;

        // Orphan row whose quiescence clock reads T-500 s (well past the 300 s
        // staleness threshold), stamped verbatim so the stale value hits disk.
        let mut state = orphan_row(now_unix);
        let stale_updated_at = to_local(now_unix - 500);
        state.started_at = stale_updated_at.clone();
        state.updated_at = stale_updated_at.clone();
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

        // (i) owner corrected to the bridge backstop.
        assert_eq!(
            reloaded.effective_relay_owner_kind(),
            RelayOwnerKind::None,
            "the confirmed-dead orphan is downgraded to owner None"
        );
        // (ii) the OLD stale timestamp is preserved — NOT bumped to now.
        assert_eq!(
            reloaded.updated_at, stale_updated_at,
            "the owner correction must NOT reset the quiescence clock"
        );
        // (iii) the real view-builder ownerless-stale filter WOULD drop it now, so
        // the triggering turn's next fresh read yields inflight_present=false.
        assert!(
            super::super::ownerless_external_input_inflight_is_stale(&reloaded),
            "the preserved-stale ownerless row is immediately droppable by the view filter"
        );
    }

    /// #3982 guard — the SIBLING `mark_delivered` persist path is a REAL mutation
    /// (`session_bound_delivered = true`) and must STILL bump `updated_at`. This
    /// pins that the bump-flag split swapped only the downgrade call site, not this
    /// one (a mis-swap would silently regress delivery-marker freshness).
    #[test]
    fn mark_delivered_still_bumps_updated_at() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let now_unix = now_unix();
        let provider = ProviderKind::Claude;

        let mut state = orphan_row(now_unix);
        let stale_updated_at = to_local(now_unix - 500);
        state.updated_at = stale_updated_at.clone();
        let channel_id = state.channel_id;
        write_row_verbatim(root, &state);

        let identity = InflightTurnIdentity::from_state(&state);
        let outcome = mark_session_bound_relay_delivered_locked_in_root(
            root,
            &provider,
            channel_id,
            &identity,
            state.tmux_session_name.as_deref().expect("session"),
        );
        assert_eq!(outcome, MarkDeliveredOutcome::Marked);

        let path = inflight_state_path(root, &provider, channel_id);
        let reloaded = load_inflight_state_unlocked(&path).expect("row survives mark");
        assert!(
            reloaded.session_bound_delivered,
            "the delivery marker is durably stamped"
        );
        assert_ne!(
            reloaded.updated_at, stale_updated_at,
            "a real delivery mutation still refreshes the quiescence clock"
        );
    }
}
