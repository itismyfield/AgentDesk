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
    let deadline = tokio::time::Instant::now() + POLICY_LOOKUP_TIMEOUT;
    let metadata = tokio::time::timeout_at(deadline, channel.to_channel(deps.http))
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
    let parent_name = tokio::time::timeout_at(
        deadline,
        crate::services::discord::session_runtime::resolve_channel_name(deps.http, parent),
    )
    .await
    .map_err(|_| "Discord channel policy lookup timed out".to_string())?
    .map_err(|e| format!("Discord channel policy lookup failed: {e}"))?;
    if !crate::services::discord::agentdesk_config::thread_inheritance_enabled(
        parent,
        parent_name.as_deref(),
    ) {
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
        let parent_behavior = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let behavior = Arc::clone(&parent_behavior);
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let behavior = Arc::clone(&behavior);
            async move {
                if uri.path().rsplit('/').next() == Some(parent.to_string().as_str()) {
                    match behavior.load(std::sync::atomic::Ordering::SeqCst) {
                        1 => {
                            return (
                                axum::http::StatusCode::FORBIDDEN,
                                axum::Json(
                                    serde_json::json!({"code": 50001, "message": "Missing Access"}),
                                ),
                            );
                        }
                        2 => std::future::pending::<()>().await,
                        _ => {}
                    }
                }
                let metadata = match uri.path().rsplit('/').next() {
                    Some(id) if id == child.to_string() => serde_json::json!({
                        "id": child.to_string(), "type": 11, "guild_id": "73490000",
                        "parent_id": parent.to_string(), "name": "inherit-thread"
                    }),
                    Some(id) if id == parent.to_string() => serde_json::json!({
                        "id": parent.to_string(), "type": 0, "guild_id": "73490000",
                        "name": "parent"
                    }),
                    _ => panic!("unexpected metadata request: {uri}"),
                };
                (axum::http::StatusCode::OK, axum::Json(metadata))
            }
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
        let prompt = root.path().join("parent-role.md");
        let workspace = root.path().join("parent-workspace");
        std::fs::write(&prompt, "parent role").unwrap();
        std::fs::create_dir(&workspace).unwrap();
        for (binding_kind, binding) in [
            ("id", serde_json::json!({"id": parent.to_string()})),
            ("name", serde_json::json!({"name": "parent"})),
            ("alias", serde_json::json!({"aliases": ["parent"]})),
        ] {
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
                let mut channel = binding.clone();
                channel["prompt_file"] = serde_json::json!(prompt);
                channel["workspace"] = serde_json::json!(workspace);
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
                let enabled = configured.unwrap_or(true);
                assert_eq!(
                    resolve(&deps, &pool, child, false).await.unwrap(),
                    if enabled { parent } else { child }.to_string(),
                    "binding={binding_kind}, YAML={configured:?}, legacy={legacy_value}"
                );
                let parent_metadata = crate::services::discord::resolve_thread_parent(&http, child)
                    .await
                    .unwrap();
                assert_eq!(parent_metadata, (parent, Some("parent".to_owned())));
                let (has_role, inherited_workspace, memory_channel) =
                    super::super::super::message_handler::thread_inheritance_for_tests(
                        child,
                        Some("inherit-thread"),
                        &parent_metadata,
                    );
                assert_eq!(has_role, enabled, "role: {binding_kind}/{configured:?}");
                assert_eq!(
                    inherited_workspace.as_deref(),
                    enabled.then(|| workspace.to_str().unwrap()),
                    "workspace: {binding_kind}/{configured:?}"
                );
                assert_eq!(
                    memory_channel,
                    if enabled { parent } else { child },
                    "memory: {binding_kind}/{configured:?}"
                );
            }
        }
        crate::config_live_reload::install(crate::config::Config::default());
        assert_eq!(
            resolve(&deps, &pool, child, false).await.unwrap(),
            parent.to_string()
        );
        parent_behavior.store(1, std::sync::atomic::Ordering::SeqCst);
        assert!(
            resolve(&deps, &pool, child, false)
                .await
                .unwrap_err()
                .starts_with("Discord channel policy lookup failed:")
        );
        assert_eq!(
            crate::services::discord::resolve_thread_parent(&http, child).await,
            Some((parent, None)),
            "best-effort parent lookup keeps its error behavior"
        );
        parent_behavior.store(2, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            tokio::time::timeout(
                POLICY_LOOKUP_TIMEOUT * 2,
                resolve(&deps, &pool, child, false)
            )
            .await
            .unwrap()
            .unwrap_err(),
            "Discord channel policy lookup timed out"
        );
        assert_eq!(
            resolve(&deps, &pool, child, true).await.unwrap(),
            child.to_string()
        );
        server.abort();
        pool.close().await;
        db.drop().await;
    }
}
