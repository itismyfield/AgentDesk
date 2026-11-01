use super::*;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

struct Rig {
    ctx: serenity::Context,
    replies: Arc<std::sync::Mutex<Vec<Value>>>,
    servers: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
    }
}

impl Rig {
    async fn new() -> Self {
        Self::new_for_channel("example-cc").await
    }

    async fn new_for_channel(name: &str) -> Self {
        let mock = discord_mock::DiscordMockState::new();
        mock.name_channel(discord_mock::CHANNEL_ID, name);
        let (backend, gateway, server) = discord_mock::start(mock).await;
        let replies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = replies.clone();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let app = axum::Router::new()
            .route(
                "/api/v10/interactions/{id}/{token}/callback",
                axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                    let recorded = recorded.clone();
                    async move {
                        recorded.lock().unwrap().push(body);
                        axum::http::StatusCode::NO_CONTENT
                    }
                }),
            )
            .fallback(move |request: axum::http::Request<axum::body::Body>| {
                let client = client.clone();
                let backend = backend.clone();
                async move {
                    let response = client
                        .get(format!("{backend}{}", request.uri()))
                        .send()
                        .await
                        .unwrap();
                    let status = response.status();
                    (status, response.bytes().await.unwrap())
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let frontend = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let ctx = discord_mock::serenity_context(format!("http://{address}"), gateway).await;
        Self {
            ctx,
            replies,
            servers: vec![server, frontend],
        }
    }

    async fn invoke(&self, data: &Data, name: &str, focused: Option<(&str, &str)>) -> Value {
        let options = run_bot_framework_options(run_bot_build_slash_commands());
        let (root_name, child_name) = name
            .split_once(' ')
            .map_or((name, None), |(root, child)| (root, Some(child)));
        let root_command = options
            .commands
            .iter()
            .find(|c| c.name == root_name)
            .unwrap();
        let mut command_tree = vec![root_command];
        if let Some(child) = child_name {
            command_tree.push(
                root_command
                    .subcommands
                    .iter()
                    .find(|command| command.name == child)
                    .unwrap(),
            );
        }
        let (manager, _) =
            serenity::gateway::ShardManager::new(serenity::gateway::ShardManagerOptions {
                data: self.ctx.data.clone(),
                event_handlers: vec![],
                raw_event_handlers: vec![],
                framework: Arc::new(std::sync::OnceLock::new()),
                shard_index: 0,
                shard_init: 0,
                shard_total: 1,
                ws_url: Arc::new(tokio::sync::Mutex::new("ws://127.0.0.1:1".into())),
                cache: self.ctx.cache.clone(),
                http: self.ctx.http.clone(),
                intents: serenity::GatewayIntents::empty(),
                presence: None,
            });
        let mut raw_options = focused.map_or(json!([]), |(parameter, value)| {
            json!([
                {"type": 3, "name": parameter, "value": value, "focused": true}
            ])
        });
        if let Some(child) = child_name {
            raw_options = json!([{"type": 1, "name": child, "options": raw_options}]);
        }
        let mut user = serenity::User::default();
        user.id = serenity::UserId::new(42);
        user.name = "slash-fixture".into();
        let interaction: serenity::CommandInteraction = serde_json::from_value(json!({
            "id": "7678001", "application_id": discord_mock::BOT_ID.to_string(),
            "data": {"id": "7678002", "name": root_name, "type": 1, "resolved": {}, "options": raw_options},
            "guild_id": null, "channel": null, "channel_id": discord_mock::CHANNEL_ID.to_string(),
            "member": null, "user": user, "token": "fixture-token", "version": 1,
            "app_permissions": null, "locale": "en-US", "guild_locale": null,
            "entitlements": [], "context": null, "attachment_size_limit": 8388608
        })).unwrap();
        let args = interaction.data.options();
        let sent = AtomicBool::new(false);
        let invocation_data: tokio::sync::Mutex<Box<dyn std::any::Any + Send + Sync>> =
            tokio::sync::Mutex::new(Box::new(()));
        let app = poise::ApplicationContext {
            interaction: &interaction,
            interaction_type: if focused.is_some() {
                poise::CommandInteractionType::Autocomplete
            } else {
                poise::CommandInteractionType::Command
            },
            args: &args,
            has_sent_initial_response: &sent,
            framework: poise::FrameworkContext {
                serenity_context: &self.ctx,
                options: &options,
                user_data: data,
                shard_manager: &manager,
            },
            command_tree: &command_tree,
            invocation_data: &invocation_data,
            __non_exhaustive: (),
        };
        if focused.is_some() {
            let mut tree = Vec::new();
            let result = poise::dispatch::dispatch_autocomplete(
                app.framework,
                &interaction,
                &sent,
                &invocation_data,
                &args,
                &mut tree,
            )
            .await;
            assert!(
                result.is_ok()
                    || matches!(
                        result,
                        Err(poise::FrameworkError::CommandCheckFailed { .. })
                    )
            );
            let mut replies = self.replies.lock().unwrap();
            assert_eq!(replies.len(), 1, "autocomplete must answer exactly once");
            let mut response = replies.pop().unwrap();
            assert_eq!(
                response["type"], 8,
                "autocomplete cannot send a command reply"
            );
            response["data"].take()
        } else {
            let allowed = options.command_check.unwrap()(app.into()).await.unwrap();
            json!({"allowed": allowed, "replies": *self.replies.lock().unwrap()})
        }
    }
}

fn isolated(work: impl std::future::Future<Output = ()>) {
    let root = tempfile::tempdir().unwrap();
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    crate::config_live_reload::install(crate::config::Config::default());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(work);
}

fn data(provider: ProviderKind) -> Data {
    Data {
        shared: crate::services::discord::make_shared_data_for_tests(),
        token: "fixture-token".into(),
        provider,
    }
}

#[test]
fn wrong_provider_command_explains_which_bot_to_choose_once_ephemerally() {
    isolated(async {
        let rig = Rig::new().await;
        let denied = rig
            .invoke(&data(ProviderKind::Codex), "cancel-queued", None)
            .await;
        assert_eq!(denied["allowed"], false);
        let replies = denied["replies"].as_array().unwrap();
        assert_eq!(
            replies.len(),
            1,
            "a rejected command must explain how to retry"
        );
        assert_eq!(replies[0]["type"], 4);
        assert_eq!(replies[0]["data"]["flags"], 64);
        let content = replies[0]["data"]["content"].as_str().unwrap();
        assert!(content.contains("claude"), "{content}");
        assert!(content.contains("/cancel-queued"), "{content}");
        rig.replies.lock().unwrap().clear();
        let nested = rig
            .invoke(&data(ProviderKind::Codex), "adk inspect", None)
            .await;
        assert_eq!(nested["replies"].as_array().unwrap().len(), 1);
        assert!(
            nested["replies"][0]["data"]["content"]
                .as_str()
                .unwrap()
                .contains("/adk inspect")
        );
        rig.replies.lock().unwrap().clear();
        let allowed = rig
            .invoke(&data(ProviderKind::Claude), "cancel-queued", None)
            .await;
        assert_eq!(allowed["allowed"], true);
        assert_eq!(allowed["replies"], json!([]));
    });
}

#[test]
fn registered_skill_autocomplete_filters_caps_and_hides_unauthorized_results() {
    isolated(async {
        let rig = Rig::new().await;
        let data = data(ProviderKind::Claude);
        data.shared.settings.write().await.owner_user_id = Some(42);
        *data.shared.skills_cache.write().await = (0..30)
            .map(|n| (format!("build-{n:02}"), "설명".repeat(80)))
            .collect();
        let results = rig.invoke(&data, "skill", Some(("skill", "BUILD-0"))).await;
        let choices = results["choices"].as_array().unwrap();
        assert_eq!(choices.len(), 10);
        assert_eq!(choices[0]["value"], "build-00");
        assert!(
            choices
                .iter()
                .all(|choice| choice["name"].as_str().unwrap().chars().count() <= 100)
        );
        let all = rig.invoke(&data, "skill", Some(("skill", ""))).await;
        assert_eq!(all["choices"].as_array().unwrap().len(), 25);
        let empty = rig.invoke(&data, "skill", Some(("skill", "missing"))).await;
        assert_eq!(empty["choices"], json!([]));
        data.shared.settings.write().await.owner_user_id = Some(7);
        let unauthorized = rig.invoke(&data, "skill", Some(("skill", ""))).await;
        assert_eq!(unauthorized["choices"], json!([]));
        data.shared.settings.write().await.allowed_user_ids.push(42);
        let allowed_user = rig
            .invoke(&data, "skill", Some(("skill", "build-00")))
            .await;
        assert_eq!(allowed_user["choices"].as_array().unwrap().len(), 1);
        assert!(
            rig.replies.lock().unwrap().is_empty(),
            "autocomplete must not send messages"
        );
    });
}

#[test]
fn registered_skill_autocomplete_bounds_labels_and_preserves_valid_ids() {
    isolated(async {
        let rig = Rig::new().await;
        let data = data(ProviderKind::Claude);
        data.shared.settings.write().await.owner_user_id = Some(42);
        let maximum = "이".repeat(100);
        *data.shared.skills_cache.write().await = vec![
            (maximum.clone(), "설명\n".repeat(50)),
            ("이".repeat(101), "too long".into()),
            ("".into(), "empty id".into()),
            (maximum.clone(), "duplicate".into()),
        ];
        let result = rig.invoke(&data, "skill", Some(("skill", "이"))).await;
        let choices = result["choices"].as_array().unwrap();
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0]["value"], maximum);
        let label = choices[0]["name"].as_str().unwrap();
        assert_eq!(label.chars().count(), 100);
        assert!(!label.chars().any(char::is_control));
    });
}

