//! Inflight store-side CAS "save" cluster (#3835 extraction).
//!
//! The compare-and-set write half of the inflight sidecar contract: fresh-row
//! creation, absent-guarded seeding, and the identity-guarded save / rebind-
//! adoption variants. Moved verbatim out of `inflight.rs` so the hot state
//! parent stays below the frozen production-LoC baseline without changing any
//! call-site name. The parent re-exports every public symbol at its original
//! visibility, so `inflight::*` flat paths stay byte-identical for discord-
//! module / inflight-core callers. The `_in_root` explicit-root seams keep the
//! `pub(super)` visibility the parent's tests (and sibling `budget` /
//! `anchor_repost` test modules) reach via re-import. Offset-monotonic /
//! identity-guard invariants are unchanged (pure move).

use super::*;

#[path = "save_store/create_monotonic_observer.rs"]
mod create_monotonic_observer;

#[path = "save_store/delivery_rewind.rs"]
mod delivery_rewind;
#[path = "save_store/identity_gate.rs"]
pub(super) mod identity_gate;
#[path = "save_store/rebind_adoption.rs"]
mod rebind_adoption;

pub(in crate::services::discord) use self::delivery_rewind::save_inflight_delivery_rewind_if_matches_identity;
pub(in crate::services::discord) use self::identity_gate::{
    GuardedSaveOutcome, StreamRelayAuthority, bind_recovery_anchor_if_matches_identity,
    clear_long_running_placeholder_if_matches_identity,
    mark_readopted_from_inflight_if_identity_unchanged,
    patch_bridge_entry_state_if_identity_unchanged,
    patch_bridge_entry_state_tracking_placeholder_clear,
    patch_restart_full_response_if_identity_unchanged, patch_restart_mode_if_matches_identity,
    persist_leak_recovery_response_offset_if_matches_identity_locked,
    persist_recovery_output_path_if_matches_identity_locked,
    recovery_anchor_message_if_matches_identity, recovery_anchor_msg_id_if_matches_identity,
    save_inflight_state_if_identity_matches_allow_output_restamp,
    save_inflight_state_if_identity_unchanged, save_inflight_state_if_matches_identity,
    save_stream_tick_state_if_bridge_authority,
    save_stream_tick_state_preserving_current_message_races,
    stamp_claude_e_process_if_matches_identity, stamp_runtime_handoff_if_matches_identity,
    touch_inflight_state_if_matches_identity,
};
pub(in crate::services::discord) use self::rebind_adoption::{
    save_existing_inflight_rebind_adoption_if_matches_episode,
    save_existing_inflight_rebind_adoption_if_matches_identity,
    save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_episode,
    save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity,
};

#[cfg(test)]
use self::identity_gate::{
    patch_bridge_entry_state_if_identity_unchanged_in_root,
    save_inflight_state_if_identity_matches_allow_output_restamp_in_root,
    save_inflight_state_if_identity_unchanged_in_root,
};
#[cfg(test)]
pub(super) use self::identity_gate::{
    save_existing_inflight_rebind_adoption_if_matches_identity_in_root,
    save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root,
    save_inflight_state_if_matches_identity_in_root,
};
#[cfg(test)]
#[path = "save_store/bridge_entry_guard_tests.rs"]
mod bridge_entry_guard_tests;

/// Blind whole-blob write of `InflightTurnState`: serializes the ENTIRE row and
/// clobbers whatever is on disk, with no compare-and-set on turn identity.
///
/// TEST SEED ONLY (#4259 R4): the production symbol is compile-time absent.
/// Runtime writers must use a create-shaped API or a lock-held narrow RMW seam;
/// identity checking alone does not make a stale whole-row snapshot safe.
#[cfg(test)]
pub(in crate::services::discord) fn save_inflight_state(
    state: &InflightTurnState,
) -> Result<(), String> {
    let Some(root) = inflight_runtime_root() else {
        return Err("Home directory not found".to_string());
    };
    save_inflight_state_in_root(&root, state)
}

/// #897 counter-model review P2 #1 — atomic "create, don't overwrite"
/// variant of `save_inflight_state`. Used by `POST /api/inflight/rebind` so a
/// concurrent legitimate turn that wins the mailbox race between the rebind
/// handler's existence check and its write cannot have its canonical
/// inflight file silently overwritten by the synthetic rebind state
/// (`user_msg_id=0`, placeholder ids zeroed). Returns `InflightAlreadyExists`
/// when the target path is already occupied — the handler translates that
/// into HTTP 409 and the operator retries (or leaves it to the live turn).
#[derive(Debug)]
pub(in crate::services::discord) enum CreateNewInflightError {
    /// A state file already exists at the target path — another path wrote
    /// it between the caller's preflight check and this call.
    AlreadyExists,
    /// Filesystem or serialization failure.
    Internal(String),
}

