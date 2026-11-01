use crate::services::discord::commands::cancel_backstop_tests::{
    ActionTransport, invoke_real_slash_action,
};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude as serenity;

/// A channel bound with a workspace but no name, alias or live session has no
/// session key; `/resume` says so without blaming a missing workspace.
#[test]
fn resume_without_a_channel_name_does_not_blame_the_workspace() {
    let root = tempfile::tempdir().expect("command runtime root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    let config = crate::runtime_layout::config_file_path(root.path());
    std::fs::create_dir_all(config.parent().unwrap()).expect("config dir");
    let yaml = format!(
        "server: {{}}\nagents:\n  - id: resume-fixture\n    name: Resume Fixture\n    provider: claude\n    channels:\n      claude:\n        id: '{}'\n        workspace: {}\n",
        discord_mock::CHANNEL_ID,
        serde_json::to_string(&workspace).unwrap(),
    );
    std::fs::write(&config, yaml).expect("fixture config");
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let channel = serenity::ChannelId::new(discord_mock::CHANNEL_ID);
    assert!(
        crate::services::discord::settings::resolve_workspace(channel, None).is_some(),
        "the fixture binds a workspace"
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("resume action runtime");
    runtime.block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let owner = serenity::UserId::new(42);
        shared.settings.write().await.owner_user_id = Some(owner.get());
        let data = crate::services::discord::Data {
            shared,
            token: "test-token".into(),
            provider: ProviderKind::Claude,
        };
        let transport = ActionTransport::new(discord_mock::DiscordMockState::new()).await;
        let answered = invoke_real_slash_action(
            &transport.ctx,
            &data,
            channel,
            owner,
            super::cmd_resume(),
            "resume",
        )
        .await;
        assert!(answered);
        let replies = transport.followup_contents();
        let [reply] = replies.as_slice() else {
            panic!("one reply: {replies:?}");
        };
        assert!(reply.contains("session_key"), "{reply}");
        assert!(!reply.contains("workspace"), "{reply}");
    });
}
