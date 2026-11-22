//! A thread's settings owner is its own execution's node, never its parent's policy alone.

use super::*;
use crate::db::dispatched_sessions::hosted_execution::{HostedState, tests::record};
use crate::services::platform::tmux::SessionPresence;
use crate::services::session_host::host_decision::force_presence_for_test;
use crate::services::session_host::surface_policy::{Relation, force_relation_for_test};

const TMUX: &str = "AgentDesk-claude-s1b-owner";

// Under a remote Herdr parent: a live local tmux thread passes, a Bound thread on the remote
// endpoint is that node's, and a thread whose execution is not read is refused with the reason.
#[tokio::test(flavor = "current_thread")]
async fn a_threads_settings_owner_follows_its_own_execution_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let (parent, thread) = (8_118_470, 8_118_471);
    let _hosts =
        crate::config::session_hosts::force_for_test(Some("test-node"), &[(parent, "other")]);
    let _thread = force_relation_for_test(thread, Relation::Child { parent });
    let key = crate::services::discord::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        &ProviderKind::Claude,
        TMUX,
    );
    let seed = |raw: Option<serde_json::Value>| {
        let (pool, key, token) = (pool.clone(), key.clone(), shared.token_hash.clone());
        async move {
            sqlx::query(
                "INSERT INTO sessions (session_key, provider, status, identity_kind,
                                       discord_token_hash, channel_id, hosted_execution)
                 VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)
                 ON CONFLICT (session_key) DO UPDATE SET hosted_execution = EXCLUDED.hosted_execution",
            )
            .bind(&key)
            .bind(&token)
            .bind(thread.to_string())
            .bind(raw)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    let check =
        || shared.check_settings_owner(&ProviderKind::Claude, serenity::ChannelId::new(thread));
    seed(None).await;
    force_presence_for_test(TMUX, SessionPresence::Present);
    check()
        .await
        .expect("a live local tmux thread is not the parent's node's");

    let mut owner =
        crate::db::dispatched_sessions::hosted_execution::tests::owner(&thread.to_string());
    owner.discord_token_hash = shared.token_hash.clone();
    owner.logical_key = TMUX.into();
    owner.runtime_root = crate::config::runtime_root().unwrap().display().to_string();
    let mut bound = record(&owner, "nonce-owner", HostedState::Bound);
    let at = bound.location.as_mut().unwrap();
    (at.execution_node, at.endpoint_config_key) = ("other".into(), "other-endpoint".into());
    (at.socket_addr, at.named_session) = ("/adk/herdr/agentdesk.sock".into(), "agentdesk".into());
    seed(Some(serde_json::to_value(&bound).unwrap())).await;
    let remote = check().await.unwrap_err().to_string();
    assert!(remote.contains("노드 `other` 소유 채널"), "{remote}");

    seed(None).await;
    force_presence_for_test(TMUX, SessionPresence::ProbeFailed);
    let held = check().await.unwrap_err().to_string();
    assert!(
        held.starts_with("실행 호스트를 확인하지 못해 설정 변경을 거절합니다("),
        "{held}"
    );
    pool.close().await;
    db.drop().await;
}

// A Herdr pane's fast setting is kept in a thread's settings channel, else in the channel itself.
#[tokio::test(flavor = "current_thread")]
async fn a_herdr_fast_setting_is_kept_in_a_threads_settings_channel() {
    let (parent, thread, plain) = (8_118_480, 8_118_481, 8_118_482);
    let _thread = force_relation_for_test(thread, Relation::Child { parent });
    use super::super::super::fast_mode::herdr_store_channel as store;
    let id = serenity::ChannelId::new;
    assert_eq!(store(id(thread), async { id(parent) }).await, id(parent));
    assert_eq!(store(id(plain), async { id(parent) }).await, id(plain));
}
