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

// Direct production callers of `save_inflight_state` remain intentionally broad
// while #4111 narrows three RMW sites. Starting inventory for a future
// write-API restriction, generated with `rg -n "save_inflight_state\("
// src/services/discord` and excluding test-only surfaces: files/modules named
// `tests.rs` / `*_tests.rs` and any `#[cfg(test)]` or `#[cfg(all(test, unix))]`
// test modules are not production call sites. Current production inventory: 40
// callers.
// - src/services/discord/router/message_handler/headless_turn.rs:1071
// - src/services/discord/router/message_handler/intake_turn.rs:2515
// - src/services/discord/router/message_handler/provider_isolation.rs:460
// - src/services/discord/router/message_handler/watchdog.rs:822
// - src/services/discord/session_runtime/worktree.rs:599
// - src/services/discord/tui_prompt_relay/codex_idle_rollout.rs:140
// - src/services/discord/tui_prompt_relay/synthetic_start.rs:297
// - src/services/discord/tui_prompt_relay/synthetic_start.rs:344
// - src/services/discord/turn_bridge/mod.rs:1094
// - src/services/discord/turn_bridge/mod.rs:1139
// - src/services/discord/turn_bridge/mod.rs:1161
// - src/services/discord/turn_bridge/mod.rs:1650
// - src/services/discord/turn_bridge/mod.rs:1694
// - src/services/discord/turn_bridge/mod.rs:2531
// - src/services/discord/turn_bridge/mod.rs:2987
// - src/services/discord/turn_bridge/mod.rs:3040
// - src/services/discord/turn_bridge/mod.rs:3064
// - src/services/discord/turn_bridge/mod.rs:3239
// - src/services/discord/turn_bridge/mod.rs:3270
// - src/services/discord/turn_bridge/mod.rs:3300
// - src/services/discord/turn_bridge/mod.rs:3704
// - src/services/discord/turn_bridge/mod.rs:3725
// - src/services/discord/turn_bridge/mod.rs:3735
// - src/services/discord/turn_bridge/mod.rs:3774
// - src/services/discord/turn_bridge/mod.rs:3829
// - src/services/discord/turn_bridge/mod.rs:3851
// - src/services/discord/turn_bridge/mod.rs:3868
// - src/services/discord/turn_bridge/mod.rs:3897
// - src/services/discord/turn_bridge/mod.rs:3929
// - src/services/discord/turn_bridge/mod.rs:4500
// - src/services/discord/turn_bridge/mod.rs:4524
// - src/services/discord/turn_bridge/mod.rs:4537
// - src/services/discord/turn_bridge/mod.rs:4549
// - src/services/discord/turn_bridge/mod.rs:5536
// - src/services/discord/turn_bridge/mod.rs:6330
// - src/services/discord/turn_bridge/mod.rs:6374
// - src/services/discord/turn_bridge/retry_state.rs:328
// - src/services/discord/turn_bridge/two_message_panel.rs:205
// - src/services/discord/turn_bridge/watcher_handoff.rs:427
// - src/services/discord/turn_bridge/watcher_handoff.rs:451
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
                88_006,
                11,
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

        let persisted_path = inflight_state_path(temp.path(), &provider, state.channel_id);
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
    fn matches_identity_save_skips_after_adoption_changes_only_output_path() {
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
/// explicit root so unit tests can exercise the O_CREAT|O_EXCL semantics
/// without tripping over `AGENTDESK_ROOT_DIR` env-var races.
fn save_inflight_state_create_new_in_root(
    root: &Path,
    state: &InflightTurnState,
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
    let _lock = lock_inflight_state_path(&path).map_err(CreateNewInflightError::Internal)?;
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

    // `OpenOptions::create_new(true)` is the canonical atomic check-and-
    // create primitive on POSIX (O_CREAT | O_EXCL). No reliance on a
    // preceding `load_inflight_state` — the kernel itself serializes this.
    use std::io::Write;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(json.as_bytes())
                .map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;
            file.sync_all()
                .map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(CreateNewInflightError::AlreadyExists)
        }
        Err(e) => Err(CreateNewInflightError::Internal(e.to_string())),
    }
}

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