impl std::fmt::Display for CreateNewInflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyExists => write!(f, "inflight state already exists"),
            Self::Internal(msg) => write!(f, "{msg}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::InflightRestartMode;
    use crate::services::provider::ProviderKind;

    struct EnvReset(Option<std::ffi::OsString>);

    impl Drop for EnvReset {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn set_runtime_root() -> (tempfile::TempDir, EnvReset) {
        let reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::TempDir::new().expect("runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        (temp, reset)
    }

    fn id0_offsetless_state(channel_id: u64) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-test".to_string()),
            343_742_347_365_974_026,
            0,
            77_000,
            "recover this".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-codex-adk-test".to_string()),
            Some("/tmp/recovery-idempotent.jsonl".to_string()),
            None,
            128,
        );
        state.turn_start_offset = None;
        state
    }

    fn api_friction_response_pair() -> (String, String) {
        let raw = concat!(
            "Visible answer before marker.\n",
            "API_FRICTION: {\"endpoint\":\"GET /api/docs\",",
            "\"friction_type\":\"missing_docs\",",
            "\"summary\":\"docs endpoint omitted the category\",",
            "\"workaround\":\"used live route metadata\",",
            "\"suggested_fix\":\"document the category response\"}\n",
            "Visible answer after marker."
        )
        .to_string();
        let extracted = crate::services::api_friction::extract_api_friction_reports(&raw);
        assert_eq!(extracted.reports.len(), 1);
        assert!(extracted.parse_errors.is_empty());
        assert!(!extracted.cleaned_response.contains("API_FRICTION:"));
        (raw, extracted.cleaned_response)
    }

    fn api_friction_response_with_leading_blanks_pair() -> (String, String) {
        let raw = concat!(
            "\n",
            "\n",
            "Visible answer before marker.\n",
            "API_FRICTION: {\"endpoint\":\"GET /api/docs\",",
            "\"friction_type\":\"missing_docs\",",
            "\"summary\":\"docs endpoint omitted the category\"}\n",
            "Visible answer after marker."
        )
        .to_string();
        let extracted = crate::services::api_friction::extract_api_friction_reports(&raw);
        assert_eq!(extracted.reports.len(), 1);
        assert!(extracted.parse_errors.is_empty());
        assert!(!extracted.cleaned_response.contains("API_FRICTION:"));
        assert!(!extracted.cleaned_response.starts_with('\n'));
        (raw, extracted.cleaned_response)
    }

    fn state_with_full_response(
        channel_id: u64,
        full_response: &str,
        tmux_session_name: &str,
    ) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-test".to_string()),
            343_742_347_365_974_026,
            77_010,
            18,
            "user prompt".to_string(),
            Some("session".to_string()),
            Some(tmux_session_name.to_string()),
            Some(format!("/tmp/{tmux_session_name}.jsonl")),
            None,
            512,
        );
        state.full_response = full_response.to_string();
        state.response_sent_offset = 0;
        state
    }

    #[test]
    fn restart_full_response_patch_strips_api_friction_after_guarded_save_declines() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        let (raw, cleaned) = api_friction_response_pair();
        let mut state =
            state_with_full_response(44_085, &raw, "AgentDesk-codex-restart-clean-4185");
        state.set_restart_mode(InflightRestartMode::DrainRestart);
        save_inflight_state(&state).expect("seed raw restart-preserved row");

        let mut cleaned_snapshot = state.clone();
        cleaned_snapshot.full_response = cleaned.clone();
        assert_eq!(
            save_inflight_state_if_identity_unchanged(
                &cleaned_snapshot,
                "test::restart_cleaned_guarded_save_declines",
            ),
            GuardedSaveOutcome::IdentityMismatch,
            "the broad identity-refresh save must keep refusing restart-owned rows"
        );
        assert_eq!(
            patch_restart_full_response_if_identity_unchanged(
                &cleaned_snapshot,
                "test::restart_cleaned_patch",
            ),
            GuardedSaveOutcome::Saved
        );

        let persisted = super::super::load_inflight_state(&ProviderKind::Codex, state.channel_id)
            .expect("persisted restart row");
        assert_eq!(persisted.full_response, cleaned);
        assert!(!persisted.full_response.contains("API_FRICTION:"));
        assert_ne!(persisted.full_response, raw);
        assert_eq!(
            persisted.restart_mode,
            Some(InflightRestartMode::DrainRestart)
        );
        assert_eq!(persisted.current_msg_id, state.current_msg_id);
        assert_eq!(persisted.response_sent_offset, state.response_sent_offset);
    }

    #[test]
    fn restart_full_response_patch_declines_when_cleaning_changes_relayed_prefix() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        let (raw, cleaned) = api_friction_response_with_leading_blanks_pair();
        let response_sent_offset = raw.find("Visible answer").expect("visible prefix");
        assert!(response_sent_offset > 0);
        assert_ne!(
            &raw.as_bytes()[..response_sent_offset],
            &cleaned.as_bytes()[..response_sent_offset]
        );

        let mut state =
            state_with_full_response(44_087, &raw, "AgentDesk-codex-restart-prefix-decline-4185");
        state.set_restart_mode(InflightRestartMode::DrainRestart);
        state.response_sent_offset = response_sent_offset;
        save_inflight_state(&state).expect("seed raw restart-preserved row");

        let mut cleaned_snapshot = state.clone();
        cleaned_snapshot.full_response = cleaned;
        assert_eq!(
            patch_restart_full_response_if_identity_unchanged(
                &cleaned_snapshot,
                "test::restart_cleaned_patch_prefix_declines",
            ),
            GuardedSaveOutcome::IdentityMismatch
        );

        let persisted = super::super::load_inflight_state(&ProviderKind::Codex, state.channel_id)
            .expect("persisted restart row");
        assert_eq!(persisted.full_response, raw);
        assert_eq!(persisted.response_sent_offset, response_sent_offset);
        assert_eq!(
            persisted.restart_mode,
            Some(InflightRestartMode::DrainRestart)
        );
    }

    #[test]
    fn restart_full_response_patch_saves_when_cleaning_keeps_relayed_prefix() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        let (raw, cleaned) = api_friction_response_pair();
        let response_sent_offset = raw.find("API_FRICTION:").expect("marker offset");
        assert!(response_sent_offset > 0);
        assert_eq!(
            &raw.as_bytes()[..response_sent_offset],
            &cleaned.as_bytes()[..response_sent_offset]
        );

        let mut state =
            state_with_full_response(44_088, &raw, "AgentDesk-codex-restart-prefix-save-4185");
        state.set_restart_mode(InflightRestartMode::DrainRestart);
        state.response_sent_offset = response_sent_offset;
        save_inflight_state(&state).expect("seed raw restart-preserved row");

        let mut cleaned_snapshot = state.clone();
        cleaned_snapshot.full_response = cleaned.clone();
        assert_eq!(
            patch_restart_full_response_if_identity_unchanged(
                &cleaned_snapshot,
                "test::restart_cleaned_patch_prefix_saves",
            ),
            GuardedSaveOutcome::Saved
        );

        let persisted = super::super::load_inflight_state(&ProviderKind::Codex, state.channel_id)
            .expect("persisted restart row");
        assert_eq!(persisted.full_response, cleaned);
        assert_eq!(persisted.response_sent_offset, response_sent_offset);
        assert_eq!(
            persisted.restart_mode,
            Some(InflightRestartMode::DrainRestart)
        );
    }

    #[test]
    fn non_restart_identity_refresh_save_still_persists_cleaned_api_friction_text() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        let (raw, cleaned) = api_friction_response_pair();
        let mut state = state_with_full_response(44_086, &raw, "AgentDesk-codex-normal-clean-4185");
        save_inflight_state(&state).expect("seed raw normal row");
        state = super::super::load_inflight_state(&ProviderKind::Codex, state.channel_id)
            .expect("load exact seeded normal row");

        state.full_response = cleaned.clone();
        assert_eq!(
            save_inflight_state_if_identity_unchanged(
                &mut state,
                "test::normal_cleaned_identity_refresh",
            ),
            GuardedSaveOutcome::Saved
        );
        assert_eq!(
            patch_restart_full_response_if_identity_unchanged(
                &state,
                "test::normal_cleaned_restart_patch_refuses",
            ),
            GuardedSaveOutcome::IdentityMismatch,
            "the restart-only patch must not participate in ordinary rows"
        );

        let persisted = super::super::load_inflight_state(&ProviderKind::Codex, state.channel_id)
            .expect("persisted normal row");
        assert_eq!(persisted.full_response, cleaned);
        assert!(!persisted.full_response.contains("API_FRICTION:"));
        assert!(persisted.restart_mode.is_none());
    }

    #[test]
    fn id0_offsetless_anchor_row_never_matches_for_reuse_or_bind() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = id0_offsetless_state(44_006);
        let identity = InflightTurnIdentity::from_state(&state);
        save_inflight_state(&state).expect("save offsetless id-0 row");

        assert_eq!(
            recovery_anchor_msg_id_if_matches_identity(
                &provider,
                state.channel_id,
                &identity,
                state.turn_start_offset,
            ),
            None,
            "id-0 anchor reuse must fail closed when the row lacks turn_start_offset"
        );
        assert_eq!(
            bind_recovery_anchor_if_matches_identity(
                &provider,
                state.channel_id,
                &identity,
                state.turn_start_offset,
                state.current_msg_id,
                Some(state.current_msg_len),
                88_006,
                11,
                None,
                None,
            ),
            GuardedSaveOutcome::IdentityMismatch,
            "id-0 anchor bind must fail closed when either side lacks turn_start_offset"
        );
        assert_eq!(
            super::super::load_inflight_state(&provider, state.channel_id)
                .expect("persisted row")
                .current_msg_id,
            state.current_msg_id,
            "failed bind must not mutate the offsetless id-0 row"
        );
    }
    #[test]
    fn id0_offsetless_identity_refresh_save_fails_closed() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let provider = ProviderKind::Codex;
        let mut state = id0_offsetless_state(44_007);
        state.full_response = "durable response".to_string();
        save_inflight_state_in_root(temp.path(), &state).expect("seed offsetless id-0 row");

        let mut stale_snapshot = state.clone();
        stale_snapshot.full_response = "stale response".to_string();
        stale_snapshot.response_sent_offset = stale_snapshot.full_response.len();
        let outcome = save_inflight_state_if_identity_unchanged_in_root(
            temp.path(),
            &stale_snapshot,
            "test::id0_offsetless_identity_refresh_save_fails_closed",
        );

        assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
        let persisted_path = inflight_state_path(temp.path(), &provider, state.channel_id);
        let persisted: InflightTurnState = serde_json::from_str(
            &std::fs::read_to_string(persisted_path).expect("read persisted inflight"),
        )
        .expect("parse persisted inflight");
        assert_eq!(persisted.full_response, "durable response");
        assert_eq!(persisted.response_sent_offset, 0);
    }

    #[test]
    fn id0_with_turn_start_offset_identity_refresh_save_succeeds_when_durable_matches() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let provider = ProviderKind::Codex;
        let mut state = InflightTurnState::new(
            provider.clone(),
            44_008,
            Some("adk-test".to_string()),
            343_742_347_365_974_026,
            0,
            77_002,
            "recover this".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-codex-id0-offset-save".to_string()),
            Some("/tmp/id0-offset-save.jsonl".to_string()),
            None,
            256,
        );
        assert_eq!(state.user_msg_id, 0);
        assert_eq!(state.turn_start_offset, Some(256));
        save_inflight_state_in_root(temp.path(), &state).expect("seed matching id-0 row");
        let persisted_path = inflight_state_path(temp.path(), &provider, state.channel_id);
        state = serde_json::from_str(
            &std::fs::read_to_string(&persisted_path).expect("reload exact seeded id-0 row"),
        )
        .expect("parse exact seeded id-0 row");

        state.full_response = "id-0 durable owner refresh".to_string();
        state.response_sent_offset = state.full_response.len();
        assert_eq!(
            save_inflight_state_if_identity_unchanged_in_root(
                temp.path(),
                &state,
                "test::id0_with_turn_start_offset_identity_refresh_save_succeeds_when_durable_matches",
            ),
            GuardedSaveOutcome::Saved
        );

        let persisted: InflightTurnState = serde_json::from_str(
            &std::fs::read_to_string(persisted_path).expect("read persisted inflight"),
        )
        .expect("parse persisted inflight");
        assert_eq!(
            persisted.full_response, "id-0 durable owner refresh",
            "legitimate id-0 TUI-direct turns with a turn_start_offset still own the row"
        );
        assert_eq!(persisted.response_sent_offset, state.response_sent_offset);
    }

    // #4259 PR-2a rework (codex r1): a warm follow-up runtime handoff
    // legitimately re-points `output_path` at the resolved legacy /tmp session
    // path — the restamp variant must accept the same-identity restamp and land
    // the NEW path, where `_if_identity_unchanged` would decline it.
    #[test]
    fn output_restamp_save_persists_new_output_path_when_identity_matches() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let provider = ProviderKind::Codex;
        let mut state = state_with_full_response(44_090, "seeded", "AgentDesk-codex-restamp-4259");
        save_inflight_state_in_root(temp.path(), &state).expect("seed intake-path row");
        let persisted_path = inflight_state_path(temp.path(), &provider, state.channel_id);
        state = serde_json::from_str(
            &std::fs::read_to_string(&persisted_path).expect("reload exact seeded row"),
        )
        .expect("parse exact seeded row");

        let expected = InflightTurnIdentity::from_state(&state);
        let seeded_output_path = state.output_path.clone();
        state.output_path = Some("/tmp/legacy/AgentDesk-codex-restamp-4259.jsonl".to_string());
        state.last_offset = 4096;
        assert_ne!(
            state.output_path, seeded_output_path,
            "fixture must exercise a genuine output_path restamp"
        );
        assert_eq!(
            save_inflight_state_if_identity_unchanged_in_root(
                temp.path(),
                &state,
                "test::output_restamp_strict_variant_declines",
            ),
            GuardedSaveOutcome::IdentityMismatch,
            "the strict variant must keep declining output_path drift"
        );
        assert_eq!(
            save_inflight_state_if_identity_matches_allow_output_restamp_in_root(
                temp.path(),
                &state,
                &expected,
                "test::output_restamp_saves",
            ),
            GuardedSaveOutcome::Saved
        );

        let persisted: InflightTurnState = serde_json::from_str(
            &std::fs::read_to_string(persisted_path).expect("read persisted inflight"),
        )
        .expect("parse persisted inflight");
        assert_eq!(
            persisted.output_path.as_deref(),
            Some("/tmp/legacy/AgentDesk-codex-restamp-4259.jsonl"),
            "restamped output_path must land"
        );
        assert_eq!(persisted.last_offset, 4096);
    }

    // #4259 PR-2a rework (codex r1): the restamp variant still pins the 4-field
    // turn identity — a row re-owned by another turn is never clobbered.
    #[test]
    fn output_restamp_save_declines_when_another_turn_owns_the_row() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let provider = ProviderKind::Codex;
        let mut owner =
            state_with_full_response(44_091, "owner response", "AgentDesk-codex-restamp-own-4259");
        owner.user_msg_id = 88_001;
        save_inflight_state_in_root(temp.path(), &owner).expect("seed re-owned row");

        let mut stale =
            state_with_full_response(44_091, "stale snapshot", "AgentDesk-codex-restamp-own-4259");
        stale.user_msg_id = 77_010;
        let expected = InflightTurnIdentity::from_state(&stale);
        stale.output_path = Some("/tmp/legacy/AgentDesk-codex-restamp-own-4259.jsonl".to_string());
        assert_eq!(
            save_inflight_state_if_identity_matches_allow_output_restamp_in_root(
                temp.path(),
                &stale,
                &expected,
                "test::output_restamp_identity_mismatch_skips",
            ),
            GuardedSaveOutcome::IdentityMismatch
        );

        let persisted_path = inflight_state_path(temp.path(), &provider, owner.channel_id);
        let persisted: InflightTurnState = serde_json::from_str(
            &std::fs::read_to_string(persisted_path).expect("read persisted inflight"),
        )
        .expect("parse persisted inflight");
        assert_eq!(persisted.user_msg_id, 88_001);
        assert_eq!(persisted.full_response, "owner response");
        assert_eq!(persisted.output_path, owner.output_path);
    }

    #[test]
    fn existing_claude_transcript_adoption_rebase_save_persists_eof_coordinates_and_runtime() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let provider = ProviderKind::Claude;
        let wrapper_path = temp.path().join("wrapper.jsonl");
        let transcript_path = temp
            .path()
            .join("88fdb7f3-0000-4000-8000-000000000000.jsonl");
        std::fs::write(&wrapper_path, vec![b'w'; 128]).expect("write wrapper");
        std::fs::write(&transcript_path, vec![b't'; 512_000]).expect("write transcript");
        let transcript_eof = std::fs::metadata(&transcript_path).unwrap().len();
        let transcript_session_id = "88fdb7f3-0000-4000-8000-000000000000";
        let channel_id = 44_153_001;
        let mut existing = InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("adk-cc".to_string()),
            123,
            456,
            789,
            "continue".to_string(),
            Some("old-wrapper-session".to_string()),
            Some("AgentDesk-claude-adoption-rebase-save-44153001".to_string()),
            Some(wrapper_path.display().to_string()),
            Some("/tmp/wrapper.input".to_string()),
            128,
        );
        existing.turn_start_offset = Some(64);
        existing.last_watcher_relayed_offset = Some(96);
        existing.last_watcher_relayed_generation_mtime_ns = Some(123_456);
        save_inflight_state_in_root(temp.path(), &existing).expect("seed existing inflight");
        let expected = InflightTurnIdentity::from_state(&existing);
        let expected_turn_start_offset = existing.turn_start_offset;
        let expected_last_offset = existing.last_offset;

        let mut adopted = existing.clone();
        adopted.output_path = Some(transcript_path.display().to_string());
        adopted.input_fifo_path = None;
        adopted.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        adopted.session_id = Some(transcript_session_id.to_string());
        adopted.last_offset = transcript_eof;
        adopted.turn_start_offset = Some(transcript_eof);
        adopted.last_watcher_relayed_offset = None;
        adopted.last_watcher_relayed_generation_mtime_ns = None;
        adopted.set_relay_owner_kind(RelayOwnerKind::Watcher);

        assert_eq!(
            save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root(
                temp.path(),
                &adopted,
                &expected,
                expected_turn_start_offset,
                expected_last_offset,
            ),
            GuardedSaveOutcome::Saved,
        );

        let persisted_path = inflight_state_path(temp.path(), &provider, channel_id);
        let persisted: InflightTurnState = serde_json::from_str(
            &std::fs::read_to_string(persisted_path).expect("read persisted inflight"),
        )
        .expect("parse persisted inflight");
        assert_eq!(
            persisted.output_path,
            Some(transcript_path.display().to_string())
        );
        assert_eq!(persisted.input_fifo_path, None);
        assert_eq!(persisted.last_offset, transcript_eof);
        assert_eq!(persisted.turn_start_offset, Some(transcript_eof));
        assert_eq!(persisted.last_watcher_relayed_offset, None);
        assert_eq!(persisted.last_watcher_relayed_generation_mtime_ns, None);
        assert_eq!(
            persisted.runtime_kind,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui),
        );
        assert_eq!(persisted.session_id.as_deref(), Some(transcript_session_id));
        assert_eq!(
            persisted.effective_relay_owner_kind(),
            RelayOwnerKind::Watcher
        );
    }

    #[test]
    fn identity_unchanged_save_skips_after_adoption_rebased_turn_start_offset() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let provider = ProviderKind::Claude;
        let wrapper_path = temp.path().join("wrapper.jsonl");
        let transcript_path = temp.path().join("adopted-transcript.jsonl");
        std::fs::write(&wrapper_path, vec![b'w'; 128]).expect("write wrapper");
        std::fs::write(&transcript_path, vec![b't'; 4096]).expect("write transcript");
        let transcript_eof = std::fs::metadata(&transcript_path).unwrap().len();
        let channel_id = 44_153_004;
        let mut stale_snapshot = InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("adk-cc".to_string()),
            123,
            456,
            789,
            "continue".to_string(),
            Some("old-wrapper-session".to_string()),
            Some("AgentDesk-claude-identity-refresh-skip-44153004".to_string()),
            Some(wrapper_path.display().to_string()),
            Some("/tmp/wrapper.input".to_string()),
            128,
        );
        stale_snapshot.turn_start_offset = Some(64);
        stale_snapshot.full_response = "stale response".to_string();
        save_inflight_state_in_root(temp.path(), &stale_snapshot).expect("seed stale snapshot row");

        let mut adopted = stale_snapshot.clone();
        adopted.output_path = Some(transcript_path.display().to_string());
        adopted.input_fifo_path = None;
        adopted.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        adopted.session_id = Some("88fdb7f3-0000-4000-8000-000000000000".to_string());
        adopted.last_offset = transcript_eof;
        adopted.turn_start_offset = Some(transcript_eof);
        adopted.last_watcher_relayed_offset = None;
        adopted.last_watcher_relayed_generation_mtime_ns = None;
        adopted.full_response = "rebased durable response".to_string();
        save_inflight_state_in_root(temp.path(), &adopted).expect("persist adopted row");

        stale_snapshot.response_sent_offset = stale_snapshot.full_response.len();
        stale_snapshot.terminal_delivery_committed = true;
        let outcome = save_inflight_state_if_identity_unchanged_in_root(
            temp.path(),
            &stale_snapshot,
            "test::identity_unchanged_save_skips_after_adoption_rebased_turn_start_offset",
        );

        assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
        let persisted_path = inflight_state_path(temp.path(), &provider, channel_id);
        let persisted: InflightTurnState = serde_json::from_str(
            &std::fs::read_to_string(persisted_path).expect("read persisted inflight"),
        )
        .expect("parse persisted inflight");
        assert_eq!(
            persisted.output_path,
            Some(transcript_path.display().to_string())
        );
        assert_eq!(persisted.last_offset, transcript_eof);
        assert_eq!(persisted.turn_start_offset, Some(transcript_eof));
        assert!(!persisted.terminal_delivery_committed);
        assert_ne!(persisted.full_response, "stale response");
    }
    #[test]
    fn identity_unchanged_save_skips_after_adoption_changes_only_output_path() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let provider = ProviderKind::Codex;
        let old_rollout_path = temp.path().join("old-rollout.jsonl");
        let adopted_rollout_path = temp.path().join("adopted-rollout.jsonl");
        std::fs::write(&old_rollout_path, vec![b'o'; 256]).expect("write old rollout");
        std::fs::write(&adopted_rollout_path, vec![b'a'; 1024]).expect("write adopted rollout");
        let channel_id = 44_153_005;
        let mut stale_snapshot = InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("adk-cc".to_string()),
            123,
            456,
            789,
            "continue".to_string(),
            Some("codex-session".to_string()),
            Some("AgentDesk-codex-output-only-adoption-44153005".to_string()),
            Some(old_rollout_path.display().to_string()),
            None,
            256,
        );
        stale_snapshot.turn_start_offset = Some(128);
        stale_snapshot.full_response = "stale response".to_string();
        save_inflight_state_in_root(temp.path(), &stale_snapshot).expect("seed stale snapshot row");

        let mut adopted = stale_snapshot.clone();
        adopted.output_path = Some(adopted_rollout_path.display().to_string());
        adopted.full_response = "adopted durable response".to_string();
        save_inflight_state_in_root(temp.path(), &adopted).expect("persist adopted row");

        stale_snapshot.response_sent_offset = stale_snapshot.full_response.len();
        stale_snapshot.terminal_delivery_committed = true;
        let outcome = save_inflight_state_if_identity_unchanged_in_root(
            temp.path(),
            &stale_snapshot,
            "test::identity_unchanged_save_skips_after_adoption_changes_only_output_path",
        );

        assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
        let persisted_path = inflight_state_path(temp.path(), &provider, channel_id);
        let persisted: InflightTurnState = serde_json::from_str(
            &std::fs::read_to_string(persisted_path).expect("read persisted inflight"),
        )
        .expect("parse persisted inflight");
        assert_eq!(
            persisted.output_path,
            Some(adopted_rollout_path.display().to_string())
        );
        assert_eq!(persisted.full_response, "adopted durable response");
        assert!(!persisted.terminal_delivery_committed);
    }

    #[test]
    fn sidecar_guard_drop_releases_lock_for_next_caller() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let path = inflight_state_path(temp.path(), &ProviderKind::Codex, 44_153_007);
        let guard = lock_inflight_state_path(&path).expect("first guard");
        assert!(matches!(
            second_handle_try_lock(&path),
            Err(std::fs::TryLockError::WouldBlock)
        ));

        drop(guard);
        assert!(
            path.with_extension("json.lock").exists(),
            "releasing a guard must not unlink the canonical sidecar"
        );

        let next_guard = lock_inflight_state_path(&path).expect("lock after guard drop");
        drop(next_guard);
    }

    #[test]
    fn create_new_atomic_write_failure_leaves_no_final_row_and_retry_succeeds() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let state = id0_offsetless_state(44_153_009);
        let path = inflight_state_path(temp.path(), &ProviderKind::Codex, state.channel_id);

        let result = save_inflight_state_create_new_in_root_with_write(
            temp.path(),
            &state,
            |final_path, data| {
                let tmp = final_path.with_extension("json.injected-pre-rename.tmp");
                std::fs::write(&tmp, data).map_err(|error| error.to_string())?;
                Err("injected failure before final rename".to_string())
            },
        );
        assert!(matches!(result, Err(CreateNewInflightError::Internal(_))));
        assert!(
            !path.exists(),
            "a pre-rename atomic-write failure must not publish the final row"
        );

        save_inflight_state_create_new_in_root(temp.path(), &state)
            .expect("retry create-new after pre-rename failure");
        let persisted: InflightTurnState = serde_json::from_str(
            &std::fs::read_to_string(path).expect("read retried inflight row"),
        )
        .expect("parse retried inflight row");
        assert_eq!(persisted.channel_id, state.channel_id);
        assert_eq!(persisted.user_msg_id, state.user_msg_id);
    }

    #[test]
    fn create_new_existence_probe_error_fails_closed_without_writing() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let create_new = id0_offsetless_state(44_153_010);
        let create_new_path =
            inflight_state_path(temp.path(), &ProviderKind::Codex, create_new.channel_id);
        let create_new_write_called = std::cell::Cell::new(false);

        let create_new_result = save_inflight_state_create_new_in_root_with_io(
            temp.path(),
            &create_new,
            |_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            |_, _| {
                create_new_write_called.set(true);
                Ok(())
            },
        );

        assert!(matches!(
            create_new_result,
            Err(CreateNewInflightError::Internal(_))
        ));
        assert!(
            !create_new_write_called.get(),
            "create-new metadata failure must not reach the writer"
        );
        assert!(
            !create_new_path.exists(),
            "create-new metadata failure must not publish a final row"
        );

        let if_absent = id0_offsetless_state(44_153_011);
        let if_absent_path =
            inflight_state_path(temp.path(), &ProviderKind::Codex, if_absent.channel_id);
        let if_absent_write_called = std::cell::Cell::new(false);
        let if_absent_result = save_inflight_state_if_absent_in_root_with_io(
            temp.path(),
            &if_absent,
            |_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            |_, _| {
                if_absent_write_called.set(true);
                Ok(())
            },
        );

        assert!(if_absent_result.is_err());
        assert!(
            !if_absent_write_called.get(),
            "if-absent metadata failure must not reach the writer"
        );
        assert!(
            !if_absent_path.exists(),
            "if-absent metadata failure must not publish a final row"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let occupied = id0_offsetless_state(44_153_012);
            let occupied_path =
                inflight_state_path(temp.path(), &ProviderKind::Codex, occupied.channel_id);
            let dangling_target = temp.path().join("missing-inflight-target.json");
            symlink(&dangling_target, &occupied_path).expect("create dangling final-name symlink");

            let create_new_result = save_inflight_state_create_new_in_root(temp.path(), &occupied);
            assert!(matches!(
                create_new_result,
                Err(CreateNewInflightError::AlreadyExists)
            ));
            assert_eq!(
                std::fs::read_link(&occupied_path).expect("create-new preserved dangling symlink"),
                dangling_target
            );

            assert!(
                !save_inflight_state_if_absent_in_root(temp.path(), &occupied)
                    .expect("if-absent sees occupied final name")
            );
            assert_eq!(
                std::fs::read_link(&occupied_path).expect("if-absent preserved dangling symlink"),
                dangling_target
            );
        }
    }

    #[test]
    fn if_absent_and_create_new_cannot_both_claim_absent_or_overwrite_the_winner() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let root = temp.path().to_path_buf();
        let channel_id = 44_153_008;
        let conditional = id0_offsetless_state(channel_id);
        let mut create_new = conditional.clone();
        create_new.user_msg_id = 77_108;
        create_new.current_msg_id = 77_208;
        let path = inflight_state_path(&root, &ProviderKind::Codex, channel_id);
        let guard = lock_inflight_state_path(&path).expect("hold canonical sidecar");
        let (conditional_tx, conditional_rx) = std::sync::mpsc::channel();

        let conditional_root = root.clone();
        let conditional_thread = std::thread::spawn(move || {
            let result = save_inflight_state_if_absent_in_root(&conditional_root, &conditional);
            conditional_tx
                .send(result)
                .expect("report conditional result");
        });
        let (create_new_tx, create_new_rx) = std::sync::mpsc::channel();
        let create_new_root = root.clone();
        let create_new_thread = std::thread::spawn(move || {
            let result = save_inflight_state_create_new_in_root(&create_new_root, &create_new);
            create_new_tx
                .send(result)
                .expect("report create-new result");
        });

        assert!(matches!(
            second_handle_try_lock(&path),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        assert!(
            conditional_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );
        assert!(
            create_new_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );
        drop(guard);

        let conditional_wrote = conditional_rx
            .recv()
            .expect("conditional result")
            .expect("conditional save");
        let create_new_wrote = match create_new_rx.recv().expect("create-new result") {
            Ok(()) => true,
            Err(CreateNewInflightError::AlreadyExists) => false,
            Err(CreateNewInflightError::Internal(error)) => {
                panic!("create-new save failed: {error}")
            }
        };
        conditional_thread.join().expect("conditional thread");
        create_new_thread.join().expect("create-new thread");
        assert_ne!(conditional_wrote, create_new_wrote);

        let persisted: InflightTurnState =
            serde_json::from_str(&std::fs::read_to_string(path).expect("read persisted inflight"))
                .expect("parse persisted inflight");
        assert_eq!(
            persisted.user_msg_id,
            if conditional_wrote { 0 } else { 77_108 }
        );
        assert_eq!(
            persisted.current_msg_id,
            if conditional_wrote { 77_000 } else { 77_208 }
        );
    }

    #[test]
    fn matches_identity_save_preserves_adopted_output_and_commits_completion() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let provider = ProviderKind::Codex;
        let old_rollout_path = temp.path().join("old-rollout.jsonl");
        let adopted_rollout_path = temp.path().join("adopted-rollout.jsonl");
        std::fs::write(&old_rollout_path, vec![b'o'; 256]).expect("write old rollout");
        std::fs::write(&adopted_rollout_path, vec![b'a'; 1024]).expect("write adopted rollout");
        let channel_id = 44_153_006;
        let mut stale_snapshot = InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("adk-cc".to_string()),
            123,
            456,
            789,
            "continue".to_string(),
            Some("codex-session".to_string()),
            Some("AgentDesk-codex-output-only-adoption-44153006".to_string()),
            Some(old_rollout_path.display().to_string()),
            None,
            256,
        );
        stale_snapshot.turn_start_offset = Some(128);
        stale_snapshot.full_response = "stale response".to_string();
        save_inflight_state_in_root(temp.path(), &stale_snapshot).expect("seed stale snapshot row");
        let expected = InflightTurnIdentity::from_state(&stale_snapshot);
        let expected_turn_start_offset = stale_snapshot.turn_start_offset;

        let mut adopted = stale_snapshot.clone();
        adopted.output_path = Some(adopted_rollout_path.display().to_string());
        adopted.full_response = "adopted durable response".to_string();
        save_inflight_state_in_root(temp.path(), &adopted).expect("persist adopted row");

        stale_snapshot.response_sent_offset = stale_snapshot.full_response.len();
        stale_snapshot.terminal_delivery_committed = true;
        let outcome = save_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &stale_snapshot,
            &expected,
            expected_turn_start_offset,
        );

        assert_eq!(outcome, GuardedSaveOutcome::Saved);
        let persisted_path = inflight_state_path(temp.path(), &provider, channel_id);
        let persisted: InflightTurnState = serde_json::from_str(
            &std::fs::read_to_string(persisted_path).expect("read persisted inflight"),
        )
        .expect("parse persisted inflight");
        assert_eq!(
            persisted.output_path,
            Some(adopted_rollout_path.display().to_string())
        );
        assert_eq!(persisted.full_response, "adopted durable response");
        assert!(persisted.terminal_delivery_committed);
    }
}