async fn session(data: &Data, channel: u64, path: &str) {
    data.shared.core.lock().await.sessions.insert(
        ChannelId::new(channel),
        DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: Some(path.into()),
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            channel_id: Some(channel),
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
        },
    );
}

#[test]
fn registered_cwd_autocomplete_only_shows_the_authorized_current_channel() {
    isolated(async {
        let rig = Rig::new().await;
        let data = data(ProviderKind::Claude);
        data.shared.settings.write().await.owner_user_id = Some(42);
        session(&data, discord_mock::CHANNEL_ID, "/work/현재-작업").await;
        session(&data, discord_mock::CHANNEL_ID + 1, "/work/other-private").await;
        let found = rig
            .invoke(&data, "resume", Some(("cwd", "/work/현재")))
            .await;
        assert_eq!(found["choices"].as_array().unwrap().len(), 1);
        assert_eq!(found["choices"][0]["value"], "/work/현재-작업");
        assert!(
            found["choices"][0]["name"]
                .as_str()
                .unwrap()
                .contains("현재-작업")
        );
        let codex_rig = Rig::new_for_channel("example-cdx").await;
        let codex = Data {
            shared: data.shared.clone(),
            token: data.token.clone(),
            provider: ProviderKind::Codex,
        };
        let codex_path = codex_rig
            .invoke(&codex, "resume", Some(("cwd", "현재")))
            .await;
        assert_eq!(codex_path["choices"][0]["value"], "/work/현재-작업");
        for partial in ["other-private", "missing"] {
            let empty = rig.invoke(&data, "resume", Some(("cwd", partial))).await;
            assert_eq!(empty["choices"], json!([]));
        }
        let all = rig.invoke(&data, "resume", Some(("cwd", ""))).await;
        assert!(all["choices"].as_array().unwrap().len() <= 25);
        session(
            &data,
            discord_mock::CHANNEL_ID,
            &format!("/work/{}", "이".repeat(100)),
        )
        .await;
        let oversized = rig.invoke(&data, "resume", Some(("cwd", ""))).await;
        assert_eq!(
            oversized["choices"],
            json!([]),
            "never truncate a path value"
        );
        session(&data, discord_mock::CHANNEL_ID, "/work/현재-작업").await;
        data.shared.settings.write().await.owner_user_id = Some(7);
        let unauthorized = rig.invoke(&data, "resume", Some(("cwd", ""))).await;
        assert_eq!(unauthorized["choices"], json!([]));
        data.shared.settings.write().await.owner_user_id = Some(42);
        data.shared.core.lock().await.sessions.clear();
        let absent = rig.invoke(&data, "resume", Some(("cwd", ""))).await;
        assert_eq!(absent["choices"], json!([]));
        let ids = rig.invoke(&data, "resume", Some(("session_id", ""))).await;
        assert_eq!(
            ids["choices"],
            json!([]),
            "no database is an empty suggestion list"
        );
    });
}

