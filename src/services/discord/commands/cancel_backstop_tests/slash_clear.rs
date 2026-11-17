//! The real poise slash action keeps authorization and interaction responses in the clear path.

use super::{ActionTransport, invoke_real_slash_action, missing_tmux};
use crate::db::dispatched_sessions::provider_resume_fence;
use crate::services::discord::host_defer_gate::tests::{map_channel, postgres};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{Data, SharedData};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::{ChannelId, UserId};
use std::sync::Arc;

const TEST_OWNER: u64 = 42;
const OLD_SELECTOR: &str = "old-slash-clear-selector";

async fn invoke_as(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    author: UserId,
) -> Vec<String> {
    let transport = ActionTransport::new(discord_mock::DiscordMockState::new()).await;
    let data = Data {
        shared,
        token: "slash-clear-test-token".into(),
        provider,
    };
    assert!(
        invoke_real_slash_action(
            &transport.ctx,
            &data,
            channel,
            author,
            super::super::control::cmd_clear(),
            "clear",
        )
        .await,
        "the production slash clear action must acknowledge the interaction"
    );
    {
        let replies = transport.replies.lock().expect("slash clear responses");
        assert_eq!(replies.len(), 2, "one defer and one final clear response");
        assert_eq!(replies[0]["type"], 5, "clear defers before doing its work");
        assert_eq!(replies[1]["type"], "followup");
    }
    let replies = transport.followup_contents();
    assert_eq!(replies.len(), 1);
    replies
}

/// Invokes the production slash action on the caller's runtime and canonical channel identity.
pub(in crate::services::discord) async fn slash_clear_for_test(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
) -> Vec<String> {
    let author = {
        let mut settings = shared.settings.write().await;
        UserId::new(*settings.owner_user_id.get_or_insert(TEST_OWNER))
    };
    let replies = invoke_as(shared, provider, channel, author).await;
    assert!(
        replies[0].contains("초기화") && !replies[0].contains("권한이 없어"),
        "the authorized action must report the conversation clear: {replies:?}"
    );
    replies
}

async fn seed_legacy(shared: &Arc<SharedData>, pool: &sqlx::PgPool, channel: ChannelId) -> String {
    map_channel(shared, channel, "slash-clear-fixture").await;
    let key = crate::services::discord::adk_session::build_adk_session_key(
        shared,
        channel,
        &ProviderKind::Claude,
        None,
    )
    .await
    .expect("mapped slash clear session key");
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, identity_kind,
             discord_token_hash, channel_id, claude_session_id, raw_provider_session_id)
         VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4, $4)",
    )
    .bind(&key)
    .bind(&shared.token_hash)
    .bind(channel.get().to_string())
    .bind(OLD_SELECTOR)
    .execute(pool)
    .await
    .expect("seed slash clear selector");
    key
}

fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("slash clear runtime")
}

#[test]
fn actual_slash_clear_commits_herdr_resume_fence_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = tempfile::tempdir().expect("mock tmux directory");
    let _tmux = missing_tmux(&tmux);
    let channel = ChannelId::new(discord_mock::CHANNEL_ID);
    let _hosts = crate::config::session_hosts::force_for_test(
        Some("slash-clear-node"),
        &[(channel.get(), "slash-clear-node")],
    );
    current_thread().block_on(Box::pin(async {
        let (db, pool) = postgres().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let key = seed_legacy(&shared, &pool, channel).await;
        assert!(
            provider_resume_fence::load_by_resolved_key_pg(&pool, &key)
                .await
                .unwrap()
                .is_none()
        );
        slash_clear_for_test(shared.clone(), ProviderKind::Claude, channel).await;
        assert!(
            provider_resume_fence::load_by_resolved_key_pg(&pool, &key)
                .await
                .unwrap()
                .is_some(),
            "the actual slash action must commit the next-launch fence"
        );
        let selected: Option<String> =
            sqlx::query_scalar("SELECT claude_session_id FROM sessions WHERE session_key = $1")
                .bind(&key)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(selected, None, "clear also forgets the legacy DB selector");
        assert!(shared.core.lock().await.sessions[&channel].cleared);
        drop(shared);
        pool.close().await;
        db.drop().await;
    }));
}