fn is_synthetic_create_state(state: &InflightTurnState) -> bool {
    let rebind = state.rebind_origin
        && state.request_owner_user_id == 0
        && state.user_msg_id == 0
        && state.current_msg_id == 0
        && matches!(
            state.turn_source,
            TurnSource::MonitorTriggered | TurnSource::ExternalAdopted
        );
    let watcher = state.turn_source == TurnSource::ExternalInput
        && state.request_owner_user_id == 0
        && state.user_msg_id == 0
        && state.effective_relay_owner_kind() == RelayOwnerKind::Watcher;
    let tui_direct = state.turn_source == TurnSource::ExternalInput
        && state.request_owner_user_id
            == crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID;
    rebind || watcher || tui_direct
}

pub(in crate::services::discord) fn save_inflight_state_create_new(
    state: &InflightTurnState,
) -> Result<(), CreateNewInflightError> {
    let Some(root) = inflight_runtime_root() else {
        return Err(CreateNewInflightError::Internal(
            "Home directory not found".to_string(),
        ));
    };
    save_inflight_state_create_new_in_root(&root, state)
}

/// Test-visible inner form of `save_inflight_state_create_new`. Takes an
/// explicit root so unit tests can exercise create-new semantics without
/// tripping over `AGENTDESK_ROOT_DIR` env-var races.
fn save_inflight_state_create_new_in_root(
    root: &Path,
    state: &InflightTurnState,
) -> Result<(), CreateNewInflightError> {
    save_inflight_state_create_new_in_root_with_write(root, state, create_new_atomic_write)
}

