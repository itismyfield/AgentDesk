//! #4259 PR-2a: identity-guarded save + outcome-conditional dirty policy for
//! the runtime-handoff loop's legacy tmux-wrapper `TmuxReady` stamp sites.
//! Split out of `runtime_handoff_loop.rs` so the parent stays below the giant
//! (>= 1000 prod LoC) threshold (codex r1).

use super::super::super::*;

/// Identity-guarded replacement for the legacy tmux-wrapper `TmuxReady` blind
/// `save_inflight_state`. The caller captures the expected 4-field identity
/// (`user_msg_id`, `started_at`, `tmux_session_name`, `turn_start_offset`)
/// before applying handoff mutations, so a decline means a concurrent turn
/// re-owned the channel between the snapshot and the write — exactly the
/// clobber this guard removes. `output_path` is NOT pinned
/// (restamp variant): a warm follow-up legitimately re-points the row at the
/// resolved legacy `/tmp` session path (`resolve_session_temp_path`;
/// claude.rs/codex.rs/qwen.rs follow-up arms), which differs from the intake
/// seed (codex r1 on the strict variant).
///
/// A declined save (`Missing` / `IdentityMismatch`) is surfaced with the
/// #4218 `channel_id` log key (the gate also logs the skip internally) and the
/// outcome is returned so the `TmuxReady` arm can keep its dirty marking
/// consistent with row ownership (see
/// [`tmux_ready_state_dirty_after_guarded_save`]).
pub(super) fn guarded_runtime_handoff_save(
    inflight_state: &InflightTurnState,
    expected: &crate::services::discord::inflight::InflightTurnIdentity,
    channel_id: ChannelId,
    caller: &'static str,
) -> crate::services::discord::inflight::GuardedSaveOutcome {
    use crate::services::discord::inflight::{
        GuardedSaveOutcome, save_inflight_state_if_identity_matches_allow_output_restamp,
    };
    let outcome = save_inflight_state_if_identity_matches_allow_output_restamp(
        inflight_state,
        expected,
        caller,
    );
    if matches!(
        outcome,
        GuardedSaveOutcome::Missing | GuardedSaveOutcome::IdentityMismatch
    ) {
        tracing::warn!(
            channel_id = channel_id.get(),
            caller,
            ?outcome,
            "inflight identity-guarded runtime-handoff save skipped; durable row no longer owned by this turn"
        );
    }
    outcome
}

/// Identity-guarded atomic stamp for runtime handoffs that may first-populate
/// `tmux_session_name`. The store validates the pre-mutation durable identity
/// and authority under the sidecar lock, then patches only runtime/session/
/// output/owner evidence. `Missing` never creates a row: every bridge entry
/// path seeds or adopts the durable row before a runtime handoff can arrive.
pub(super) fn guarded_runtime_atomic_stamp(
    inflight_state: &InflightTurnState,
    expected: &crate::services::discord::inflight::InflightTurnIdentity,
    channel_id: ChannelId,
    caller: &'static str,
) -> crate::services::discord::inflight::GuardedSaveOutcome {
    use crate::services::discord::inflight::{
        GuardedSaveOutcome, stamp_runtime_handoff_if_matches_identity,
    };
    let outcome = stamp_runtime_handoff_if_matches_identity(inflight_state, expected, caller);
    if matches!(
        outcome,
        GuardedSaveOutcome::Missing | GuardedSaveOutcome::IdentityMismatch
    ) {
        tracing::warn!(
            channel_id = channel_id.get(),
            caller,
            ?outcome,
            "runtime-handoff atomic stamp skipped; durable row no longer owned by this turn"
        );
    }
    outcome
}