pub(in crate::services::discord) fn save_inflight_state_if_identity_unchanged(
    state: &InflightTurnState,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_inflight_state_if_identity_unchanged_in_root(&root, state, caller)
}

pub(super) fn save_inflight_state_if_identity_unchanged_in_root(
    root: &Path,
    state: &InflightTurnState,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(provider) = state.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return GuardedSaveOutcome::IoError;
        }
    }
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        tracing::debug!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            snapshot_identity = ?InflightTurnIdentity::from_state(state),
            "inflight identity-refresh save skipped because durable row is missing"
        );
        return GuardedSaveOutcome::Missing;
    };
    let Ok(on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        tracing::debug!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            snapshot_identity = ?InflightTurnIdentity::from_state(state),
            "inflight identity-refresh save skipped because durable row is malformed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    };
    let expected = InflightTurnIdentity::from_state(state);
    let durable = InflightTurnIdentity::from_state(&on_disk);
    if state.user_msg_id == 0 && state.turn_start_offset.is_none() {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            snapshot_turn_start_offset = ?state.turn_start_offset,
            durable_turn_start_offset = ?on_disk.turn_start_offset,
            "inflight identity-refresh save skipped because offsetless id-0 snapshot cannot safely match a durable row"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.output_path != state.output_path {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            snapshot_output_path = ?state.output_path.as_deref(),
            durable_output_path = ?on_disk.output_path.as_deref(),
            durable_restart_mode = ?on_disk.restart_mode,
            durable_rebind_origin = on_disk.rebind_origin,
            "inflight identity-refresh save skipped because durable row output path changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.restart_mode.is_some() || on_disk.rebind_origin || !expected.matches_state(&on_disk)
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            durable_restart_mode = ?on_disk.restart_mode,
            durable_rebind_origin = on_disk.rebind_origin,
            "inflight identity-refresh save skipped because durable row authority changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }

    let mut updated = state.clone();
    updated.ensure_finalizer_turn_id();
    if !validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_inflight_state_if_identity_unchanged_in_root",
    ) {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            "inflight identity-refresh save skipped because validation rejected the refreshed write"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    updated.updated_at = now_string();
    bump_save_generation_for_write(&path, &mut updated);
    let Ok(json) = serde_json::to_string_pretty(&updated) else {
        return GuardedSaveOutcome::IoError;
    };
    match atomic_write(&path, &json) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                caller = caller,
                snapshot_identity = ?expected,
                error = %error,
                "inflight identity-refresh save failed; leaving durable row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

pub(in crate::services::discord) fn save_inflight_delivery_rewind_if_matches_identity(
    state: &InflightTurnState,
    reason: InflightDeliveryRewindReason,
) -> Result<bool, String> {
    let Some(root) = inflight_runtime_root() else {
        return Err("Home directory not found".to_string());
    };
    save_inflight_delivery_rewind_if_matches_identity_in_root(&root, state, reason)
}

pub(super) fn save_inflight_delivery_rewind_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    reason: InflightDeliveryRewindReason,
) -> Result<bool, String> {
    let Some(provider) = state.provider_kind() else {
        return Err(format!("Unknown provider '{}'", state.provider));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _lock = lock_inflight_state_path(&path)?;
    let Some(on_disk) = load_inflight_state_unlocked(&path) else {
        return Ok(false);
    };
    let expected = InflightTurnIdentity::from_state(state);
    if !expected.matches_state(&on_disk) {
        return Ok(false);
    }
    if on_disk.terminal_delivery_committed {
        return Ok(false);
    }
    let mut updated = on_disk;
    updated.full_response = state.full_response.clone();
    updated.response_sent_offset = state.response_sent_offset;
    updated.terminal_delivery_committed = state.terminal_delivery_committed;
    updated.last_offset = updated.last_offset.max(state.last_offset);
    updated.set_relay_owner_kind(state.effective_relay_owner_kind());
    updated.ensure_finalizer_turn_id();
    if !validate_inflight_state_for_save_with_delivery_rewind_reason(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_inflight_delivery_rewind_if_matches_identity_in_root",
        Some(reason),
    ) {
        return Ok(false);
    }
    updated.updated_at = now_string();
    bump_save_generation_for_write(&path, &mut updated);
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)?;
    Ok(true)
}

/// #3107 codex re-review (P1): atomic compare-and-set save. Writes `state` ONLY
/// when no inflight row exists for `(provider, channel_id)`, returning `true` iff
/// it wrote. The watcher self-heal re-acquire previously did a non-atomic
/// `load(...).is_some()` preflight + unconditional save: a concurrent intake
/// could create a REAL inflight in the gap, and the synthetic `user_msg_id = 0`
/// save would clobber it (lost turn). This closes the window by doing the check
/// AND write under the same `lock_inflight_state_path` flock the other save/clear
/// paths serialize on, so the synthetic row is written only when there is
/// genuinely no inflight at the moment of the atomic write.
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
    let Some(provider) = state.provider_kind() else {
        return Err(format!("Unknown provider '{}'", state.provider));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Hold the sidecar flock across the existence check AND the write so a
    // concurrent intake `save_inflight_state_in_root` (which takes the same
    // lock) cannot land a real inflight in the gap. `path.exists()` under the
    // lock is the compare; `atomic_write` is the set.
    let _lock = lock_inflight_state_path(&path)?;
    if path.exists() {
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
    atomic_write(&path, &json)?;
    Ok(true)
}

/// Outcome of [`save_inflight_state_if_matches_identity`] — the #3041 P1-2 R3
/// identity-guarded re-save used on a delivery-lease `Skip` epilogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum GuardedSaveOutcome {
    /// On-disk row still matched the turn identity; the row was rewritten.
    Saved,
    /// No inflight row existed (the lease HOLDER already cleared it on its
    /// success path). We do NOT resurrect it — the turn is already delivered.
    Missing,
    /// A row existed but its identity did NOT match (a newer turn replaced it,
    /// or a planned-restart / rebind-origin marker now owns the row). We do
    /// NOT clobber it.
    IdentityMismatch,
    /// Filesystem / serialization error during the write.
    IoError,
}

fn identity_matches_with_offset_guard(
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    state: &InflightTurnState,
) -> bool {
    if !expected.matches_state(state) {
        return false;
    }
    // Anchor bind/reuse reads or rewrites another persisted row. For synthetic
    // id-0 rows, fail closed unless BOTH sides carry the birth offset and it
    // matches. This is stricter than the delivery-lease id-0 degenerate fallback,
    // which is transport-level dedup only and never authorizes row mutation.
    if expected.user_msg_id == 0 {
        return matches!(
            (expected_turn_start_offset, state.turn_start_offset),
            (Some(expected_offset), Some(actual_offset)) if expected_offset == actual_offset
        );
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        state.turn_start_offset == Some(expected_offset)
    } else {
        true
    }
}

pub(in crate::services::discord) fn recovery_anchor_msg_id_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> Option<u64> {
    let root = inflight_runtime_root()?;
    let path = inflight_state_path(&root, provider, channel_id);
    let _lock = lock_inflight_state_path(&path).ok()?;
    let data = fs::read_to_string(&path).ok()?;
    let state = serde_json::from_str::<InflightTurnState>(&data).ok()?;
    if !identity_matches_with_offset_guard(expected, expected_turn_start_offset, &state) {
        return None;
    }
    (state.current_msg_id != 0).then_some(state.current_msg_id)
}

pub(in crate::services::discord) fn bind_recovery_anchor_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    expected_current_msg_id: u64,
    anchor_msg_id: u64,
    anchor_text_len: usize,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(&root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let data = match fs::read_to_string(&path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return GuardedSaveOutcome::Missing;
        }
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                error = %error,
                "inflight recovery anchor bind could not read on-disk row; blocking durable anchor write"
            );
            return GuardedSaveOutcome::IoError;
        }
    };
    let Ok(mut on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        return GuardedSaveOutcome::IdentityMismatch;
    };
    if !identity_matches_with_offset_guard(expected, expected_turn_start_offset, &on_disk) {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.current_msg_id != expected_current_msg_id {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    on_disk.current_msg_id = anchor_msg_id;
    on_disk.current_msg_len = anchor_text_len;
    on_disk.ensure_finalizer_turn_id();
    on_disk.updated_at = now_string();
    bump_save_generation_for_write(&path, &mut on_disk);
    let Ok(json) = serde_json::to_string_pretty(&on_disk) else {
        return GuardedSaveOutcome::IoError;
    };
    match atomic_write(&path, &json) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                error = %error,
                "inflight recovery anchor bind failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

pub(in crate::services::discord) fn persist_leak_recovery_response_offset_if_matches_identity_locked(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_current_msg_id: u64,
    delivered_offset: usize,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    persist_leak_recovery_response_offset_if_matches_identity_locked_in_root(
        &root,
        provider,
        channel_id,
        expected,
        expected_current_msg_id,
        delivered_offset,
    )
}

pub(super) fn persist_leak_recovery_response_offset_if_matches_identity_locked_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_current_msg_id: u64,
    delivered_offset: usize,
) -> GuardedSaveOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return GuardedSaveOutcome::IoError;
    }
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Some(mut on_disk) = load_inflight_state_unlocked(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    if !expected.matches_state(&on_disk) || on_disk.current_msg_id != expected_current_msg_id {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.response_sent_offset >= delivered_offset {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if delivered_offset > on_disk.full_response.len()
        || !on_disk.full_response.is_char_boundary(delivered_offset)
    {
        return GuardedSaveOutcome::IdentityMismatch;
    }

    on_disk.response_sent_offset = delivered_offset;
    match persist_under_lock(
        root,
        &path,
        &on_disk,
        "src/services/discord/inflight.rs:persist_leak_recovery_response_offset_if_matches_identity_locked_in_root",
    ) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "leak recovery offset patch failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

pub(in crate::services::discord) fn persist_recovery_output_path_if_matches_identity_locked(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    output_path: String,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    persist_recovery_output_path_if_matches_identity_locked_in_root(
        &root,
        provider,
        channel_id,
        expected,
        output_path,
    )
}

pub(super) fn persist_recovery_output_path_if_matches_identity_locked_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    output_path: String,
) -> GuardedSaveOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return GuardedSaveOutcome::IoError;
    }
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Some(mut on_disk) = load_inflight_state_unlocked(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    if !expected.matches_state(&on_disk) {
        return GuardedSaveOutcome::IdentityMismatch;
    }

    on_disk.output_path = Some(output_path);
    match persist_under_lock(
        root,
        &path,
        &on_disk,
        "src/services/discord/inflight.rs:persist_recovery_output_path_if_matches_identity_locked_in_root",
    ) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "recovery output-path patch failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

/// #3041 P1-2 (codex P1-2 R3): identity-guarded re-save for the bridge's
/// delivery-lease `Skip` epilogue. On a Skip the live HOLDER (watcher) owns the
/// turn and CLEARS the row on success, so the bridge epilogue must NOT blindly
/// `save_inflight_state`: if the holder's clear won the race, a blind re-save
/// would resurrect a STALE row for an already-delivered turn (recovery then sees
/// it delivered, never clears, leaks the row). This closes the window the same
/// way `clear_inflight_state_if_matches` (#2427 D-wire) does: under the lock,
/// write only when the row is STILL present AND its `(user_msg_id, started_at,
/// tmux_session_name)` identity (+ `turn_start_offset` when known) matches. Gone
/// (`Missing`) or replaced by a newer turn / restart-rebind marker
/// (`IdentityMismatch`) → no-op; holder FAILED + didn't clear → still present &
/// matching → refresh (`Saved`). Same flock + atomic_write primitives as the
/// rest of the module (Windows-safe).
pub(in crate::services::discord) fn save_inflight_state_if_matches_identity(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_inflight_state_if_matches_identity_in_root(
        &root,
        state,
        expected,
        expected_turn_start_offset,
    )
}

pub(in crate::services::discord) fn save_existing_inflight_rebind_adoption_if_matches_identity(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
        &root,
        state,
        expected,
        expected_turn_start_offset,
    )
}

pub(in crate::services::discord) fn save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset: u64,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root(
        &root,
        state,
        expected,
        expected_turn_start_offset,
        expected_last_offset,
    )
}

