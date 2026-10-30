//! Resolve policy inheritance without changing the channel used for ownership/delivery.
use super::*;
use crate::services::cluster::{agent_execution_node, execution_capacity};

const POLICY_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub(super) async fn resolve(
    deps: &IntakeDeps<'_>,
    pool: &sqlx::PgPool,
    channel: serenity::ChannelId,
    is_dm: bool,
) -> Result<String, String> {
    let id = channel.get().to_string();
    if is_dm
        || agent_execution_node::channel_is_bound(pool, &id)
            .await
            .map_err(|e| e.to_string())?
    {
        return Ok(id);
    }
    // Preserve legacy unbound-channel behavior when there is no placement policy to inherit.
    if !execution_capacity::automatic_enabled()
        && !agent_execution_node::has_channel_policy(pool)
            .await
            .map_err(|e| e.to_string())?
    {
        return Ok(id);
    }
    let metadata = tokio::time::timeout(POLICY_LOOKUP_TIMEOUT, channel.to_channel(deps.http))
        .await
        .map_err(|_| "Discord channel policy lookup timed out".to_string())?
        .map_err(|e| format!("Discord channel policy lookup failed: {e}"))?;
    let serenity::Channel::Guild(guild) = metadata else {
        return Ok(id);
    };
    if !matches!(
        guild.kind,
        serenity::ChannelType::PublicThread
            | serenity::ChannelType::PrivateThread
            | serenity::ChannelType::NewsThread
    ) {
        return Ok(id);
    }
    let parent = guild
        .parent_id
        .ok_or("Discord thread has no parent channel")?;
    if !crate::services::discord::agentdesk_config::thread_inheritance_enabled(parent, None) {
        return Ok(id);
    }
    Ok(parent.get().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use std::sync::Arc;

    #[test]
    fn boot_setup_does_not_enrich_role_map() {
        let source = include_str!("../../runtime_bootstrap/framework_setup.rs");
        assert!(!source.contains("enrich_role_map_with_channel_ids"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn thread_inherit_yaml_controls_intake_policy_pg() {
        let root = tempfile::tempdir().unwrap();
        let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _live = crate::config_live_reload::test_live_guard(lock);
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let parent = serenity::ChannelId::new(73_490_001);
        let child = serenity::ChannelId::new(73_490_002);
        sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_id, preferred_intake_node_labels) VALUES ('inherit-parent', 'Parent', 'claude', $1, '[\"test\"]'::jsonb)")
            .bind(parent.to_string()).execute(&pool).await.unwrap();
        let app = axum::Router::new().fallback(move || async move {
            axum::Json(serde_json::json!({
                "id": child.to_string(), "type": 11, "guild_id": "73490000",
                "parent_id": parent.to_string(), "name": "inherit-thread"
            }))
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy(format!("http://{address}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        let shared = crate::services::discord::make_shared_data_for_tests();
        let deps = IntakeDeps {
            http: &http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared: &shared,
            token: "test-token",
        };
        let path = root.path().join("thread-policy.yaml");
        let legacy = root.path().join("config/role_map.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        crate::config_live_reload::install(crate::config::Config::default());
        for (index, (configured, legacy_value)) in [
            (Some(false), true),
            (Some(true), false),
            (None, false),
            (None, true),
        ]
        .into_iter()
        .enumerate()
        {
            let mut channel = serde_json::json!({"id": parent.to_string()});
            if let Some(value) = configured {
                channel["thread_inherit"] = value.into();
            }
            let mut config = crate::config::Config::default();
            config.agents = serde_json::from_value(serde_json::json!([{
                "id": "inherit-parent", "name": "Parent", "provider": "claude",
                "channels": {"claude": channel}
            }]))
            .unwrap();
            let mut yaml = serde_yaml::to_value(&config).unwrap();
            if let Some(value) = configured {
                yaml["agents"][0]["channels"]["claude"]["thread_inherit"] =
                    serde_yaml::Value::Bool(value);
            }
            std::fs::write(&path, serde_yaml::to_string(&yaml).unwrap()).unwrap();
            std::fs::write(
                &legacy,
                serde_json::json!({"byChannelId": {
                    parent.to_string(): {"threadInherit": legacy_value}
                }})
                .to_string(),
            )
            .unwrap();
            let crate::config_live_reload::ReloadOutcome::Applied { restart_required } =
                crate::config_live_reload::reload_from_path(&path)
            else {
                panic!("valid channel policy was rejected");
            };
            if index > 0 {
                assert!(restart_required.is_empty(), "thread inheritance is live");
            }
            // Intake must use the published snapshot even if the disk file changes again.
            std::fs::write(&path, "invalid: [").unwrap();
            assert_eq!(
                resolve(&deps, &pool, child, false).await.unwrap(),
                if configured.unwrap_or(true) {
                    parent
                } else {
                    child
                }
                .to_string(),
                "YAML={configured:?}, legacy={legacy_value}"
            );
        }
        crate::config_live_reload::install(crate::config::Config::default());
        assert_eq!(
            resolve(&deps, &pool, child, false).await.unwrap(),
            parent.to_string()
        );
        server.abort();
        pool.close().await;
        db.drop().await;
    }
}
