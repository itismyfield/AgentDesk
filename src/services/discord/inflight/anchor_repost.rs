//! #3918: durable at-most-once guards for the committed-then-gone anchor-repost
//! fallback (the #3607/#3610 "committed, then the message disappeared" backstop).
//!
//! `recovery_paths::restart::try_recover_anchor_repost` re-posts a committed
//! terminal answer as a NEW Discord message when the original anchor has
//! vanished. That send is NOT a transaction with the on-disk row retirement:
//! Discord can accept the new message and the process can then crash — or
//! `clear_inflight_state` can silently fail — before the row is cleared, which
//! re-enters the committed branch on the next boot and would re-post the SAME
//! answer. Because the delivered disposition (`FinishAndClear`) carries no
//! attempt bump, that duplication was previously UNBOUNDED.
//!
//! These two narrow read-modify-write helpers persist the idempotency state ON
//! the carrier row, gated by the SAME strong turn identity as
//! [`super::budget::bump_recovery_relay_attempts_if_matches_identity`] (and,
//! like it, NEVER refusing a restart-marked / rebind-origin / TUI-direct row by
//! marker alone — only identity decides, because the write preserves the on-disk
//! row and mutates a single field):
//!   * [`mark_anchor_reposted_if_matches_identity`] — records `anchor_reposted`
//!     right after a `Delivered` send and before the clear, so a re-run refuses
//!     a duplicate post (the primary at-most-once guard for the realistic
//!     silent-clear-failure loop).
//!   * [`bump_anchor_repost_attempts_if_matches_identity`] — counts the send-new
//!     attempt BEFORE the send, so the narrow Discord-accept→marker-write crash
//!     window (where the marker is not yet recorded) is hard-bounded.
//!
//! The pure decision that consumes both fields lives in
//! [`crate::services::discord::recovery_paths::shared::anchor_repost_send_new_permitted`].

use std::fs;
use std::path::Path;

use super::{GuardedSaveOutcome, InflightTurnIdentity, InflightTurnState};
use crate::services::provider::ProviderKind;

/// Strong-identity-guarded read-modify-write skeleton shared by the two
/// anchor-repost mutators. Mirrors `budget::bump_*`: hold the sidecar flock
/// across read AND write so a concurrent clear/save cannot interleave, refuse a
/// row that no longer matches `expected` (a newer turn / fresh rebind), and
/// preserve every other field verbatim — only `mutate` and the metadata stamps
/// change. Restart/rebind markers are NOT grounds for refusal (they are
/// preserved by the write), exactly as the budget bump requires for the
/// re-DrainRestart-marked carrier row.
fn mutate_matching_row_in_root<F>(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    context: &'static str,
    mutate: F,
) -> GuardedSaveOutcome
where
    F: FnOnce(&mut InflightTurnState),
{
    let path = super::inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = super::lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    // Row already cleared (delivered / force-cleared) → never resurrect.
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    let Ok(mut on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        // Malformed row: do not clobber — the loader eviction path GCs it.
        return GuardedSaveOutcome::IdentityMismatch;
    };
    // Strong identity: user_msg_id + started_at + tmux_session_name (+ offset for
    // TUI-direct disambiguation) must all match the turn whose repost just ran.
    if !expected.matches_state(&on_disk) {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        if on_disk.turn_start_offset != Some(expected_offset) {
            return GuardedSaveOutcome::IdentityMismatch;
        }
    } else if expected.user_msg_id == 0 && on_disk.turn_start_offset.is_some() {
        // TUI-direct turns (`user_msg_id == 0`) collide on `started_at`'s
        // 1-second resolution; without an offset to compare we cannot prove
        // this is the same turn, so refuse rather than mark a stranger's row.
        return GuardedSaveOutcome::IdentityMismatch;
    }
    mutate(&mut on_disk);
    on_disk.ensure_finalizer_turn_id();
    on_disk.updated_at = super::now_string();
    super::bump_save_generation_for_write(&path, &mut on_disk);
    let Ok(json) = serde_json::to_string_pretty(&on_disk) else {
        return GuardedSaveOutcome::IoError;
    };
    match super::atomic_write(&path, &json) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                context,
                error = %error,
                "inflight anchor-repost row write failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

/// Record `anchor_reposted = true` on the on-disk inflight row for
/// `(provider, channel_id)` when it still matches `expected`. Called by
/// `try_recover_anchor_repost` immediately AFTER a `Delivered` send-new and
/// BEFORE the caller's `dispose_*` clears the row, so a persisted-row re-run
/// (failed clear / crash after this write) sees the marker and refuses to post
/// the same answer a second time.
pub(in crate::services::discord) fn mark_anchor_reposted_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(root) = super::inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    mark_anchor_reposted_if_matches_identity_in_root(
        &root,
        provider,
        channel_id,
        expected,
        expected_turn_start_offset,
    )
}