#[test]
fn registered_session_autocomplete_reads_scoped_transcripts_and_caps_results_pg() {
    isolated(async {
        let database = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("current-worktree");
        let home = root.path().join("claude-home");
        std::fs::create_dir_all(&cwd).unwrap();
        let directory =
            crate::services::claude_tui::transcript_tail::claude_project_dir_candidates_for_cwd(
                &cwd,
                Some(&home),
            )
            .unwrap()
            .remove(0);
        std::fs::create_dir_all(&directory).unwrap();
        for n in 0..30 {
            let id = uuid::Uuid::from_u128(n + 1);
            std::fs::write(
                directory.join(format!("{id}.jsonl")),
                format!("{{\"type\":\"ai-title\",\"aiTitle\":\"작업-{n:02}\"}}\n"),
            )
            .unwrap();
        }
        let data = Data {
            shared: crate::services::discord::make_shared_data_for_tests_with_storage(Some(
                pool.clone(),
            )),
            token: "fixture-token".into(),
            provider: ProviderKind::Claude,
        };
        data.shared.settings.write().await.owner_user_id = Some(42);
        session(&data, discord_mock::CHANNEL_ID, cwd.to_str().unwrap()).await;
        data.shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&ChannelId::new(discord_mock::CHANNEL_ID))
            .unwrap()
            .channel_name = Some("example-cc".into());
        let key = crate::services::discord::adk_session::build_adk_session_key(
            &data.shared,
            ChannelId::new(discord_mock::CHANNEL_ID),
            &data.provider,
            None,
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO sessions (session_key, provider, status, channel_id, cwd) VALUES ($1, 'claude', 'disconnected', $2, $3)")
            .bind(key).bind(discord_mock::CHANNEL_ID.to_string()).bind(cwd.to_str().unwrap())
            .execute(&pool).await.unwrap();
        let rig = Rig::new().await;
        crate::services::session_resume::catalog::scope_claude_home_for_tests(home, async {
            let prefix = rig
                .invoke(&data, "resume", Some(("session_id", "작업-0")))
                .await;
            let choices = prefix["choices"].as_array().unwrap();
            assert_eq!(choices.len(), 10);
            assert!(
                choices
                    .iter()
                    .all(|choice| uuid::Uuid::parse_str(choice["value"].as_str().unwrap()).is_ok())
            );
            let all = rig.invoke(&data, "resume", Some(("session_id", ""))).await;
            assert_eq!(all["choices"].as_array().unwrap().len(), 25);
            let empty = rig
                .invoke(&data, "resume", Some(("session_id", "missing")))
                .await;
            assert_eq!(empty["choices"], json!([]));
        })
        .await;
        drop(data);
        pool.close().await;
        database.drop().await;
    });
}

