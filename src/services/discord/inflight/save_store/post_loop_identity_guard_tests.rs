use crate::services::discord::inflight::{
    GuardedSaveOutcome, InflightTurnState, load_inflight_state, save_inflight_state,
    save_inflight_state_if_identity_unchanged,
};
use crate::services::provider::ProviderKind;

fn post_loop_state() -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Codex,
        44_259,
        Some("adk-test".to_string()),
        343_742_347_365_974_026,
        77_010,
        18,
        "user prompt".to_string(),
        Some("session".to_string()),
        Some("AgentDesk-codex-post-loop-4259".to_string()),
        Some("/tmp/AgentDesk-codex-post-loop-4259.jsonl".to_string()),
        None,
        512,
    );
    state.full_response = "partial response".to_string();
    state.long_running_placeholder_active = true;
    state
}

#[test]
fn post_loop_finalize_saves_require_matching_identity() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let temp = tempfile::TempDir::new().expect("runtime root");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let original = post_loop_state();
    save_inflight_state(&original).expect("seed current turn row");

    let mut same_turn_finalize = original.clone();
    same_turn_finalize.long_running_placeholder_active = false;
    assert_eq!(
        save_inflight_state_if_identity_unchanged(
            &same_turn_finalize,
            "turn_bridge::post_loop_finalize::mutation_test_same_identity",
        ),
        GuardedSaveOutcome::Saved,
        "post-loop terminal updates must persist while the same turn still owns the row"
    );
    let persisted = load_inflight_state(&ProviderKind::Codex, original.channel_id)
        .expect("same-turn finalize row");
    assert!(!persisted.long_running_placeholder_active);

    let mut newer_turn = original.clone();
    newer_turn.user_msg_id = original.user_msg_id + 1;
    newer_turn.long_running_placeholder_active = true;
    save_inflight_state(&newer_turn).expect("replace row with newer turn");

    assert_eq!(
        save_inflight_state_if_identity_unchanged(
            &same_turn_finalize,
            "turn_bridge::post_loop_finalize::mutation_test_mismatched_identity",
        ),
        GuardedSaveOutcome::IdentityMismatch,
        "a stale post-loop finalize must decline after a different turn re-owns the row"
    );
    let persisted =
        load_inflight_state(&ProviderKind::Codex, original.channel_id).expect("newer turn row");
    assert_eq!(persisted.user_msg_id, newer_turn.user_msg_id);
    assert!(persisted.long_running_placeholder_active);

    let post_loop_source = include_str!("../../turn_bridge/post_loop_finalize.rs");
    assert_eq!(
        post_loop_source
            .matches("save_inflight_state_if_identity_unchanged(")
            .count(),
        4,
        "all four post-loop terminal saves must remain identity guarded"
    );
    assert!(
        !post_loop_source.contains("let _ = save_inflight_state(&inflight_state);"),
        "post-loop finalize must not regress to a blind whole-row save"
    );
}