/// Root-explicit inner form for unit tests (avoids `AGENTDESK_ROOT_DIR` env
/// races, same pattern as the other `_in_root` helpers in this subtree).
pub(in crate::services::discord) fn mark_anchor_reposted_if_matches_identity_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    mutate_matching_row_in_root(
        root,
        provider,
        channel_id,
        expected,
        expected_turn_start_offset,
        "mark_anchor_reposted",
        |row| row.anchor_reposted = true,
    )
}

/// Increment `anchor_repost_attempts` on the on-disk inflight row for
/// `(provider, channel_id)` when it still matches `expected`. Called by
/// `try_recover_anchor_repost` BEFORE each send-new so the residual crash window
/// is hard-bounded to `RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET` posts.
pub(in crate::services::discord) fn bump_anchor_repost_attempts_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(root) = super::inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    bump_anchor_repost_attempts_if_matches_identity_in_root(
        &root,
        provider,
        channel_id,
        expected,
        expected_turn_start_offset,
    )
}

/// Root-explicit inner form for unit tests.
pub(in crate::services::discord) fn bump_anchor_repost_attempts_if_matches_identity_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    mutate_matching_row_in_root(
        root,
        provider,
        channel_id,
        expected,
        expected_turn_start_offset,
        "bump_anchor_repost_attempts",
        |row| row.anchor_repost_attempts = row.anchor_repost_attempts.saturating_add(1),
    )
}

#[cfg(test)]
mod tests {
    //! #3918 red-green: the committed-then-gone anchor-repost send-new must fire
    //! AT MOST ONCE per logical turn even under a duplicate/retry trigger.
    //!
    //! Hermetic by construction: rows are written/read with the root-explicit
    //! save + a direct file parse (NOT `load_inflight_states_from_root`, whose
    //! eviction side-effect consults the env-global generation counter).
    use std::path::Path;

    use tempfile::TempDir;

    use super::super::{
        GuardedSaveOutcome, InflightTurnIdentity, InflightTurnState, inflight_state_path,
        save_inflight_state_in_root,
    };
    use super::{
        bump_anchor_repost_attempts_if_matches_identity_in_root,
        mark_anchor_reposted_if_matches_identity_in_root,
    };
    use crate::services::discord::InflightRestartMode;
    use crate::services::discord::recovery_paths::shared::anchor_repost_send_new_permitted;
    use crate::services::provider::ProviderKind;

    const BUDGET: u32 = crate::services::discord::inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET;

