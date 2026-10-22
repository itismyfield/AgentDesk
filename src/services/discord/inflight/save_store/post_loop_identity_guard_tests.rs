use crate::services::discord::inflight::{
    GuardedSaveOutcome, InflightTurnState, load_inflight_state, save_inflight_state,
    save_inflight_state_if_identity_unchanged,
};
use crate::services::discord::router::source_guard_support::{Lexed, forge};
use crate::services::provider::ProviderKind;
use crate::test_support::CommentForgery;

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
    state.updated_at = "2026-07-30 12:00:00".to_string();
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
    let original = load_inflight_state(&ProviderKind::Codex, original.channel_id)
        .expect("persisted current turn row");

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

    assert!(
        save_inflight_state_if_identity_unchanged(
            &same_turn_finalize,
            "turn_bridge::post_loop_finalize::mutation_test_mismatched_identity",
        )
        .is_identity_mismatch_legacy(),
        "a stale post-loop finalize must decline after a different turn re-owns the row"
    );
    let persisted =
        load_inflight_state(&ProviderKind::Codex, original.channel_id).expect("newer turn row");
    assert_eq!(persisted.user_msg_id, newer_turn.user_msg_id);
    assert!(persisted.long_running_placeholder_active);

    assert_eq!(
        post_loop_save_gaps(POST_LOOP_FINALIZE),
        Vec::<String>::new()
    );
}

const POST_LOOP_FINALIZE: &str = include_str!("../../turn_bridge/post_loop_finalize.rs");
const GUARDED_SAVE: &str = "save_inflight_state_if_identity_unchanged(";
const BLIND_SAVE: &str = "let _ = save_inflight_state(&inflight_state);";

/// Classification 2: four identity-guarded saves counted in code, no blind whole-row save
/// in comment-free text. A call-count tripwire; it proves neither reachability nor pairing.
fn post_loop_save_gaps(source: &str) -> Vec<String> {
    let lexed = Lexed::new(source);
    let mut gaps = Vec::new();
    if lexed.count(GUARDED_SAVE) != 4 {
        gaps.push("all four post-loop terminal saves must remain identity guarded".to_string());
    }
    if lexed.text().contains(BLIND_SAVE) {
        gaps.push("post-loop finalize must not regress to a blind whole-row save".to_string());
    }
    gaps
}

#[test]
fn post_loop_save_guard_rejects_commented_saves_and_counts_coded_blind_saves() {
    let first =
        "let _ = crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(";
    // One of the four saves commented out still leaves four raw matches.
    for (open, close) in [("// ", "\n"), ("/* outer /* ", " */ */")] {
        let one = POST_LOOP_FINALIZE.replacen(first, &format!("{open}{first}{close}"), 1);
        assert_eq!(one.matches(GUARDED_SAVE).count(), 4);
        assert_eq!(post_loop_save_gaps(&one).len(), 1, "{open}");
    }
    for forgery in CommentForgery::ALL {
        let forged = forge(POST_LOOP_FINALIZE, GUARDED_SAVE, forgery);
        assert_eq!(post_loop_save_gaps(&forged).len(), 1, "{forgery:?}");
    }
    let prose = POST_LOOP_FINALIZE.replacen(first, &format!("// {BLIND_SAVE}\n{first}"), 1);
    assert_ne!(prose, POST_LOOP_FINALIZE);
    assert_eq!(post_loop_save_gaps(&prose), Vec::<String>::new());
    let coded = POST_LOOP_FINALIZE.replacen(first, &format!("{BLIND_SAVE}\n{first}"), 1);
    assert_eq!(post_loop_save_gaps(&coded).len(), 1);
}
