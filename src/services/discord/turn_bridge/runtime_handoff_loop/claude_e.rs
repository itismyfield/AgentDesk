use super::super::*;

fn stamp_process_identity_fields(inflight_state: &mut InflightTurnState, pid: u32) {
    if pid == 0 {
        return;
    }
    let process_identity = crate::services::process::ProcessIdentity::capture(pid);
    inflight_state.claude_e_pid = Some(pid);
    inflight_state.claude_e_process_starttime = process_identity.persisted_starttime();
    inflight_state.claude_e_macos_lstart_hash = process_identity.persisted_macos_lstart_hash();
}

pub(super) fn stamp_process_evidence(
    inflight_state: &mut InflightTurnState,
    output_path: String,
    last_offset: u64,
    pid: u32,
    state_dirty: bool,
) -> (bool, crate::services::discord::inflight::GuardedSaveOutcome) {
    let persisted_baseline = inflight_state.clone();
    let expected_identity =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(&persisted_baseline);
    inflight_state.runtime_kind =
        Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeEAdapter);
    inflight_state.tmux_session_name = None;
    inflight_state.output_path = Some(output_path);
    inflight_state.input_fifo_path = None;
    inflight_state.last_offset = last_offset;
    stamp_process_identity_fields(inflight_state, pid);
    let outcome = crate::services::discord::inflight::stamp_claude_e_process_if_matches_identity(
        (&persisted_baseline, &mut *inflight_state),
        &expected_identity,
    );
    if outcome == crate::services::discord::inflight::GuardedSaveOutcome::IoError {
        inflight_state.clone_from(&persisted_baseline);
    }
    (
        super::guarded_save::tmux_ready_state_dirty_after_guarded_save(state_dirty, Some(outcome)),
        outcome,
    )
}

#[cfg(test)]
mod tests {
    use super::{stamp_process_evidence, stamp_process_identity_fields};
    use crate::services::discord::inflight::{
        GuardedSaveOutcome, InflightTurnState, load_inflight_state, save_inflight_state,
    };
    use crate::services::provider::ProviderKind;

    fn state_with_process_evidence() -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            45_960,
            None,
            7,
            8,
            9,
            "prompt".to_string(),
            Some("session".to_string()),
            None,
            Some("/tmp/claude-e-output".to_string()),
            None,
            11,
        );
        state.claude_e_pid = Some(42);
        state.claude_e_process_starttime = Some(43);
        state.claude_e_macos_lstart_hash = Some(44);
        state
    }

    #[test]
    fn recovery_pid_sentinel_preserves_existing_process_evidence() {
        let mut state = state_with_process_evidence();

        stamp_process_identity_fields(&mut state, 0);

        assert_eq!(state.claude_e_pid, Some(42));
        assert_eq!(state.claude_e_process_starttime, Some(43));
        assert_eq!(state.claude_e_macos_lstart_hash, Some(44));
    }

    #[test]
    fn process_evidence_callsite_uses_pre_mutation_baseline_and_adopts_exact_row() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("runtime root");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let mut stale_p1 = state_with_process_evidence();
        stale_p1.channel_id = 42_592_563;
        save_inflight_state(&stale_p1).expect("seed pre-handoff row");
        let mut p2 = stale_p1.clone();

        let (p2_dirty, p2_outcome) = stamp_process_evidence(
            &mut p2,
            "/tmp/claude-e-p2-output".to_string(),
            2_048,
            0,
            false,
        );
        assert_eq!(p2_outcome, GuardedSaveOutcome::Saved);
        assert!(p2_dirty);
        let persisted_p2 =
            load_inflight_state(&ProviderKind::Claude, p2.channel_id).expect("load persisted P2");
        assert_eq!(
            serde_json::to_value(&p2).expect("serialize adopted P2"),
            serde_json::to_value(&persisted_p2).expect("serialize persisted P2"),
        );

        let (p1_dirty, p1_outcome) = stamp_process_evidence(
            &mut stale_p1,
            "/tmp/claude-e-p1-output".to_string(),
            1_024,
            0,
            false,
        );
        assert_eq!(p1_outcome, GuardedSaveOutcome::IdentityMismatch);
        assert!(!p1_dirty);
        let preserved_p2 =
            load_inflight_state(&ProviderKind::Claude, p2.channel_id).expect("load preserved P2");
        assert_eq!(
            serde_json::to_value(&preserved_p2).expect("serialize preserved P2"),
            serde_json::to_value(&persisted_p2).expect("serialize expected P2"),
        );
    }

    #[test]
    fn process_evidence_body_divergence_is_non_retryable_and_preserves_durable() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("runtime root");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let mut seed = state_with_process_evidence();
        seed.channel_id = 42_592_564;
        seed.full_response = "local response".to_string();
        save_inflight_state(&seed).expect("seed pre-handoff row");
        let baseline =
            load_inflight_state(&ProviderKind::Claude, seed.channel_id).expect("load baseline");

        let mut concurrent = baseline.clone();
        concurrent.full_response = "durable response".to_string();
        save_inflight_state(&concurrent).expect("persist divergent durable response");
        let durable = load_inflight_state(&ProviderKind::Claude, seed.channel_id)
            .expect("load divergent durable row");

        for preexisting_dirty in [false, true] {
            let mut local = baseline.clone();
            let (dirty, outcome) = stamp_process_evidence(
                &mut local,
                "/tmp/claude-e-failed-output".to_string(),
                8_192,
                0,
                preexisting_dirty,
            );

            assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
            assert_eq!(dirty, preexisting_dirty);
            assert_eq!(
                local.runtime_kind,
                Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeEAdapter)
            );
            assert_eq!(
                local.output_path.as_deref(),
                Some("/tmp/claude-e-failed-output")
            );
            assert_eq!(local.last_offset, 8_192);
            assert_eq!(local.full_response, baseline.full_response);
            let still_durable = load_inflight_state(&ProviderKind::Claude, seed.channel_id)
                .expect("load durable row after failed stamp");
            assert_eq!(
                serde_json::to_value(&still_durable).expect("serialize durable row"),
                serde_json::to_value(&durable).expect("serialize expected durable row"),
            );
        }
    }
}
