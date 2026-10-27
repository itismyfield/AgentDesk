use super::super::super::super::prompt_builder::{
    DispatchProfile, PromptProfiles, build_system_prompt_with_manifest,
};
use super::super::super::super::settings::RoleBinding;
use poise::serenity_prelude::ChannelId;
use source_guard_tests::{
    HEADLESS_TURN, headless_profile_gaps, lifecycle_take_gaps, post_spawn_gaps, take_once_gaps,
};

pub(super) mod source_guard_tests;

#[tokio::test(start_paused = true)]
async fn deferred_headless_start_leaves_metadata_with_caller_and_no_foreground_queue() {
    use crate::services::discord::{self, inflight::InflightTurnState, live_bridge};
    use crate::services::provider::{CancelToken, ProviderKind};
    use poise::serenity_prelude::{MessageId, UserId};
    use std::sync::Arc;

    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let channel = ChannelId::new(655_212_001);
    let mut shared = discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
    let token = Arc::new(CancelToken::new());
    let mut state = InflightTurnState::new(
        ProviderKind::Codex,
        channel.get(),
        None,
        7,
        11,
        12,
        "headless routine".into(),
        None,
        None,
        None,
        None,
        0,
    );
    state.turn_nonce = token.turn_nonce().map(str::to_owned);
    state.silent_turn = true;
    state.delivery_bot = Some("routine-bot".into());
    state.source = crate::dispatch::Source::Voice;
    assert!(
        discord::mailbox_try_start_turn(
            &shared,
            channel,
            token.clone(),
            UserId::new(7),
            MessageId::new(11),
        )
        .await
    );
    discord::increment_global_active(&shared, "fixture");
    let recovery = live_bridge::try_recovery(&ProviderKind::Codex, channel.get()).unwrap();
    let start = tokio::time::Instant::now();
    assert!(
        matches!(
            super::register_headless_original(&shared, &ProviderKind::Codex, &state, &token).await,
            Err(false)
        ),
        "headless must return retryable start failure instead of consuming a user requeue"
    );
    assert_eq!(start.elapsed(), std::time::Duration::from_secs(5));
    let snapshot = shared.mailbox(channel).snapshot().await;
    assert!(snapshot.cancel_token.is_none());
    assert!(
        snapshot.intervention_queue.is_empty(),
        "no foreground turn can consume headless input"
    );
    assert!(discord::inflight::load_inflight_state(&ProviderKind::Codex, channel.get()).is_none());
    assert!(state.silent_turn);
    assert_eq!(state.delivery_bot.as_deref(), Some("routine-bot"));
    assert_eq!(state.source, crate::dispatch::Source::Voice);
    assert!(!live_bridge::is_live(&ProviderKind::Codex, channel.get()));
    drop(recovery);
}

#[test]
fn reserved_headless_start_has_no_post_spawn_error_path() {
    assert_eq!(post_spawn_gaps(HEADLESS_TURN), Vec::<String>::new());
}

#[test]
fn recovery_context_survives_headless_goal_lifecycle_consumed_return() {
    let root = tempfile::tempdir().expect("create temp runtime root");
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    assert_eq!(lifecycle_take_gaps(HEADLESS_TURN), Vec::<String>::new());
}

#[test]
fn headless_real_turn_consumes_recovery_context_once_before_prompt_use() {
    let root = tempfile::tempdir().expect("create temp runtime root");
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    assert_eq!(take_once_gaps(HEADLESS_TURN), Vec::<String>::new());
}

#[test]
fn actual_headless_assembly_selects_only_all_and_headless_shared_sections() {
    assert_eq!(headless_profile_gaps(HEADLESS_TURN), Vec::<String>::new());

    let runtime_root = tempfile::tempdir().expect("runtime root");
    let _runtime_guard = crate::config::set_agentdesk_root_for_test(runtime_root.path());
    let shared_prompt_path = crate::runtime_layout::shared_prompt_path(runtime_root.path());
    std::fs::create_dir_all(shared_prompt_path.parent().expect("shared prompt parent"))
        .expect("create shared prompt parent");
    std::fs::write(
        shared_prompt_path,
        "<!-- profile: all -->\nHEADLESS ACTUAL ALL 4560\n<!-- /profile -->\n\
         <!-- profile: full -->\nHEADLESS ACTUAL FULL 4560\n<!-- /profile -->\n\
         <!-- profile: headless -->\nHEADLESS ACTUAL HEADLESS 4560\n<!-- /profile -->\n",
    )
    .expect("write shared prompt");
    let binding = RoleBinding {
        role_id: "headless-actual-profile-4560".to_string(),
        prompt_file: runtime_root
            .path()
            .join("missing-role-prompt.md")
            .display()
            .to_string(),
        provider: None,
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: false,
        auth_profile: "default".to_string(),
        memory: Default::default(),
    };
    let built = build_system_prompt_with_manifest(
        "ctx",
        &[],
        "/nonexistent-headless-workspace-4560",
        ChannelId::new(1),
        ChannelId::new(1),
        "tok",
        Some(&binding),
        false,
        PromptProfiles::headless(DispatchProfile::Full),
        None,
        None,
        None,
        None,
        None,
        false,
        false,
        None,
        None,
        Some("turn-headless-actual-profile-4560"),
    );

    assert!(built.system_prompt.contains("HEADLESS ACTUAL ALL 4560"));
    assert!(
        built
            .system_prompt
            .contains("HEADLESS ACTUAL HEADLESS 4560")
    );
    assert!(!built.system_prompt.contains("HEADLESS ACTUAL FULL 4560"));
}