/// #4259 PR-2a (codex r1): the `TmuxReady` arm's dirty marking, made
/// conditional on the guarded-save outcome. The arm used to end with an
/// unconditional `state_dirty = true`, which re-queued the arm's mutations for
/// the stream loop's periodic BLIND dirty flush (`stream_tick.rs`
/// `save_inflight_state` tail) — so a skipped guarded save was immediately
/// undone by the flush clobbering the re-owned row with the stale snapshot,
/// reducing the guard to decoration.
///
/// - `Saved` → mark dirty (legacy behavior; later mutations still flush).
/// - `IoError` → mark dirty (legacy retry semantics: the flush is the retry).
/// - `Missing` / `IdentityMismatch` → this turn no longer owns the row; do NOT
///   newly mark the arm's mutations dirty (a pre-existing dirty flag from
///   earlier loop work is preserved — clearing it could drop an unrelated
///   pending flush).
/// - `None` (no guarded save ran on this pass: non-unix, no-Http, standby
///   without Http) → mark dirty (legacy behavior).
pub(super) fn tmux_ready_state_dirty_after_guarded_save(
    previous_state_dirty: bool,
    outcome: Option<crate::services::discord::inflight::GuardedSaveOutcome>,
) -> bool {
    use crate::services::discord::inflight::GuardedSaveOutcome;
    match outcome {
        Some(GuardedSaveOutcome::Missing | GuardedSaveOutcome::IdentityMismatch) => {
            previous_state_dirty
        }
        Some(GuardedSaveOutcome::Saved | GuardedSaveOutcome::IoError) | None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::inflight::{GuardedSaveOutcome, load_inflight_state};

    /// Mirrors the legacy tmux-wrapper `TmuxReady` stamp: a real (non-id-0)
    /// Discord turn whose durable row carries the intake-seeded
    /// `tmux_session_name` + `output_path`. The handoff re-persists the same
    /// identity, possibly restamping `output_path` (warm follow-up).
    fn tmux_ready_owner_state(channel_id: u64, user_msg_id: u64) -> InflightTurnState {
        let tmux = "AgentDesk-codex-adk-4259";
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-4259".to_string()),
            343_742_347_365_974_026,
            user_msg_id,
            18,
            "user prompt".to_string(),
            Some("session".to_string()),
            Some(tmux.to_string()),
            Some(format!("/seeded/{tmux}.jsonl")),
            Some(format!("/seeded/{tmux}.input")),
            512,
        );
        state.last_offset = 512;
        state
    }

    // #4259 PR-2a rework (codex r1): a warm follow-up hands off with the
    // RESOLVED legacy /tmp output path, which differs from the intake seed —
    // the restamp-tolerant guard must still land the same-turn stamp.
    #[test]
    fn tmux_ready_guarded_save_restamps_output_path_when_this_turn_still_owns_the_row() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        let channel = ChannelId::new(4_259_001);
        let mut state = tmux_ready_owner_state(channel.get(), 77_010);
        save_inflight_state(&state).expect("seed owner row");
        let expected = crate::services::discord::inflight::InflightTurnIdentity::from_state(&state);

        state.output_path = Some("/tmp/AgentDesk-codex-adk-4259.jsonl".to_string());
        state.last_offset = 4096;
        let outcome = guarded_runtime_handoff_save(
            &state,
            &expected,
            channel,
            "turn_bridge::runtime_handoff_loop::tmux_ready_watcher_handoff",
        );
        assert_eq!(outcome, GuardedSaveOutcome::Saved);

        let persisted =
            load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted row");
        assert_eq!(
            persisted.output_path.as_deref(),
            Some("/tmp/AgentDesk-codex-adk-4259.jsonl"),
            "warm-followup output_path restamp must land on the normal path"
        );
        assert_eq!(persisted.last_offset, 4096);
    }

    // #4755: the wrapper must compare against the identity captured before any
    // handoff mutation. If a future mutation changes an identity field before
    // this call, the durable row must be matched with the pre-mutation identity.
    #[test]
    fn tmux_ready_guarded_save_uses_explicit_pre_mutation_identity() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        let channel = ChannelId::new(4_755_001);
        let mut state = tmux_ready_owner_state(channel.get(), 77_010);
        save_inflight_state(&state).expect("seed owner row");
        let expected = crate::services::discord::inflight::InflightTurnIdentity::from_state(&state);

        state.turn_start_offset = Some(4_096);
        state.last_offset = 4_096;
        let outcome = guarded_runtime_handoff_save(
            &state,
            &expected,
            channel,
            "turn_bridge::runtime_handoff_loop::tmux_ready_pre_mutation_identity",
        );
        assert_eq!(outcome, GuardedSaveOutcome::Saved);

        let persisted =
            load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted row");
        assert_eq!(persisted.turn_start_offset, Some(4_096));
        assert_eq!(persisted.last_offset, 4_096);
    }

    // #4259 PR-2a: the whole point of the guard — a CONCURRENT turn that
    // re-owned the channel between this turn's snapshot and its handoff write
    // must NOT be clobbered; the save skips with `IdentityMismatch`, leaves no
    // write, and the arm must not queue the stale snapshot for the blind dirty
    // flush either.
    #[test]
    fn tmux_ready_guarded_save_skips_and_does_not_clobber_a_re_owned_row() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        let channel = ChannelId::new(4_259_002);
        let snapshot = tmux_ready_owner_state(channel.get(), 77_010);
        let expected =
            crate::services::discord::inflight::InflightTurnIdentity::from_state(&snapshot);

        // A concurrent turn (different `user_msg_id`) re-owned the channel; its
        // row is on disk when this turn's stale handoff snapshot tries to write.
        let mut concurrent = tmux_ready_owner_state(channel.get(), 99_999);
        concurrent.last_offset = 8192;
        save_inflight_state(&concurrent).expect("seed re-owned row");

        let outcome = guarded_runtime_handoff_save(
            &snapshot,
            &expected,
            channel,
            "turn_bridge::runtime_handoff_loop::tmux_ready_watcher_handoff",
        );
        assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);

        let persisted =
            load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted row");
        assert_eq!(
            persisted.user_msg_id, 99_999,
            "concurrent turn's row must not be clobbered"
        );
        assert_eq!(persisted.last_offset, 8192);

        // codex r1: a skipped save must not re-arm the arm's dirty marking —
        // otherwise the stream_tick blind dirty flush would write the stale
        // snapshot anyway and the guard would be decoration.
        assert!(
            !tmux_ready_state_dirty_after_guarded_save(false, Some(outcome)),
            "mismatch outcome must not queue the stale snapshot for the dirty flush"
        );
    }

    // #4259 PR-2a (codex r1): outcome → dirty policy table. Missing/mismatch
    // never NEWLY mark dirty (but preserve an earlier mark); Saved/IoError/no
    // guarded save keep the legacy unconditional marking.
    #[test]
    fn tmux_ready_dirty_marking_follows_guarded_save_outcome() {
        use GuardedSaveOutcome::*;
        for lost in [Missing, IdentityMismatch] {
            assert!(!tmux_ready_state_dirty_after_guarded_save(
                false,
                Some(lost)
            ));
            assert!(
                tmux_ready_state_dirty_after_guarded_save(true, Some(lost)),
                "an earlier pending flush must not be dropped"
            );
        }
        for kept in [Some(Saved), Some(IoError), None] {
            assert!(tmux_ready_state_dirty_after_guarded_save(false, kept));
            assert!(tmux_ready_state_dirty_after_guarded_save(true, kept));
        }
    }
}
