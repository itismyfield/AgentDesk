use crate::services::discord::commands::cancel_backstop_tests::{
    ActionTransport, invoke_real_slash_action_with_options,
};
use crate::services::discord::runtime_bootstrap::run_bot_build_slash_commands;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude as serenity;

/// The registered `/allowall` takes a required on/off/status choice, and an
/// owner's `status` answers without changing public access while `off` does.
#[test]
fn allowall_status_reports_without_changing_public_access() {
    let allowall = || {
        run_bot_build_slash_commands()
            .into_iter()
            .find(|command| command.name == "allowall")
            .expect("registered /allowall")
    };
    let command = allowall();
    let [action] = command.parameters.as_slice() else {
        panic!("/allowall takes one parameter");
    };
    assert!(action.required);
    let choices: Vec<&str> = action.choices.iter().map(|c| c.name.as_ref()).collect();
    assert_eq!(choices, ["on", "off", "status"]);

    let root = tempfile::tempdir().expect("command runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("allowall action runtime");
    runtime.block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let owner = serenity::UserId::new(42);
        shared.settings.write().await.owner_user_id = Some(owner.get());
        let data = crate::services::discord::Data {
            shared,
            token: "test-token".into(),
            provider: ProviderKind::Claude,
        };
        let channel = serenity::ChannelId::new(discord_mock::CHANNEL_ID);
        let transport = ActionTransport::new(discord_mock::DiscordMockState::new()).await;
        // Choice indexes follow the registered order: on=0, off=1, status=2.
        for (before, choice, after) in [(true, 2, true), (false, 2, false), (true, 1, false)] {
            data.shared.settings.write().await.allow_all_users = before;
            let options = serde_json::json!([{"name": "action", "type": 4, "value": choice}]);
            let answered = invoke_real_slash_action_with_options(
                &transport.ctx,
                &data,
                channel,
                owner,
                allowall(),
                "allowall",
                options,
            )
            .await;
            assert!(answered, "choice {choice} from {before}");
            let now = data.shared.settings.read().await.allow_all_users;
            assert_eq!(now, after, "choice {choice} from {before}");
        }
    });
}
