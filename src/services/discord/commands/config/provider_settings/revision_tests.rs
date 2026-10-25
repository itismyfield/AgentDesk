#[cfg(unix)]
use super::*;

#[cfg(unix)]
pub(super) async fn seed_worker(pool: &sqlx::PgPool, instance: &str, status: &str) {
    sqlx::query(
        "INSERT INTO worker_nodes (instance_id, status, capabilities, last_heartbeat_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind(instance)
    .bind(status)
    .bind(serde_json::json!({"intake_worker": {"enabled": true, "providers": ["codex"], "features": ["preserve_on_cancel_v1"]}}))
    .execute(pool)
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn truthful_offline_idle_owner_is_stale_and_does_not_override_local_owner_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    let scratch = tempfile::tempdir().unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &scratch.path().join("missing.yaml"),
    );
    let (db, pool) = crate::services::discord::host_defer_gate::tests::postgres().await;
    seed_worker(&pool, "offline-owner", "offline").await;
    sqlx::query("INSERT INTO sessions (session_key, provider, channel_id, instance_id, status) VALUES ('offline-setting-owner', 'codex', '648670', 'offline-owner', 'idle')")
        .execute(&pool).await.unwrap();
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let channel = serenity::ChannelId::new(648670);
    let error = shared
        .check_settings_owner(&ProviderKind::Codex, channel)
        .await
        .unwrap_err();
    let notice = error.to_string();
    assert!(notice.contains("정리되지 않은 소유 기록"), "{notice}");
    assert!(notice.contains("offline-owner"), "{notice}");
    assert!(!shared.overrides.session_reset_pending.contains(&channel));
    let local = crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    sqlx::query("INSERT INTO sessions (session_key, provider, channel_id, instance_id, status) VALUES ('local-setting-owner', 'codex', '648670', $1, 'idle')")
        .bind(local).execute(&pool).await.unwrap();
    shared
        .check_settings_owner(&ProviderKind::Codex, channel)
        .await
        .unwrap();
    pool.close().await;
    db.drop().await;
}

#[cfg(unix)]
#[tokio::test]
async fn truthful_active_intake_owner_without_session_is_refused_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    let scratch = tempfile::tempdir().unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &scratch.path().join("missing.yaml"),
    );
    let (db, pool) = crate::services::discord::host_defer_gate::tests::postgres().await;
    sqlx::query("INSERT INTO intake_session_owners (provider, raw_channel_id, owner_instance_id, generation) VALUES ('codex', '648671', 'intake-only-owner', 0)")
        .execute(&pool).await.unwrap();
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let channel = serenity::ChannelId::new(648671);
    let error = shared
        .check_settings_owner(&ProviderKind::Codex, channel)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("노드 `intake-only-owner` 소유"),
        "{error}"
    );
    let sessions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE channel_id = '648671'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sessions, 0);
    assert!(!shared.overrides.session_reset_pending.contains(&channel));
    pool.close().await;
    db.drop().await;
}

#[cfg(unix)]
#[test]
fn truthful_shared_feature_reset_is_consumed_by_the_first_parent_or_thread_turn() {
    use crate::services::discord::{DiscordSession, make_shared_data_for_tests};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    let scratch = tempfile::tempdir().unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &scratch.path().join("missing.yaml"),
    );
    let tmux = crate::services::discord::host_defer_gate::tests::ScriptedTmux::install();
    let http = Arc::new(
        serenity::http::HttpBuilder::new("")
            .proxy("http://127.0.0.1:9")
            .build(),
    );
    super::super::session_override_cluster_characterization_tests::test_rt().block_on(async {
        for (fast, goals) in [(true, false), (false, true)] {
            for thread_first in [false, true] {
                let shared = make_shared_data_for_tests();
                let parent = serenity::ChannelId::new(648672);
                let thread = serenity::ChannelId::new(648673);
                for channel in [parent, thread] {
                    shared.core.lock().await.sessions.insert(
                        channel,
                        DiscordSession {
                            session_id: Some(channel.to_string()),
                            memento_context_loaded: true,
                            memento_reflected: false,
                            current_path: None,
                            history: Vec::new(),
                            pending_uploads: Vec::new(),
                            cleared: false,
                            remote_profile_name: None,
                            channel_id: Some(channel.get()),
                            channel_name: None,
                            category_name: None,
                            last_active: tokio::time::Instant::now(),
                            worktree: None,
                            born_generation: shared.restart.current_generation,
                        },
                    );
                }
                let key = shared
                    .feature_settings_channel_id(&ProviderKind::Codex, thread, Some(parent))
                    .await;
                assert_eq!(key, parent);
                if fast {
                    shared
                        .overrides
                        .fast_mode_session_reset_pending
                        .insert(fast_mode_reset_pending_key(key, &ProviderKind::Codex));
                }
                if goals {
                    shared
                        .overrides
                        .codex_goals_session_reset_pending
                        .insert(key);
                }
                sync_session_reset_pending(&shared, key);
                let (first, next) = if thread_first {
                    (thread, parent)
                } else {
                    (parent, thread)
                };
                let result =
                    crate::services::discord::commands::control::reset_provider_session_if_pending(
                        &http,
                        &shared,
                        &ProviderKind::Codex,
                        first,
                        key,
                    )
                    .await;
                assert_eq!(
                    result,
                    Some(crate::services::discord::admin_host_guard::ManagedReset::Applied(None))
                );
                {
                    let state = shared.core.lock().await;
                    assert_eq!(state.sessions[&first].session_id, None);
                    assert_eq!(
                        state.sessions[&next].session_id.as_deref(),
                        Some(next.to_string().as_str())
                    );
                }
                assert!(!fast_mode_reset_pending_for_provider(
                    &shared,
                    key,
                    &ProviderKind::Codex
                ));
                assert!(
                    !shared
                        .overrides
                        .codex_goals_session_reset_pending
                        .contains(&key)
                );
                assert!(
                    crate::services::discord::commands::control::reset_provider_session_if_pending(
                        &http,
                        &shared,
                        &ProviderKind::Codex,
                        next,
                        key,
                    )
                    .await
                    .is_none()
                );
                assert_eq!(
                    shared.core.lock().await.sessions[&next]
                        .session_id
                        .as_deref(),
                    Some(next.to_string().as_str())
                );
            }
        }
    });
    assert!(tmux.take_calls().is_empty());
}

#[test]
fn truthful_shared_feature_scope_warns_which_session_loses_context() {
    let notice = super::super::super::fast_mode::feature_scope_notice(true);
    for phrase in [
        "부모 채널",
        "스레드",
        "먼저 턴을 시작하는 세션",
        "맥락 소실",
    ] {
        assert!(notice.contains(phrase), "{notice}");
    }
    assert_eq!(
        super::super::super::fast_mode::feature_scope_notice(false),
        ""
    );
}
