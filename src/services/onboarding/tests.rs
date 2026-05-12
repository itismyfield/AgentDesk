#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::{
        Arc, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{Router, body::Body, extract::Path as AxumPath, http::Request, routing::get};
    use tower::ServiceExt;

    fn env_guard() -> MutexGuard<'static, ()> {
        crate::services::discord::runtime_store::lock_test_env()
    }

    struct RuntimeRootGuard {
        _lock: MutexGuard<'static, ()>,
        previous: Option<std::path::PathBuf>,
    }

    impl RuntimeRootGuard {
        fn new(path: &Path) -> Self {
            let lock = env_guard();
            let previous = crate::config::current_test_runtime_root_override();
            crate::config::set_test_runtime_root_override(Some(path.to_path_buf()));
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for RuntimeRootGuard {
        fn drop(&mut self) {
            crate::config::set_test_runtime_root_override(self.previous.take());
        }
    }

    fn test_db() -> crate::db::Db {
        crate::db::test_db()
    }

    fn test_engine(db: &crate::db::Db) -> crate::engine::PolicyEngine {
        crate::engine::PolicyEngine::new_with_legacy_db(
            &crate::config::Config::default(),
            db.clone(),
        )
        .unwrap()
    }

    const VALID_OWNER_ID: &str = "123456789012345678";
    const VALID_OWNER_ID_U64: u64 = 123_456_789_012_345_678;

    fn sample_complete_body(
        channel_id: &str,
        channel_name: &str,
        rerun_policy: Option<&str>,
    ) -> CompleteBody {
        CompleteBody {
            token: "command-token".to_string(),
            announce_token: None,
            notify_token: None,
            command_token_2: None,
            command_provider_2: None,
            guild_id: "123".to_string(),
            owner_id: Some(VALID_OWNER_ID.to_string()),
            provider: Some("codex".to_string()),
            channels: vec![ChannelMapping {
                channel_id: channel_id.to_string(),
                channel_name: channel_name.to_string(),
                role_id: "adk-cdx".to_string(),
                description: Some("dispatch desk".to_string()),
                system_prompt: Some("be precise".to_string()),
            }],
            template: Some("operations".to_string()),
            rerun_policy: rerun_policy.map(str::to_string),
        }
    }

    fn sample_draft() -> OnboardingDraft {
        OnboardingDraft {
            version: ONBOARDING_DRAFT_VERSION,
            updated_at_ms: 1,
            step: 4,
            command_bots: vec![
                OnboardingDraftCommandBot {
                    provider: "codex".to_string(),
                    token: "command-token".to_string(),
                    bot_info: Some(OnboardingDraftBotInfo {
                        valid: true,
                        bot_id: Some("100".to_string()),
                        bot_name: Some("command".to_string()),
                        error: None,
                    }),
                },
                OnboardingDraftCommandBot {
                    provider: "claude".to_string(),
                    token: "command-token-2".to_string(),
                    bot_info: Some(OnboardingDraftBotInfo {
                        valid: true,
                        bot_id: Some("101".to_string()),
                        bot_name: Some("command-2".to_string()),
                        error: None,
                    }),
                },
            ],
            announce_token: "announce-token".to_string(),
            notify_token: "notify-token".to_string(),
            announce_bot_info: Some(OnboardingDraftBotInfo {
                valid: true,
                bot_id: Some("200".to_string()),
                bot_name: Some("announce".to_string()),
                error: None,
            }),
            notify_bot_info: Some(OnboardingDraftBotInfo {
                valid: true,
                bot_id: Some("300".to_string()),
                bot_name: Some("notify".to_string()),
                error: None,
            }),
            provider_statuses: BTreeMap::from([
                (
                    "codex".to_string(),
                    OnboardingDraftProviderStatus {
                        installed: true,
                        logged_in: true,
                        version: Some("1.2.3".to_string()),
                    },
                ),
                (
                    "claude".to_string(),
                    OnboardingDraftProviderStatus {
                        installed: true,
                        logged_in: false,
                        version: Some("9.9.9".to_string()),
                    },
                ),
            ]),
            selected_template: Some("operations".to_string()),
            agents: vec![OnboardingDraftAgent {
                id: "adk-cdx".to_string(),
                name: "Dispatch Desk".to_string(),
                name_en: Some("Dispatch Desk".to_string()),
                description: "dispatch desk".to_string(),
                description_en: Some("dispatch desk".to_string()),
                prompt: "be precise".to_string(),
                custom: true,
            }],
            custom_name: "Dispatch Desk".to_string(),
            custom_desc: "dispatch desk".to_string(),
            custom_name_en: "Dispatch Desk".to_string(),
            custom_desc_en: "dispatch desk".to_string(),
            expanded_agent: Some("adk-cdx".to_string()),
            selected_guild: "guild-123".to_string(),
            channel_assignments: vec![OnboardingDraftChannelAssignment {
                agent_id: "adk-cdx".to_string(),
                agent_name: "Dispatch Desk".to_string(),
                recommended_name: "adk-cdx-cdx".to_string(),
                channel_id: "1234".to_string(),
                channel_name: "dispatch-room".to_string(),
            }],
            owner_id: VALID_OWNER_ID.to_string(),
            has_existing_setup: false,
            confirm_rerun_overwrite: false,
        }
    }

    async fn spawn_mock_discord_server() -> (String, Arc<AtomicUsize>) {
        let post_count = Arc::new(AtomicUsize::new(0));
        let channels = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let channels_for_get = channels.clone();
        let channels_for_post = channels.clone();
        let post_count_for_route = post_count.clone();
        let app = Router::new().route(
            "/guilds/{guild_id}/channels",
            get(move |AxumPath(_guild_id): AxumPath<String>| {
                let channels = channels_for_get.clone();
                async move { Json(channels.lock().unwrap().clone()) }
            })
            .post(
                move |AxumPath(_guild_id): AxumPath<String>,
                      Json(body): Json<serde_json::Value>| {
                    let channels = channels_for_post.clone();
                    let post_count = post_count_for_route.clone();
                    async move {
                        let count = post_count.fetch_add(1, Ordering::SeqCst) + 1;
                        let created = json!({
                            "id": (1000 + count).to_string(),
                            "name": body.get("name").and_then(|value| value.as_str()).unwrap_or("created"),
                            "type": 0
                        });
                        channels.lock().unwrap().push(created.clone());
                        Json(created)
                    }
                },
            ),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{}", addr), post_count)
    }

    #[tokio::test]
    async fn draft_api_round_trip_redacts_tokens_and_exposes_resume_state() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let app = Router::new()
            .route(
                "/draft",
                axum::routing::get(crate::server::routes::onboarding::draft_get)
                    .put(crate::server::routes::onboarding::draft_put)
                    .delete(draft_delete),
            )
            .route(
                "/status",
                axum::routing::get(crate::server::routes::onboarding::status),
            )
            .with_state(state);

        let put_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/draft")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&sample_draft()).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put_response.status(), StatusCode::OK);
        let put_body = axum::body::to_bytes(put_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let put_json: serde_json::Value = serde_json::from_slice(&put_body).unwrap();
        assert_eq!(put_json["ok"], json!(true));
        assert_eq!(put_json["draft"]["command_bots"][0]["token"], json!(""));
        assert_eq!(put_json["draft"]["announce_token"], json!(""));
        assert_eq!(put_json["draft"]["notify_token"], json!(""));
        assert_eq!(put_json["draft"]["updated_at_ms"], json!(1));
        assert_eq!(
            put_json["secret_policy"]["cleared_on_complete"],
            json!(true)
        );

        let status_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status_response.status(), StatusCode::OK);
        let status_body = axum::body::to_bytes(status_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let status_json: serde_json::Value = serde_json::from_slice(&status_body).unwrap();
        assert_eq!(status_json["setup_mode"], json!("fresh"));
        assert_eq!(status_json["draft_available"], json!(true));
        assert_eq!(status_json["resume_state"], json!("draft_available"));

        let get_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/draft")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get_response.status(), StatusCode::OK);
        let get_body = axum::body::to_bytes(get_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let get_json: serde_json::Value = serde_json::from_slice(&get_body).unwrap();
        assert_eq!(get_json["available"], json!(true));
        assert_eq!(get_json["draft"]["command_bots"][0]["token"], json!(""));
        assert_eq!(get_json["draft"]["announce_token"], json!(""));
        assert_eq!(get_json["draft"]["notify_token"], json!(""));
        assert_eq!(get_json["draft"]["selected_template"], json!("operations"));
        assert_eq!(get_json["secret_policy"]["stores_raw_tokens"], json!(false));
        assert_eq!(
            get_json["secret_policy"]["returns_raw_tokens_in_draft"],
            json!(false)
        );

        let delete_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/draft")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delete_response.status(), StatusCode::OK);

        let status_after_delete = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status_after_delete_body =
            axum::body::to_bytes(status_after_delete.into_body(), usize::MAX)
                .await
                .unwrap();
        let status_after_delete_json: serde_json::Value =
            serde_json::from_slice(&status_after_delete_body).unwrap();
        assert_eq!(status_after_delete_json["draft_available"], json!(false));
        assert_eq!(status_after_delete_json["resume_state"], json!("none"));
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, contents).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn write_agentdesk_discord_config_prefers_config_dir_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(
            root.join("config").join("agentdesk.yaml"),
            "server:\n  port: 8791\n",
        )
        .unwrap();

        write_agentdesk_discord_config(
            root,
            "guild-123",
            "primary-token",
            "claude",
            None,
            None,
            Some(VALID_OWNER_ID),
        )
        .unwrap();

        assert!(!root.join("agentdesk.yaml").exists());
        let config =
            crate::config::load_from_path(&root.join("config").join("agentdesk.yaml")).unwrap();
        assert_eq!(config.server.port, 8791);
        assert_eq!(config.discord.guild_id.as_deref(), Some("guild-123"));
        assert_eq!(config.discord.owner_id, Some(VALID_OWNER_ID_U64));
        assert_eq!(
            config.discord.bots["command"].provider.as_deref(),
            Some("claude")
        );
        assert_eq!(
            config.discord.bots["command"].token.as_deref(),
            Some("primary-token")
        );
    }

    #[test]
    fn write_discord_and_credential_artifacts_use_runtime_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        write_agentdesk_discord_config(
            root,
            "guild-123",
            "primary-token",
            "claude",
            Some("secondary-token"),
            Some("codex"),
            Some(VALID_OWNER_ID),
        )
        .unwrap();
        write_credential_token(root, "announce", Some("announce-token")).unwrap();
        write_credential_token(root, "notify", Some("notify-token")).unwrap();

        let config =
            crate::config::load_from_path(&root.join("config").join("agentdesk.yaml")).unwrap();
        assert_eq!(config.discord.guild_id.as_deref(), Some("guild-123"));
        assert_eq!(config.discord.owner_id, Some(VALID_OWNER_ID_U64));
        assert_eq!(config.discord.bots.len(), 2);
        assert_eq!(
            config.discord.bots["command"].provider.as_deref(),
            Some("claude")
        );
        assert_eq!(
            config.discord.bots["command_2"].provider.as_deref(),
            Some("codex")
        );

        assert_eq!(
            std::fs::read_to_string(crate::runtime_layout::credential_token_path(
                root, "announce"
            ))
            .unwrap(),
            "announce-token\n"
        );
        assert_eq!(
            std::fs::read_to_string(crate::runtime_layout::credential_token_path(root, "notify"))
                .unwrap(),
            "notify-token\n"
        );
        assert!(
            std::fs::symlink_metadata(crate::runtime_layout::legacy_credential_dir(root))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(
                crate::runtime_layout::legacy_credential_dir(root).join("announce_bot_token"),
            )
            .unwrap(),
            "announce-token\n"
        );
    }

    #[test]
    fn write_agentdesk_discord_config_rejects_short_owner_id() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        let error = write_agentdesk_discord_config(
            root,
            "guild-123",
            "primary-token",
            "claude",
            None,
            None,
            Some("7"),
        )
        .unwrap_err();

        assert!(error.contains("owner_id must be a Discord user id"));
    }

    #[test]
    fn desired_channel_name_strips_leading_hash() {
        let mapping = ChannelMapping {
            channel_id: String::new(),
            channel_name: "#agentdesk-cdx".to_string(),
            role_id: "adk-cdx".to_string(),
            description: None,
            system_prompt: None,
        };

        assert_eq!(desired_channel_name(&mapping).unwrap(), "agentdesk-cdx");
    }

    #[tokio::test]
    async fn resolve_channel_mapping_reuses_existing_channel() {
        let post_count = Arc::new(AtomicUsize::new(0));
        let post_count_for_route = post_count.clone();
        let app = Router::new().route(
            "/guilds/{guild_id}/channels",
            get(|AxumPath(_guild_id): AxumPath<String>| async move {
                Json(json!([
                    {"id": "42", "name": "agentdesk-cdx", "type": 0}
                ]))
            })
            .post(
                move |AxumPath(_guild_id): AxumPath<String>,
                      Json(body): Json<serde_json::Value>| {
                    let post_count = post_count_for_route.clone();
                    async move {
                        post_count.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "id": "77",
                            "name": body.get("name").and_then(|value| value.as_str()).unwrap_or("created"),
                            "type": 0
                        }))
                    }
                },
            ),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mapping = ChannelMapping {
            channel_id: "agentdesk-cdx".to_string(),
            channel_name: "agentdesk-cdx".to_string(),
            role_id: "adk-cdx".to_string(),
            description: Some("desc".to_string()),
            system_prompt: Some("prompt".to_string()),
        };

        let resolved = resolve_channel_mapping(
            &reqwest::Client::new(),
            "token",
            &format!("http://{}", addr),
            "123",
            &mapping,
            None,
        )
        .await
        .unwrap();

        assert_eq!(resolved.channel_id, "42");
        assert_eq!(resolved.channel_name, "agentdesk-cdx");
        assert!(!resolved.created);
        assert_eq!(post_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resolve_channel_mapping_creates_missing_channel() {
        let post_count = Arc::new(AtomicUsize::new(0));
        let post_count_for_route = post_count.clone();
        let app = Router::new().route(
            "/guilds/{guild_id}/channels",
            get(|AxumPath(_guild_id): AxumPath<String>| async move { Json(json!([])) }).post(
                move |AxumPath(_guild_id): AxumPath<String>,
                      Json(body): Json<serde_json::Value>| {
                    let post_count = post_count_for_route.clone();
                    async move {
                        post_count.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "id": "77",
                            "name": body.get("name").and_then(|value| value.as_str()).unwrap_or("created"),
                            "type": 0
                        }))
                    }
                },
            ),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mapping = ChannelMapping {
            channel_id: "agentdesk-cdx".to_string(),
            channel_name: "agentdesk-cdx".to_string(),
            role_id: "adk-cdx".to_string(),
            description: Some("desc".to_string()),
            system_prompt: Some("prompt".to_string()),
        };

        let resolved = resolve_channel_mapping(
            &reqwest::Client::new(),
            "token",
            &format!("http://{}", addr),
            "123",
            &mapping,
            None,
        )
        .await
        .unwrap();

        assert_eq!(resolved.channel_id, "77");
        assert_eq!(resolved.channel_name, "agentdesk-cdx");
        assert!(resolved.created);
        assert_eq!(post_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn complete_retries_from_partial_state_without_duplicate_channel_creation() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let (discord_api_base, post_count) = spawn_mock_discord_server().await;
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let body = sample_complete_body("agentdesk-cdx", "agentdesk-cdx", Some("reuse_existing"));

        let failure_options = CompleteExecutionOptions {
            discord_api_base: discord_api_base.clone(),
            fail_after_stage: Some(OnboardingCompletionStage::ArtifactsPersisted),
        };
        let (failed_status, failed_body) =
            complete_with_options(&state, &body, &failure_options).await;
        assert_eq!(failed_status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(failed_body["partial_apply"], json!(true));
        assert_eq!(
            failed_body["completion_state"]["stage"],
            json!("artifacts_persisted")
        );
        assert_eq!(post_count.load(Ordering::SeqCst), 1);

        let status_state = AppState::test_state(db.clone(), test_engine(&db));
        let (status_code, Json(status_body)) = status(&status_state).await;
        assert_eq!(status_code, StatusCode::OK);
        assert_eq!(status_body["partial_apply"], json!(true));
        assert_eq!(
            status_body["completion_state"]["channels"][0]["channel_id"],
            json!("1001")
        );

        let success_options = CompleteExecutionOptions {
            discord_api_base,
            fail_after_stage: None,
        };
        let (success_status, success_body) =
            complete_with_options(&state, &body, &success_options).await;
        assert_eq!(success_status, StatusCode::OK);
        assert_eq!(success_body["ok"], json!(true));
        assert_eq!(success_body["partial_apply"], json!(false));
        assert_eq!(post_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            success_body["artifacts"]["channel_mappings"][0]["resolution"],
            json!("checkpoint")
        );

        let conn = db.read_conn().unwrap();
        let stored_channel: String = conn
            .query_row(
                "SELECT discord_channel_id FROM agents WHERE id = 'adk-cdx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_channel, "1001");
    }

    #[tokio::test]
    async fn status_omits_invalid_legacy_owner_id_from_rerun_payload() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                sqlite_test::params!["onboarding_owner_id", "42"],
            )
            .unwrap();
        }

        let state = AppState::test_state(db.clone(), test_engine(&db));
        let (status_code, Json(status_body)) = status(&state).await;

        assert_eq!(status_code, StatusCode::OK);
        assert_eq!(status_body["owner_id"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn draft_get_sanitizes_invalid_legacy_owner_id_from_saved_draft() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let app = Router::new()
            .route(
                "/draft",
                axum::routing::get(crate::server::routes::onboarding::draft_get),
            )
            .with_state(state);

        let mut legacy_draft = sample_draft();
        legacy_draft.owner_id = "42".to_string();
        save_onboarding_draft(temp.path(), &legacy_draft).unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/draft")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["draft"]["owner_id"], json!(""));
    }

    #[tokio::test]
    async fn complete_retries_from_channels_resolved_partial_state() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let (discord_api_base, post_count) = spawn_mock_discord_server().await;
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let body = sample_complete_body("agentdesk-cdx", "agentdesk-cdx", Some("reuse_existing"));

        let failure_options = CompleteExecutionOptions {
            discord_api_base: discord_api_base.clone(),
            fail_after_stage: Some(OnboardingCompletionStage::ChannelsResolved),
        };
        let (failed_status, failed_body) =
            complete_with_options(&state, &body, &failure_options).await;
        assert_eq!(failed_status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(failed_body["partial_apply"], json!(true));
        assert_eq!(
            failed_body["completion_state"]["stage"],
            json!("channels_resolved")
        );
        assert_eq!(post_count.load(Ordering::SeqCst), 1);
        assert!(!onboarding_config_path(temp.path()).is_file());

        let success_options = CompleteExecutionOptions {
            discord_api_base,
            fail_after_stage: None,
        };
        let (success_status, success_body) =
            complete_with_options(&state, &body, &success_options).await;
        assert_eq!(success_status, StatusCode::OK);
        assert_eq!(success_body["ok"], json!(true));
        assert_eq!(success_body["partial_apply"], json!(false));
        assert_eq!(post_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            success_body["artifacts"]["channel_mappings"][0]["resolution"],
            json!("checkpoint")
        );
        assert!(onboarding_config_path(temp.path()).is_file());
    }

    #[tokio::test]
    async fn complete_keeps_existing_draft_on_failure_and_clears_it_on_success() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let (discord_api_base, _post_count) = spawn_mock_discord_server().await;
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let app = Router::new()
            .route(
                "/draft",
                axum::routing::get(crate::server::routes::onboarding::draft_get),
            )
            .with_state(state.clone());
        let body = sample_complete_body("agentdesk-cdx", "agentdesk-cdx", Some("reuse_existing"));

        save_onboarding_draft(temp.path(), &sample_draft().normalize().unwrap()).unwrap();
        assert!(onboarding_draft_path(temp.path()).is_file());

        let failure_options = CompleteExecutionOptions {
            discord_api_base: discord_api_base.clone(),
            fail_after_stage: Some(OnboardingCompletionStage::ArtifactsPersisted),
        };
        let (failed_status, failed_body) =
            complete_with_options(&state, &body, &failure_options).await;
        assert_eq!(failed_status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(failed_body["partial_apply"], json!(true));
        let retained_draft = load_onboarding_draft(temp.path()).unwrap().unwrap();
        assert_eq!(retained_draft.command_bots[0].token, "command-token");

        let success_options = CompleteExecutionOptions {
            discord_api_base,
            fail_after_stage: None,
        };
        let (success_status, success_body) =
            complete_with_options(&state, &body, &success_options).await;
        assert_eq!(success_status, StatusCode::OK);
        assert_eq!(success_body["ok"], json!(true));
        assert!(load_onboarding_draft(temp.path()).unwrap().is_none());
        let draft_get_response = app
            .oneshot(
                Request::builder()
                    .uri("/draft")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(draft_get_response.status(), StatusCode::OK);
        let draft_get_body = axum::body::to_bytes(draft_get_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let draft_get_json: serde_json::Value = serde_json::from_slice(&draft_get_body).unwrap();
        assert_eq!(draft_get_json["available"], json!(false));

        let status_state = AppState::test_state(db.clone(), test_engine(&db));
        let (status_code, Json(status_body)) = status(&status_state).await;
        assert_eq!(status_code, StatusCode::OK);
        assert_eq!(status_body["setup_mode"], json!("rerun"));
        assert_eq!(status_body["draft_available"], json!(false));
        assert_eq!(status_body["resume_state"], json!("none"));
    }

    #[tokio::test]
    async fn draft_put_rejects_oversized_payload() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let app = Router::new()
            .route(
                "/draft",
                axum::routing::put(crate::server::routes::onboarding::draft_put),
            )
            .with_state(state);

        let mut oversized = sample_draft();
        oversized.agents = (0..80)
            .map(|index| OnboardingDraftAgent {
                id: format!("agent-{index}"),
                name: format!("Agent {index}"),
                name_en: Some(format!("Agent {index}")),
                description: "desc".to_string(),
                description_en: Some("desc".to_string()),
                prompt: "prompt".repeat(32),
                custom: true,
            })
            .collect();

        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/draft")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&oversized).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("max agents")
        );
    }

    #[tokio::test]
    async fn draft_put_rejects_invalid_owner_id() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let app = Router::new()
            .route(
                "/draft",
                axum::routing::put(crate::server::routes::onboarding::draft_put),
            )
            .with_state(state);

        let mut draft = sample_draft();
        draft.owner_id = "42".to_string();

        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/draft")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&draft).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("owner_id must be a Discord user id")
        );
        assert!(load_onboarding_draft(temp.path()).unwrap().is_none());
    }

    #[test]
    fn requested_channel_fingerprint_includes_provider() {
        let body = sample_complete_body("agentdesk-cdx", "agentdesk-cdx", Some("reuse_existing"));
        let claude = requested_channel_fingerprint(&body, "claude").unwrap();
        let codex = requested_channel_fingerprint(&body, "codex").unwrap();
        assert_ne!(claude, codex);
    }

    #[test]
    fn load_onboarding_completion_state_ignores_corrupt_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = onboarding_completion_state_path(temp.path());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, "{not-json").unwrap();

        let state = load_onboarding_completion_state(temp.path()).unwrap();
        assert!(state.is_none());
        assert!(!path.exists());
        let archived = path
            .parent()
            .unwrap()
            .read_dir()
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("onboarding_completion_state.json.corrupt-")
            });
        assert!(archived);
    }

    #[tokio::test]
    async fn complete_rejects_empty_guild_id_even_with_numeric_channels() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let mut body = sample_complete_body("9001", "agentdesk-cdx", Some("reuse_existing"));
        body.guild_id = "   ".to_string();

        let (status, response) =
            complete_with_options(&state, &body, &CompleteExecutionOptions::default()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            response["error"],
            json!("guild_id is required for onboarding completion")
        );
        assert!(
            load_onboarding_completion_state(temp.path())
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn complete_requires_explicit_replace_policy_before_overwriting_agent() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let db = test_db();
        let config_path = onboarding_config_path(temp.path());
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut config = crate::config::Config::default();
        config.agents.push(crate::config::AgentDef {
            id: "adk-cdx".to_string(),
            name: "adk-cdx".to_string(),
            name_ko: None,
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            provider: "claude".to_string(),
            channels: crate::config::AgentChannels {
                codex: Some(crate::config::AgentChannel::Detailed(
                    crate::config::AgentChannelConfig {
                        id: Some("5555".to_string()),
                        name: Some("legacy-cdx".to_string()),
                        aliases: Vec::new(),
                        prompt_file: None,
                        workspace: Some("~/legacy".to_string()),
                        provider: Some("codex".to_string()),
                        model: None,
                        reasoning_effort: None,
                        peer_agents: None,
                        quality_feedback_injection: None,
                        dispatch_profile: None,
                        cache_ttl_minutes: None,
                    },
                )),
                ..crate::config::AgentChannels::default()
            },
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
        });
        crate::config::save_to_path(&config_path, &config).unwrap();
        let role_map_path = crate::runtime_layout::role_map_path(temp.path());
        if let Some(parent) = role_map_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            &role_map_path,
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "byChannelId": {
                    "5555": {
                        "roleId": "adk-cdx",
                        "provider": "codex",
                        "workspace": "~/legacy"
                    }
                },
                "byChannelName": {
                    "legacy-cdx": {
                        "roleId": "adk-cdx",
                        "channelId": "5555",
                        "workspace": "~/legacy"
                    }
                },
                "fallbackByChannelName": { "enabled": true }
            }))
            .unwrap(),
        )
        .unwrap();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, discord_channel_id, description, system_prompt, status, xp) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', 0)",
                sqlite_test::params![
                    "adk-cdx",
                    "adk-cdx",
                    "claude",
                    "5555",
                    "existing desc",
                    "existing prompt"
                ],
            )
            .unwrap();
        }

        let state = AppState::test_state(db.clone(), test_engine(&db));
        let reuse_body = sample_complete_body("9001", "agentdesk-cdx", Some("reuse_existing"));
        let (conflict_status, conflict_body) =
            complete_with_options(&state, &reuse_body, &CompleteExecutionOptions::default()).await;
        assert_eq!(conflict_status, StatusCode::CONFLICT);
        assert_eq!(conflict_body["partial_apply"], json!(false));
        assert_eq!(conflict_body["retry_recommended"], json!(false));
        assert_eq!(
            conflict_body["conflicts"][0]
                .as_str()
                .unwrap()
                .contains("rerun_policy=reuse_existing"),
            true
        );

        let replace_body = sample_complete_body("9001", "agentdesk-cdx", Some("replace_existing"));
        let (replace_status, replace_body_json) =
            complete_with_options(&state, &replace_body, &CompleteExecutionOptions::default())
                .await;
        assert_eq!(replace_status, StatusCode::OK);
        assert_eq!(replace_body_json["ok"], json!(true));
        assert_eq!(
            replace_body_json["rerun_policy"]["applied"],
            json!("replace_existing")
        );

        let conn = db.read_conn().unwrap();
        let (provider, channel_id, description, prompt): (
            String,
            String,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT provider, discord_channel_id, description, system_prompt \
                 FROM agents WHERE id = 'adk-cdx'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(provider, "codex");
        assert_eq!(channel_id, "9001");
        assert_eq!(description.as_deref(), Some("dispatch desk"));
        assert_eq!(prompt.as_deref(), Some("be precise"));

        let saved_config = crate::config::load_from_path(&config_path).unwrap();
        let saved_agent = saved_config
            .agents
            .iter()
            .find(|agent| agent.id == "adk-cdx")
            .unwrap();
        let saved_channel = saved_agent
            .channels
            .codex
            .as_ref()
            .and_then(crate::config::AgentChannel::target)
            .unwrap();
        assert_eq!(saved_channel, "9001");

        let saved_role_map: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&role_map_path).unwrap()).unwrap();
        assert!(saved_role_map["byChannelId"].get("5555").is_none());
        assert!(saved_role_map["byChannelName"].get("legacy-cdx").is_none());
        assert_eq!(
            saved_role_map["byChannelId"]["9001"]["roleId"],
            json!("adk-cdx")
        );
        assert_eq!(
            saved_role_map["byChannelName"]["agentdesk-cdx"]["channelId"],
            json!("9001")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_provider_uses_resolver_exec_path_under_minimal_path() {
        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("provider-helper");
        let provider = temp.path().join("claude");
        let original_path = std::env::var_os("PATH");
        let original_home = std::env::var_os("HOME");

        write_executable(&helper, "#!/bin/sh\nprintf 'claude-test 9.9.9\\n'\n");
        write_executable(
            &provider,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  provider-helper\nelse\n  exit 64\nfi\n",
        );

        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::set_var("HOME", temp.path());
            std::env::set_var("AGENTDESK_CLAUDE_PATH", &provider);
        }

        let (status, Json(body)) = check_provider(CheckProviderBody {
            provider: "claude".to_string(),
        })
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["installed"], json!(true));
        assert_eq!(body["logged_in"], json!(false));
        assert_eq!(body["version"], json!("claude-test 9.9.9"));
        assert_eq!(body["source"], json!("env_override"));
        assert_eq!(body["path"], json!(provider.to_string_lossy().to_string()));

        unsafe {
            std::env::remove_var("AGENTDESK_CLAUDE_PATH");
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match original_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_provider_reports_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("claude");
        let original_path = std::env::var_os("PATH");
        let original_home = std::env::var_os("HOME");

        std::fs::write(&provider, "#!/bin/sh\nprintf 'claude-test 9.9.9\\n'\n").unwrap();
        let mut perms = std::fs::metadata(&provider).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&provider, perms).unwrap();

        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::set_var("HOME", temp.path());
            std::env::set_var("AGENTDESK_CLAUDE_PATH", &provider);
        }

        let (status, Json(body)) = check_provider(CheckProviderBody {
            provider: "claude".to_string(),
        })
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["installed"], json!(false));
        assert_eq!(body["version"], json!(null));
        assert_eq!(body["failure_kind"], json!("permission_denied"));
        assert_eq!(body["path"], json!(null));

        unsafe {
            std::env::remove_var("AGENTDESK_CLAUDE_PATH");
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match original_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_provider_reports_opencode_permission_denied_with_attempts() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("opencode");
        let original_path = std::env::var_os("PATH");
        let original_home = std::env::var_os("HOME");

        std::fs::write(&provider, "#!/bin/sh\nprintf 'opencode 9.9.9\\n'\n").unwrap();
        let mut perms = std::fs::metadata(&provider).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&provider, perms).unwrap();

        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::set_var("HOME", temp.path());
            std::env::set_var("AGENTDESK_OPENCODE_PATH", &provider);
        }

        let (status, Json(body)) = check_provider(CheckProviderBody {
            provider: "opencode".to_string(),
        })
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["installed"], json!(false));
        assert_eq!(body["logged_in"], json!(false));
        assert_eq!(body["version"], json!(null));
        assert_eq!(body["failure_kind"], json!("permission_denied"));
        assert_eq!(body["path"], json!(null));
        assert!(
            body["attempts"]
                .as_array()
                .is_some_and(|attempts| !attempts.is_empty())
        );

        unsafe {
            std::env::remove_var("AGENTDESK_OPENCODE_PATH");
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match original_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}