fn save_inflight_state_create_new_in_root_with_write(
    root: &Path,
    state: &InflightTurnState,
    write: impl FnOnce(&Path, &str) -> Result<(), String>,
) -> Result<(), CreateNewInflightError> {
    save_inflight_state_create_new_in_root_with_io(
        root,
        state,
        |path| fs::symlink_metadata(path),
        write,
    )
}

fn final_name_is_occupied_with_metadata(
    path: &Path,
    metadata: impl FnOnce(&Path) -> std::io::Result<fs::Metadata>,
) -> std::io::Result<bool> {
    match metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn save_inflight_state_create_new_in_root_with_io(
    root: &Path,
    state: &InflightTurnState,
    metadata: impl FnOnce(&Path) -> std::io::Result<fs::Metadata>,
    write: impl FnOnce(&Path, &str) -> Result<(), String>,
) -> Result<(), CreateNewInflightError> {
    let Some(provider) = state.provider_kind() else {
        return Err(CreateNewInflightError::Internal(format!(
            "Unknown provider '{}'",
            state.provider
        )));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;
    }
    // This stable sidecar lock is the create-new CAS authority. Every
    // cooperating writer takes it, so the exact final-path existence check and
    // the atomic temp+sync+rename publication below are one serialized action.
    let _lock = lock_inflight_state_path(&path).map_err(CreateNewInflightError::Internal)?;
    match final_name_is_occupied_with_metadata(&path, metadata) {
        Ok(true) => return Err(CreateNewInflightError::AlreadyExists),
        Ok(false) => {}
        Err(error) => return Err(CreateNewInflightError::Internal(error.to_string())),
    }
    let mut updated = state.clone();
    updated.ensure_finalizer_turn_id();
    validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_inflight_state_create_new_in_root",
    );
    updated.updated_at = now_string();
    bump_save_generation_for_write(&path, &mut updated);
    let json = serde_json::to_string_pretty(&updated)
        .map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;

    // The sidecar lock, not an O_EXCL open of the final JSON path, is the
    // compare-and-set authority. Publish through the shared durable writer so
    // a write/sync failure or crash before rename cannot leave an empty or
    // partial final row that permanently converts retries into AlreadyExists.
    write(&path, &json).map_err(CreateNewInflightError::Internal)?;
    if !is_synthetic_create_state(&updated) {
        create_monotonic_observer::observe_successful_real_create(&updated);
    }
    Ok(())
}