#[test]
fn unauthorized_slash_clear_keeps_selector_and_fence_unchanged_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(discord_mock::CHANNEL_ID);
    let _hosts = crate::config::session_hosts::force_for_test(
        Some("slash-clear-node"),
        &[(channel.get(), "slash-clear-node")],
    );
    current_thread().block_on(Box::pin(async {
        let (db, pool) = postgres().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        shared.settings.write().await.owner_user_id = Some(TEST_OWNER);
        let key = seed_legacy(&shared, &pool, channel).await;
        let replies = invoke_as(
            shared.clone(),
            ProviderKind::Claude,
            channel,
            UserId::new(TEST_OWNER + 1),
        )
        .await;
        assert_eq!(replies, ["권한이 없어 세션을 초기화하지 않았어요."]);
        assert!(
            provider_resume_fence::load_by_resolved_key_pg(&pool, &key)
                .await
                .unwrap()
                .is_none()
        );
        let selected: Option<String> =
            sqlx::query_scalar("SELECT claude_session_id FROM sessions WHERE session_key = $1")
                .bind(&key)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(selected.as_deref(), Some(OLD_SELECTOR));
        assert!(!shared.core.lock().await.sessions[&channel].cleared);
        drop(shared);
        pool.close().await;
        db.drop().await;
    }));
}

#[test]
fn actual_slash_resume_refuses_herdr_and_keeps_clear_fence_pg() {
    use crate::db::dispatched_session_canonical_identity::{
        CanonicalSessionIdentity, SessionIdentityKind,
    };

    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(discord_mock::CHANNEL_ID);
    let _hosts = crate::config::session_hosts::force_for_test(
        Some("slash-clear-node"),
        &[(channel.get(), "slash-clear-node")],
    );
    current_thread().block_on(Box::pin(async {
        let (db, pool) = postgres().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        shared.settings.write().await.owner_user_id = Some(TEST_OWNER);
        let key = seed_legacy(&shared, &pool, channel).await;
        let channel_key = channel.get().to_string();
        let fence = provider_resume_fence::set_for_channel_pg(
            &pool,
            "claude",
            CanonicalSessionIdentity {
                kind: SessionIdentityKind::DiscordChannel,
                discord_token_hash: &shared.token_hash,
                channel_id: &channel_key,
            },
            &key,
        )
        .await
        .unwrap();
        let transport = ActionTransport::new(discord_mock::DiscordMockState::new()).await;
        let data = Data {
            shared: shared.clone(),
            token: "slash-resume-test-token".into(),
            provider: ProviderKind::Claude,
        };
        assert!(
            invoke_real_slash_action(
                &transport.ctx,
                &data,
                channel,
                UserId::new(TEST_OWNER),
                super::super::session::cmd_resume(),
                "resume",
            )
            .await
        );
        let replies = transport.followup_contents();
        assert_eq!(replies.len(), 1);
        assert!(
            replies[0].contains("/resume 실패") && replies[0].contains("Herdr 설정 채널"),
            "the actual resume dispatch must report HostUnsupported: {replies:?}"
        );
        assert_eq!(
            provider_resume_fence::load_by_resolved_key_pg(&pool, &key)
                .await
                .unwrap(),
            Some(fence),
            "an unsupported resume cannot authorize a provider session or release the fence"
        );
        let selected: Option<String> =
            sqlx::query_scalar("SELECT claude_session_id FROM sessions WHERE session_key = $1")
                .bind(&key)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(selected.as_deref(), Some(OLD_SELECTOR));
        drop(data);
        drop(transport);
        drop(shared);
        pool.close().await;
        db.drop().await;
    }));
}
