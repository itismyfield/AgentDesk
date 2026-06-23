//! #3293 verify r1 (finding 1): restart-budget counter persistence that works
//! on restart-marked rows.
//!
//! The #3293 carrier row is re-marked `DrainRestart` by
//! `mark_all_inflight_states_restart_mode` on EVERY shutdown, so at recovery
//! time its on-disk form always has `restart_mode = Some(..)`. The generic
//! [`super::save_inflight_state_if_matches_identity`] guard refuses any write
//! to such a row (and to `user_msg_id == 0` TUI-direct rows), which made the
//! `PreserveAndCount` disposition a permanent no-op: `recovery_relay_attempts`
//! stayed 0 forever and `ClearBudgetExhausted` was unreachable on exactly the
//! row class the budget exists for.
//!
//! This module adds a narrow read-modify-write that increments ONLY the
//! counter on the on-disk row (preserving `restart_mode`, `rebind_origin`,
//! and every other field as-is), gated by the same strong turn identity
//! (`user_msg_id` + `started_at` + `tmux_session_name`, plus
//! `turn_start_offset` for TUI-direct disambiguation). The original guard's
//! purpose — never clobber a different turn's row — is kept: a newer turn or
//! a fresh rebind row has a different identity and is refused.

use std::fs;
use std::path::Path;

use super::{GuardedSaveOutcome, InflightTurnIdentity, InflightTurnState};
use crate::services::provider::ProviderKind;

/// Increment `recovery_relay_attempts` on the on-disk inflight row for
/// `(provider, channel_id)` when it still matches `expected`. Unlike the full
/// guarded save this NEVER rejects restart-marked / rebind-origin /
/// TUI-direct rows by marker alone — only identity decides, because the write
/// preserves the on-disk row (markers included) and bumps a single counter.
pub(in crate::services::discord) fn bump_recovery_relay_attempts_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(root) = super::inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    bump_recovery_relay_attempts_if_matches_identity_in_root(
        &root,
        provider,
        channel_id,
        expected,
        expected_turn_start_offset,
    )
}

/// Root-explicit inner form for unit tests (avoids `AGENTDESK_ROOT_DIR` env
/// races, same pattern as the other `_in_root` helpers in this subtree).
pub(in crate::services::discord) fn bump_recovery_relay_attempts_if_matches_identity_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let path = super::inflight_state_path(root, provider, channel_id);
    // Same sidecar flock as the rest of the module: hold it across the read
    // AND the write so a concurrent clear/save cannot interleave.
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
    // Strong identity: user_msg_id + started_at + tmux_session_name must all
    // match the turn whose relay just failed. Restart/rebind markers are NOT
    // grounds for refusal here — they are preserved verbatim below.
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
        // this is the same turn, so refuse rather than count a stranger's row.
        return GuardedSaveOutcome::IdentityMismatch;
    }
    on_disk.recovery_relay_attempts = on_disk.recovery_relay_attempts.saturating_add(1);
    on_disk.ensure_finalizer_turn_id();
    on_disk.updated_at = super::now_string();
    let Ok(json) = serde_json::to_string_pretty(&on_disk) else {
        return GuardedSaveOutcome::IoError;
    };
    match super::atomic_write(&path, &json) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                error = %error,
                "inflight relay-attempt bump failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

#[cfg(test)]
mod tests {
    //! Red-green coverage for the #3297 adversarial finding 1 blind spot:
    //! "counter increment WRITE succeeds on a restart-marked row".
    //!
    //! Hermetic by construction: rows are written/read with the root-explicit
    //! save + a direct file parse (NOT `load_inflight_states_from_root`, whose
    //! eviction side-effect consults the env-global generation counter — the
    //! #3293 scope-(a) parallel-test race this PR deliberately avoids adding
    //! to). `restart_mode` is set directly for the same reason
    //! (`set_restart_mode` stamps `restart_generation` from the env root).
    use std::path::Path;

    use tempfile::TempDir;

    use super::super::{
        GuardedSaveOutcome, InflightTurnIdentity, InflightTurnState, inflight_state_path,
        save_inflight_state_if_matches_identity_in_root, save_inflight_state_in_root,
    };
    use super::bump_recovery_relay_attempts_if_matches_identity_in_root;
    use crate::services::discord::InflightRestartMode;
    use crate::services::provider::ProviderKind;