fn create_new_atomic_write(path: &Path, data: &str) -> Result<(), String> {
    #[cfg(test)]
    {
        // Preserve the existing observer-order failure witness without replacing
        // the production writer: the one-shot seam aborts this production call
        // before `atomic_write` can publish the final path.
        let lock_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.with_extension("json.lock"))
            .map_err(|error| error.to_string())?;
        create_monotonic_observer::test_seams::sync_result(&lock_file)
            .map_err(|error| error.to_string())?;
    }
    atomic_write(path, data)
}

#[cfg(test)]
pub(super) fn save_inflight_state_in_root(
    root: &Path,
    state: &InflightTurnState,
) -> Result<(), String> {
    let Some(provider) = state.provider_kind() else {
        return Err(format!("Unknown provider '{}'", state.provider));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _lock = lock_inflight_state_path(&path)?;
    let mut updated = state.clone();
    updated.ensure_finalizer_turn_id();
    if !validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_inflight_state_in_root",
    ) {
        return Ok(());
    }
    updated.updated_at = now_string();
    bump_save_generation_for_write(&path, &mut updated);
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)
}

/// #3107 codex re-review (P1): atomic compare-and-set save. Writes `state` ONLY
/// when no inflight row exists for `(provider, channel_id)`, returning `true` iff
/// it wrote. The watcher self-heal re-acquire previously did a non-atomic
/// `load(...).is_some()` preflight + unconditional save: a concurrent intake
/// could create a REAL inflight in the gap, and the synthetic `user_msg_id = 0`
/// save would clobber it (lost turn). This closes the window by doing the check
/// AND write under the same `lock_inflight_state_path` sidecar lock the other
/// save/clear paths serialize on, so the synthetic row is written only when
/// there is genuinely no inflight at the moment of the atomic write.
pub(in crate::services::discord) fn save_inflight_state_if_absent(
    state: &InflightTurnState,
) -> Result<bool, String> {
    let Some(root) = inflight_runtime_root() else {
        return Err("Home directory not found".to_string());
    };
    save_inflight_state_if_absent_in_root(&root, state)
}

