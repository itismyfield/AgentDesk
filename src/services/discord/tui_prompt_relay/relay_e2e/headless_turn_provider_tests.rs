use super::*;
use crate::services::discord::{mailbox_try_start_turn, make_shared_data_for_tests};
use crate::services::provider::CancelToken;
use poise::serenity_prelude::{MessageId, UserId};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_turn_provider_mismatch_refuses_before_mailbox_or_session_mutation() {
    let harness = RelayE2eHarness::start().await;
    let cases = [
        (ProviderKind::Claude, ProviderKind::Codex),
        (ProviderKind::Codex, ProviderKind::Claude),
        (ProviderKind::Claude, ProviderKind::Claude),
        (ProviderKind::Codex, ProviderKind::Codex),
    ];
    for (runtime, execution) in cases {
        let mut shared = make_shared_data_for_tests();
        Arc::get_mut(&mut shared).unwrap().provider = runtime.clone();
        shared.settings.write().await.provider = runtime.clone();
        let channel = harness.channel_id;
        let path = crate::runtime_layout::role_map_path(harness.root.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::json!({"byChannelId": {
            channel.to_string(): {"roleId": "provider-test", "promptFile": "prompt.md", "provider": execution.as_str()}
        }}).to_string()).unwrap();
        let incumbent = Arc::new(CancelToken::new());
        assert!(
            mailbox_try_start_turn(
                &shared,
                channel,
                incumbent.clone(),
                UserId::new(1),
                MessageId::new(900)
            )
            .await
        );
        let outcome = router::start_reserved_headless_turn_with_owner(
            &harness.ctx,
            channel,
            "status",
            "test-owner",
            UserId::new(1),
            &shared,
            "test-token",
            None,
            None,
            Some("provider-test".into()),
            None,
            None,
            router::reserve_headless_turn(),
        )
        .await;
        if runtime != execution {
            assert_eq!(
                outcome,
                Err(router::HeadlessTurnStartError::Internal(format!(
                    "headless provider mismatch: mailbox={} execution={}",
                    runtime.as_str(),
                    execution.as_str()
                )))
            );
        } else {
            assert!(
                matches!(outcome, Err(router::HeadlessTurnStartError::Conflict(ref reason)) if reason.contains("agent mailbox is busy"))
            );
        }
        let snapshot = mailbox_snapshot(&shared, channel).await;
        assert!(Arc::ptr_eq(
            snapshot.cancel_token.as_ref().unwrap(),
            &incumbent
        ));
        assert!(shared.core.lock().await.sessions.is_empty());
        assert_eq!(harness.placeholder_posts(), 0);
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    }
}
