use super::*;
use crate::services::discord::Data;
use crate::services::discord::admin_host_guard::tests::Recorder;
use crate::services::discord::commands::cancel_backstop_tests::invoke_real_slash_action;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;

#[tokio::test]
async fn stop_alias_matches_the_stop_owner_failure_reply() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let api = Recorder::start().await;
    let (_, gateway_url, gateway) =
        discord_mock::start(discord_mock::DiscordMockState::new()).await;
    let ctx =
        discord_mock::serenity_context(format!("http://127.0.0.1:{}", api.port), gateway_url).await;
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.settings.write().await.owner_user_id = Some(1);
    let data = Data {
        shared,
        token: "stop-alias-test".into(),
        provider: ProviderKind::Gemini,
    };
    let mut replies = Vec::new();
    for alias in [false, true] {
        api.take();
        let mut command = if alias {
            cmd_skill()
        } else {
            super::super::control::cmd_stop()
        };
        if alias {
            command.slash_action = Some(|ctx| {
                Box::pin(async move {
                    run_skill_slash_command(ctx.into(), "/skill", "stop".into(), None)
                        .await
                        .expect("the alias action succeeds");
                    Ok(())
                })
            });
        }
        assert!(
            invoke_real_slash_action(
                &ctx,
                &data,
                serenity::ChannelId::new(648_502),
                serenity::UserId::new(1),
                command,
                if alias { "skill" } else { "stop" }
            )
            .await
        );
        let calls = api.take();
        let messages: Vec<String> = calls
            .iter()
            .filter_map(|call| {
                let (_, body) = call.split_once(' ')?.1.split_once(' ')?;
                let value: serde_json::Value = serde_json::from_str(body).ok()?;
                value["content"].as_str().map(str::to_owned)
            })
            .collect();
        assert_eq!(messages.len(), 1, "{calls:?}");
        replies.push(messages);
    }
    assert_eq!(replies[0], replies[1]);
    assert!(replies[0][0].contains("owner"));
    gateway.abort();
}