fn save_inflight_state_if_absent_in_root(
    root: &Path,
    state: &InflightTurnState,
) -> Result<bool, String> {
    save_inflight_state_if_absent_in_root_with_io(
        root,
        state,
        |path| fs::symlink_metadata(path),
        atomic_write,
    )
}

fn save_inflight_state_if_absent_in_root_with_io(
    root: &Path,
    state: &InflightTurnState,
    metadata: impl FnOnce(&Path) -> std::io::Result<fs::Metadata>,
    write: impl FnOnce(&Path, &str) -> Result<(), String>,
) -> Result<bool, String> {
    let Some(provider) = state.provider_kind() else {
        return Err(format!("Unknown provider '{}'", state.provider));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Hold the sidecar lock across the namespace occupancy check AND the
    // write so a concurrent intake cannot land a real inflight in the gap.
    // `symlink_metadata` treats any final-name entry, including a dangling
    // symlink, as occupied; only NotFound permits publication.
    let _lock = lock_inflight_state_path(&path)?;
    if final_name_is_occupied_with_metadata(&path, metadata).map_err(|error| error.to_string())? {
        return Ok(false);
    }
    let mut updated = state.clone();
    updated.ensure_finalizer_turn_id();
    validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_inflight_state_if_absent_in_root",
    );
    updated.updated_at = now_string();
    bump_save_generation_for_write(&path, &mut updated);
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    write(&path, &json)?;
    if !is_synthetic_create_state(&updated) {
        create_monotonic_observer::observe_successful_real_create(&updated);
    }
    Ok(true)
}