    fn make_state(channel_id: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-cdx".to_string()),
            7,
            42,
            43,
            "hello".to_string(),
            Some("session-3918".to_string()),
            Some(format!("AgentDesk-codex-adk-cdx-{channel_id}")),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        )
    }

    fn read_row(root: &Path, channel_id: u64) -> Option<InflightTurnState> {
        let path = inflight_state_path(root, &ProviderKind::Codex, channel_id);
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// THE #3918 idempotency pin: a delivered anchor-repost records a durable
    /// `anchor_reposted` marker; if the row is NOT cleared afterwards (clear
    /// failed / crash after the marker write), the next boot re-loads it and the
    /// send-new is REFUSED — the repost fires AT MOST ONCE per turn even under a
    /// duplicate trigger.
    #[test]
    fn delivered_repost_is_not_resent_after_a_failed_clear() {
        let temp = TempDir::new().unwrap();
        let state = make_state(391_801);
        save_inflight_state_in_root(temp.path(), &state).expect("seed committed row");
        let identity = InflightTurnIdentity::from_state(&state);

        // Boot 1: a fresh committed row is permitted to repost.
        let row0 = read_row(temp.path(), state.channel_id).expect("seeded row");
        assert!(
            anchor_repost_send_new_permitted(
                row0.anchor_reposted,
                row0.anchor_repost_attempts,
                BUDGET
            ),
            "a fresh committed-then-gone row must be permitted to repost once"
        );

        // try_recover_anchor_repost: pre-send attempt bump, then (Delivered) the
        // durable marker — both BEFORE the caller's clear.
        assert_eq!(
            bump_anchor_repost_attempts_if_matches_identity_in_root(
                temp.path(),
                &ProviderKind::Codex,
                state.channel_id,
                &identity,
                state.turn_start_offset,
            ),
            GuardedSaveOutcome::Saved,
        );
        assert_eq!(
            mark_anchor_reposted_if_matches_identity_in_root(
                temp.path(),
                &ProviderKind::Codex,
                state.channel_id,
                &identity,
                state.turn_start_offset,
            ),
            GuardedSaveOutcome::Saved,
        );

        // Simulate `clear_inflight_state` FAILING: the row stays on disk. Boot 2
        // (the duplicate trigger) re-loads it and re-evaluates the guard.
        let row1 =
            read_row(temp.path(), state.channel_id).expect("row persists after a failed clear");
        assert!(
            row1.anchor_reposted,
            "a delivered repost must be durably marked"
        );
        assert_eq!(row1.anchor_repost_attempts, 1);
        assert!(
            !anchor_repost_send_new_permitted(
                row1.anchor_reposted,
                row1.anchor_repost_attempts,
                BUDGET
            ),
            "a persisted-row re-run MUST NOT send-new again — the repost is idempotent (at most once)"
        );
    }

    /// The residual Discord-accept→marker-write crash window is BUDGET-bounded:
    /// repeated pre-send attempts WITHOUT a recorded marker (each boot crashes
    /// before the marker lands) are refused once the budget is reached, so the
    /// duplication can never be UNBOUNDED.
    #[test]
    fn pre_send_attempts_are_budget_bounded_without_a_marker() {
        let temp = TempDir::new().unwrap();
        let state = make_state(391_802);
        save_inflight_state_in_root(temp.path(), &state).expect("seed");
        let identity = InflightTurnIdentity::from_state(&state);

        for attempt in 1..=BUDGET {
            let row = read_row(temp.path(), state.channel_id).expect("row persists");
            assert!(
                anchor_repost_send_new_permitted(
                    row.anchor_reposted,
                    row.anchor_repost_attempts,
                    BUDGET
                ),
                "attempt {attempt} must still be permitted while under budget"
            );
            assert_eq!(
                bump_anchor_repost_attempts_if_matches_identity_in_root(
                    temp.path(),
                    &ProviderKind::Codex,
                    state.channel_id,
                    &identity,
                    state.turn_start_offset,
                ),
                GuardedSaveOutcome::Saved,
            );
        }

        let row = read_row(temp.path(), state.channel_id).expect("row persists");
        assert_eq!(row.anchor_repost_attempts, BUDGET);
        assert!(
            !anchor_repost_send_new_permitted(
                row.anchor_reposted,
                row.anchor_repost_attempts,
                BUDGET
            ),
            "once the pre-send attempt budget is reached the repost MUST stop — bounded"
        );
    }

    /// Identity guard intact: a row now owned by a NEWER turn is never marked or
    /// counted (the #3041 protection `budget::bump` also enforces).
    #[test]
    fn mark_refuses_row_owned_by_newer_turn() {
        let temp = TempDir::new().unwrap();
        let mut newer = make_state(391_803);
        newer.user_msg_id = 999;
        save_inflight_state_in_root(temp.path(), &newer).expect("seed newer turn");

        let mut older = make_state(391_803);
        older.user_msg_id = 777;
        let identity = InflightTurnIdentity::from_state(&older);

        assert_eq!(
            mark_anchor_reposted_if_matches_identity_in_root(
                temp.path(),
                &ProviderKind::Codex,
                older.channel_id,
                &identity,
                older.turn_start_offset,
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        let row = read_row(temp.path(), older.channel_id).expect("row persists");
        assert_eq!(row.user_msg_id, 999, "newer turn's row must be untouched");
        assert!(!row.anchor_reposted, "stranger row must not be marked");
    }

    /// The mark preserves a restart marker verbatim — carrier rows are
    /// re-`DrainRestart`-marked on every shutdown, so the write must not refuse
    /// or strip it (same contract as the budget bump).
    #[test]
    fn mark_preserves_restart_marker() {
        let temp = TempDir::new().unwrap();
        let mut state = make_state(391_804);
        state.restart_mode = Some(InflightRestartMode::DrainRestart);
        save_inflight_state_in_root(temp.path(), &state).expect("seed restart-marked row");
        let identity = InflightTurnIdentity::from_state(&state);

        assert_eq!(
            mark_anchor_reposted_if_matches_identity_in_root(
                temp.path(),
                &ProviderKind::Codex,
                state.channel_id,
                &identity,
                state.turn_start_offset,
            ),
            GuardedSaveOutcome::Saved,
        );
        let row = read_row(temp.path(), state.channel_id).expect("row persists");
        assert!(row.anchor_reposted);
        assert_eq!(
            row.restart_mode,
            Some(InflightRestartMode::DrainRestart),
            "the restart marker must be preserved verbatim by the mark write"
        );
    }

    /// A cleared (missing) row is never resurrected by a mark/bump.
    #[test]
    fn mutators_do_not_resurrect_missing_row() {
        let temp = TempDir::new().unwrap();
        let state = make_state(391_805);
        let identity = InflightTurnIdentity::from_state(&state);

        assert_eq!(
            mark_anchor_reposted_if_matches_identity_in_root(
                temp.path(),
                &ProviderKind::Codex,
                state.channel_id,
                &identity,
                state.turn_start_offset,
            ),
            GuardedSaveOutcome::Missing,
        );
        assert_eq!(
            bump_anchor_repost_attempts_if_matches_identity_in_root(
                temp.path(),
                &ProviderKind::Codex,
                state.channel_id,
                &identity,
                state.turn_start_offset,
            ),
            GuardedSaveOutcome::Missing,
        );
        assert!(
            read_row(temp.path(), state.channel_id).is_none(),
            "mutators must not create a row"
        );
    }

    /// Codex hardening of #3918: the anchor-repost send-new is HARD-GATED on a
    /// `Saved` pre-send bump. `restart::try_recover_anchor_repost` proceeds to
    /// the send ONLY when the bump is `Saved`; every non-`Saved` outcome returns
    /// `AnchorRepostOutcome::RefusedPreserveRow` (refuse the post, preserve the
    /// row for a later boot). Without this gate an
    /// `IoError` bump (the attempt never persisted) would re-load the SAME
    /// `attempts == 0` row on every boot under a persistent write fault and send
    /// again forever → UNBOUNDED duplicate relay — the exact window the PR claims
    /// to hard-bound. This pins all three failure modes: the send is refused, and
    /// the row is left correctly for a later boot (IoError → deferred re-post) or
    /// untouched (Missing / IdentityMismatch → the gone/replaced turn never reposts).
    #[test]
    fn non_saved_pre_send_bump_blocks_the_send() {
        // Mirror the exact restart.rs send gate: post iff the bump is `Saved`.
        fn send_permitted(outcome: GuardedSaveOutcome) -> bool {
            matches!(outcome, GuardedSaveOutcome::Saved)
        }

        // (a) IoError — the attempt did NOT persist. Force a real, deterministic
        // IoError by rooting at a regular FILE: `create_dir_all(<file>/codex)`
        // fails with ENOTDIR (root-bypass-proof, unlike a chmod). The send MUST
        // be refused; this is the unbounded-dup case the fix closes.
        let temp = TempDir::new().unwrap();
        let root_file = temp.path().join("inflight-root-is-a-file");
        std::fs::write(&root_file, b"x").unwrap();
        let committed = make_state(391_806);
        let committed_identity = InflightTurnIdentity::from_state(&committed);
        let io = bump_anchor_repost_attempts_if_matches_identity_in_root(
            &root_file,
            &ProviderKind::Codex,
            committed.channel_id,
            &committed_identity,
            committed.turn_start_offset,
        );
        assert_eq!(io, GuardedSaveOutcome::IoError);
        assert!(
            !send_permitted(io),
            "an IoError pre-send bump (attempt not persisted) MUST refuse the send-new"
        );

        // An IoError leaves the on-disk row verbatim (attempts unchanged), so a
        // LATER boot whose bump succeeds re-posts the answer (deferred, not
        // dropped). A freshly committed, un-bumped row is the state a transient
        // IoError leaves behind — still permitted under budget.
        let temp = TempDir::new().unwrap();
        let deferred = make_state(391_807);
        save_inflight_state_in_root(temp.path(), &deferred).expect("seed committed row");
        let later =
            read_row(temp.path(), deferred.channel_id).expect("row persists for a later boot");
        assert_eq!(later.anchor_repost_attempts, 0);
        assert!(
            anchor_repost_send_new_permitted(
                later.anchor_reposted,
                later.anchor_repost_attempts,
                BUDGET
            ),
            "an IoError-blocked send leaves the row, so a later boot can still re-post (deferred)"
        );

        // (b) Missing — the row was cleared / never existed. The send MUST be
        // refused AND the mutator MUST NOT resurrect the row.
        let temp = TempDir::new().unwrap();
        let gone = make_state(391_808);
        let gone_identity = InflightTurnIdentity::from_state(&gone);
        let missing = bump_anchor_repost_attempts_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Codex,
            gone.channel_id,
            &gone_identity,
            gone.turn_start_offset,
        );
        assert_eq!(missing, GuardedSaveOutcome::Missing);
        assert!(
            !send_permitted(missing),
            "a Missing row MUST refuse the send-new (the turn is gone)"
        );
        assert!(
            read_row(temp.path(), gone.channel_id).is_none(),
            "a blocked (Missing) send must not resurrect the row"
        );

        // (c) IdentityMismatch — the row is now owned by a NEWER turn. The send
        // MUST be refused and the stranger row left untouched (the stale answer
        // never reposts nor consumes the live turn's budget).
        let temp = TempDir::new().unwrap();
        let mut newer = make_state(391_809);
        newer.user_msg_id = 999;
        save_inflight_state_in_root(temp.path(), &newer).expect("seed newer turn");
        let mut older = make_state(391_809);
        older.user_msg_id = 777;
        let older_identity = InflightTurnIdentity::from_state(&older);
        let mismatch = bump_anchor_repost_attempts_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Codex,
            older.channel_id,
            &older_identity,
            older.turn_start_offset,
        );
        assert_eq!(mismatch, GuardedSaveOutcome::IdentityMismatch);
        assert!(
            !send_permitted(mismatch),
            "a stranger-owned (IdentityMismatch) row MUST refuse the send-new"
        );
        let stranger = read_row(temp.path(), older.channel_id).expect("newer row persists");
        assert_eq!(stranger.user_msg_id, 999, "newer turn's row untouched");
        assert_eq!(
            stranger.anchor_repost_attempts, 0,
            "a blocked (mismatched) send must not consume the live turn's budget"
        );
    }
}
