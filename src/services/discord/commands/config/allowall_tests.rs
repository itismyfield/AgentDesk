use crate::services::discord::commands::cancel_backstop_tests::{
    ActionTransport, invoke_real_slash_action_with_options,
};
use crate::services::discord::runtime_bootstrap::run_bot_build_slash_commands;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude as serenity;

const MANAGED_CONFIG: &[u8] = b"server: {}\ndiscord:\n  bots:\n    main:\n      token: test-token\n      provider: claude\n      auth: {allow_all_users: true}\n";

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

    let root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let yaml_path = crate::runtime_layout::config_file_path(root.path());
    crate::config::test_env::assert_test_config_write_within_root(root.path(), &yaml_path);
    std::fs::create_dir_all(yaml_path.parent().unwrap()).unwrap();
    std::fs::write(&yaml_path, MANAGED_CONFIG).unwrap();
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
    assert_eq!(
        crate::config::resolved_config_path()
            .canonicalize()
            .unwrap(),
        yaml_path.canonicalize().unwrap()
    );
    let saved = crate::config::load_from_path(&yaml_path).unwrap();
    assert_eq!(saved.discord.bots["main"].auth.allow_all_users, Some(false));
}

#[test]
fn allowall_preserves_inherited_external_config_bytes() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel.yaml");
    let before = MANAGED_CONFIG;
    let cwd = tempfile::tempdir().unwrap();
    let cwd_config = crate::runtime_layout::config_file_path(cwd.path());
    std::fs::create_dir_all(cwd_config.parent().unwrap()).unwrap();
    std::fs::write(&cwd_config, before).unwrap();
    std::fs::write(&sentinel, before).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "services::discord::commands::config::allowall_tests::allowall_status_reports_without_changing_public_access",
            "--exact",
        ])
        .current_dir(cwd.path())
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env("AGENTDESK_CONFIG", &sentinel)
        .output()
        .unwrap();
    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        before,
        "inherited CONFIG sentinel changed"
    );
    assert_eq!(
        std::fs::read(&cwd_config).unwrap(),
        before,
        "CWD sentinel changed"
    );
    assert!(
        output.status.success(),
        "allowall child failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("test result: ok. 1 passed; 0 failed; 0 ignored;")
    );
}