#[test]
fn wrong_provider_autocomplete_answers_empty_without_running_the_handler() {
    isolated(async {
        let rig = Rig::new().await;
        let data = data(ProviderKind::Codex);
        data.shared.settings.write().await.owner_user_id = Some(42);
        let result = rig.invoke(&data, "skill", Some(("skill", ""))).await;
        assert_eq!(result["choices"], json!([]));
    });
}

#[test]
fn autocomplete_busy_cache_returns_empty_before_discord_deadline() {
    isolated(async {
        let rig = Rig::new().await;
        let data = data(ProviderKind::Claude);
        data.shared.settings.write().await.owner_user_id = Some(42);
        let busy = data.shared.skills_cache.write().await;
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(2750),
            rig.invoke(&data, "skill", Some(("skill", ""))),
        )
        .await
        .expect("autocomplete must finish before the 3s interaction deadline");
        assert_eq!(result["choices"], json!([]));
        drop(busy);
        let _busy_settings = data.shared.settings.write().await;
        let guarded = tokio::time::timeout(
            std::time::Duration::from_millis(2750),
            rig.invoke(&data, "skill", Some(("skill", ""))),
        )
        .await
        .expect("the framework channel check must also respect the interaction deadline");
        assert_eq!(guarded["choices"], json!([]));
    });
}

#[test]
fn registered_choices_cover_every_fixed_slash_parameter() {
    let commands = run_bot_build_slash_commands();
    assert_eq!(commands.len(), 24);
    let inspect = commands.iter().find(|c| c.name == "adk").unwrap();
    assert_eq!(inspect.subcommands.len(), 1);
    for (command, parameter, expected) in [
        ("tool-call", "action", vec!["on", "off", "status"]),
        ("allowall", "action", vec!["on", "off", "status"]),
        (
            "effort",
            "level",
            vec!["low", "medium", "high", "xhigh", "max"],
        ),
        ("usage", "period", vec!["ratelimit", "month"]),
        (
            "inspect",
            "view",
            vec!["last", "session", "prompt", "context", "recovery"],
        ),
    ] {
        let registered = commands
            .iter()
            .chain(&inspect.subcommands)
            .find(|c| c.name == command)
            .unwrap();
        let parameter = registered
            .parameters
            .iter()
            .find(|p| p.name == parameter)
            .unwrap();
        let names: Vec<&str> = parameter
            .choices
            .iter()
            .map(|choice| choice.name.as_ref())
            .collect();
        assert_eq!(names, expected, "{command}");
        assert!(names.len() <= 25 && names.iter().all(|name| name.chars().count() <= 100));
    }
}