pub(super) fn save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    save_existing_inflight_rebind_adoption_impl_in_root(
        root,
        state,
        expected,
        expected_turn_start_offset,
        None,
    )
}

pub(super) fn save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset: u64,
) -> GuardedSaveOutcome {
    save_existing_inflight_rebind_adoption_impl_in_root(
        root,
        state,
        expected,
        expected_turn_start_offset,
        Some(expected_last_offset),
    )
}

fn save_existing_inflight_rebind_adoption_impl_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset_for_rebase: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(provider) = state.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return GuardedSaveOutcome::IoError;
        }
    }
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    let Ok(on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        return GuardedSaveOutcome::IdentityMismatch;
    };
    if on_disk.rebind_origin {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.restart_mode != state.restart_mode {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if expected.user_msg_id == 0 || !expected.matches_state(&on_disk) {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        if on_disk.turn_start_offset != Some(expected_offset) {
            return GuardedSaveOutcome::IdentityMismatch;
        }
    }
    if expected_last_offset_for_rebase
        .is_some_and(|expected_last| on_disk.last_offset != expected_last)
    {
        return GuardedSaveOutcome::IdentityMismatch;
    }

    let mut updated = on_disk;
    updated.tmux_session_name = state.tmux_session_name.clone();
    updated.output_path = state.output_path.clone();
    updated.input_fifo_path = state.input_fifo_path.clone();
    updated.runtime_kind = state.runtime_kind;
    updated.session_id = state.session_id.clone();
    updated.set_relay_owner_kind(state.effective_relay_owner_kind());
    if expected_last_offset_for_rebase.is_some() {
        updated.last_offset = state.last_offset;
        updated.turn_start_offset = state.turn_start_offset;
        updated.last_watcher_relayed_offset = state.last_watcher_relayed_offset;
        updated.last_watcher_relayed_generation_mtime_ns =
            state.last_watcher_relayed_generation_mtime_ns;
    }
    updated.ensure_finalizer_turn_id();
    let _ = validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_existing_inflight_rebind_adoption_if_matches_identity_in_root",
    );
    updated.updated_at = now_string();
    bump_save_generation_for_write(&path, &mut updated);
    let Ok(json) = serde_json::to_string_pretty(&updated) else {
        return GuardedSaveOutcome::IoError;
    };
    match atomic_write(&path, &json) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "existing inflight rebind adoption save failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

/// Root-explicit inner form of [`save_inflight_state_if_matches_identity`] for
/// unit tests (avoids `AGENTDESK_ROOT_DIR` env-var races).
pub(super) fn save_inflight_state_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(provider) = state.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return GuardedSaveOutcome::IoError;
        }
    }
    // Hold the sidecar flock across the read AND the write so a concurrent
    // holder `clear_inflight_state` (which takes the same lock) cannot land its
    // remove between our identity check and our write.
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    // Holder already cleared the row on its success path → do NOT resurrect.
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    let Ok(on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        // Malformed row: treat like a mismatch and do not clobber — the loader
        // eviction path GCs malformed payloads on the next read.
        return GuardedSaveOutcome::IdentityMismatch;
    };
    // A newer turn (different identity) or a planned-restart / rebind-origin
    // marker now owns the row — never overwrite it with this preserved turn.
    if on_disk.restart_mode.is_some() || on_disk.rebind_origin {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if expected.user_msg_id == 0 || !expected.matches_state(&on_disk) {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        if on_disk.turn_start_offset != Some(expected_offset) {
            return GuardedSaveOutcome::IdentityMismatch;
        }
    }
    if on_disk.output_path != state.output_path {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            snapshot_identity = ?expected,
            durable_identity = ?InflightTurnIdentity::from_state(&on_disk),
            snapshot_output_path = ?state.output_path.as_deref(),
            durable_output_path = ?on_disk.output_path.as_deref(),
            durable_restart_mode = ?on_disk.restart_mode,
            durable_rebind_origin = on_disk.rebind_origin,
            "inflight identity-guarded save skipped because durable row output path changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    // #3089 B3: verdict observe-only here — this path already identity/offset-
    // gates above; the #3416 backward vector is the plain overwrite tails.
    let mut updated = state.clone();
    updated.ensure_finalizer_turn_id();
    let _ = validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_inflight_state_if_matches_identity_in_root",
    );
    updated.updated_at = now_string();
    bump_save_generation_for_write(&path, &mut updated);
    let Ok(json) = serde_json::to_string_pretty(&updated) else {
        return GuardedSaveOutcome::IoError;
    };
    match atomic_write(&path, &json) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight identity-guarded save failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}