    fn make_state(channel_id: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-cdx".to_string()),
            7,
            42,
            43,
            "hello".to_string(),
            Some("session-3293".to_string()),
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

    /// THE finding-1 red-green: the generic guarded save refuses a
    /// restart-marked row (red — the pre-fix path, attempts pinned at 0
    /// forever), while the dedicated bump persists the increment AND
    /// preserves the restart marker (green).
    #[test]
    fn bump_persists_attempts_on_restart_marked_row() {
        let temp = TempDir::new().unwrap();
        let mut state = make_state(932_941);
        state.restart_mode = Some(InflightRestartMode::DrainRestart);
        save_inflight_state_in_root(temp.path(), &state).expect("seed restart-marked row");
        let identity = InflightTurnIdentity::from_state(&state);

        // Pre-fix path: the generic identity-guarded save refuses the write
        // outright because `restart_mode.is_some()` — this is the exact
        // mechanism that kept the budget counter at 0 on the carrier row.
        let mut counted = state.clone();
        counted.recovery_relay_attempts = 1;
        assert_eq!(
            save_inflight_state_if_matches_identity_in_root(
                temp.path(),
                &counted,
                &identity,
                state.turn_start_offset,
            ),
            GuardedSaveOutcome::IdentityMismatch,
            "generic guarded save must still refuse restart-marked rows (its #3041 contract)"
        );

        // Fixed path: the dedicated bump lands on the same row.
        assert_eq!(
            bump_recovery_relay_attempts_if_matches_identity_in_root(
                temp.path(),
                &ProviderKind::Codex,
                state.channel_id,
                &identity,
                state.turn_start_offset,
            ),
            GuardedSaveOutcome::Saved,
            "the budget counter must persist on a restart-marked carrier row"
        );

        let row = read_row(temp.path(), state.channel_id).expect("row must remain on disk");
        assert_eq!(
            row.recovery_relay_attempts, 1,
            "attempts must be 1 after one bump"
        );
        assert_eq!(
            row.restart_mode,
            Some(InflightRestartMode::DrainRestart),
            "the restart marker must be preserved verbatim by the bump"
        );
    }

    /// Three consecutive boot failures must accumulate to the budget value —
    /// the counter is monotonic across re-marked boots, so
    /// `ClearBudgetExhausted` is actually reachable now.
    #[test]
    fn bump_accumulates_across_remarked_boots() {
        let temp = TempDir::new().unwrap();
        let mut state = make_state(932_942);
        state.restart_mode = Some(InflightRestartMode::DrainRestart);
        save_inflight_state_in_root(temp.path(), &state).expect("seed");
        let identity = InflightTurnIdentity::from_state(&state);

        for expected_attempts in 1..=3u32 {
            assert_eq!(
                bump_recovery_relay_attempts_if_matches_identity_in_root(
                    temp.path(),
                    &ProviderKind::Codex,
                    state.channel_id,
                    &identity,
                    state.turn_start_offset,
                ),
                GuardedSaveOutcome::Saved,
            );
            let row = read_row(temp.path(), state.channel_id).expect("row must remain");
            assert_eq!(row.recovery_relay_attempts, expected_attempts);
        }
    }

    /// TUI-direct (`user_msg_id == 0`) rows are countable too — the second
    /// half of finding 1 (the generic guard rejected them unconditionally).
    #[test]
    fn bump_persists_attempts_on_tui_direct_row() {
        let temp = TempDir::new().unwrap();
        let mut state = make_state(932_943);
        state.user_msg_id = 0;
        state.turn_start_offset = Some(512);
        save_inflight_state_in_root(temp.path(), &state).expect("seed");
        let identity = InflightTurnIdentity::from_state(&state);

        assert_eq!(
            bump_recovery_relay_attempts_if_matches_identity_in_root(
                temp.path(),
                &ProviderKind::Codex,
                state.channel_id,
                &identity,
                state.turn_start_offset,
            ),
            GuardedSaveOutcome::Saved,
        );
        let row = read_row(temp.path(), state.channel_id).expect("row must remain");
        assert_eq!(row.recovery_relay_attempts, 1);
    }

    /// A TUI-direct row whose offset cannot be proven equal must be refused —
    /// `started_at` alone (1s resolution) is not a strong enough identity.
    #[test]
    fn bump_refuses_tui_direct_row_without_offset_proof() {
        let temp = TempDir::new().unwrap();
        let mut state = make_state(932_944);
        state.user_msg_id = 0;
        state.turn_start_offset = Some(512);
        save_inflight_state_in_root(temp.path(), &state).expect("seed");
        let mut expected = InflightTurnIdentity::from_state(&state);
        expected.turn_start_offset = None;

        assert_eq!(
            bump_recovery_relay_attempts_if_matches_identity_in_root(
                temp.path(),
                &ProviderKind::Codex,
                state.channel_id,
                &expected,
                None,
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        let row = read_row(temp.path(), state.channel_id).expect("row must remain");
        assert_eq!(
            row.recovery_relay_attempts, 0,
            "stranger row must stay uncounted"
        );
    }

    /// Identity guard intact: a NEWER turn now owning the row is never
    /// counted (the original guard's core purpose, preserved).
    #[test]
    fn bump_refuses_row_owned_by_newer_turn() {
        let temp = TempDir::new().unwrap();
        let mut newer = make_state(932_945);
        newer.user_msg_id = 999;
        save_inflight_state_in_root(temp.path(), &newer).expect("seed newer turn");

        let mut older = make_state(932_945);
        older.user_msg_id = 777;
        let identity = InflightTurnIdentity::from_state(&older);

        assert_eq!(
            bump_recovery_relay_attempts_if_matches_identity_in_root(
                temp.path(),
                &ProviderKind::Codex,
                older.channel_id,
                &identity,
                older.turn_start_offset,
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        let row = read_row(temp.path(), older.channel_id).expect("row must remain");
        assert_eq!(row.user_msg_id, 999, "newer turn's row must be untouched");
        assert_eq!(row.recovery_relay_attempts, 0);
    }

    /// A cleared (missing) row is never resurrected by the bump.
    #[test]
    fn bump_does_not_resurrect_missing_row() {
        let temp = TempDir::new().unwrap();
        let state = make_state(932_946);
        let identity = InflightTurnIdentity::from_state(&state);

        assert_eq!(
            bump_recovery_relay_attempts_if_matches_identity_in_root(
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
            "bump must not create a row"
        );
    }
}
