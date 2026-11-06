use super::*;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[derive(Default)]
struct Network {
    delay: Duration,
    thread: bool,
    failed: bool,
    payload: bool,
    age_ms: u64,
}

struct Rig {
    ctx: serenity::Context,
    replies: Arc<std::sync::Mutex<Vec<Value>>>,
    writes: Arc<std::sync::Mutex<Vec<String>>>,
    messages: Arc<std::sync::Mutex<Vec<Value>>>,
    edits: Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    mock: discord_mock::DiscordMockState,
    url: String,
    payload: Value,
    age_ms: u64,
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
        Self::network(name, Network::default()).await
    }

    async fn network(name: &str, network: Network) -> Self {
        let mock = discord_mock::DiscordMockState::new();
        let parent = discord_mock::CHANNEL_ID + 10;
        if network.thread {
            mock.add_thread(discord_mock::CHANNEL_ID, parent);
            mock.name_channel(parent, name);
        } else {
            mock.name_channel(discord_mock::CHANNEL_ID, name);
        }
        let (backend, gateway, server) = discord_mock::start(mock.clone()).await;
        let replies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = replies.clone();
        let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let extra_writes = writes.clone();
        let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
        let extra_messages = messages.clone();
        let edits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let extra_edits = edits.clone();
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
                let extra_writes = extra_writes.clone();
                let extra_messages = extra_messages.clone();
                let extra_edits = extra_edits.clone();
                async move {
                    let method = request.method().clone();
                    let uri = request.uri().to_string();
                    if method != axum::http::Method::GET {
                        extra_writes
                            .lock()
                            .unwrap()
                            .push(format!("{method} {}", request.uri()));
                    }
                    if request.method() == axum::http::Method::GET {
                        tokio::time::sleep(network.delay).await;
                        if network.failed {
                            return (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::body::Bytes::from_static(b"{}"),
                            );
                        }
                    }
                    let url = format!("{backend}{}", request.uri());
                    let headers = request.headers().clone();
                    let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
                        .await
                        .unwrap();
                    if method != axum::http::Method::GET
                        && let Ok(message) = serde_json::from_slice::<Value>(&body)
                    {
                        extra_messages.lock().unwrap().push(message.clone());
                        if method == axum::http::Method::PATCH {
                            extra_edits.lock().unwrap().push((uri, message.clone()));
                        }
                        if request_uri_is_webhook(&url) {
                            return (
                                axum::http::StatusCode::OK,
                                axum::body::Bytes::from(
                                    discord_mock::history_message_json(
                                        7678003,
                                        message["content"].as_str().unwrap_or_default(),
                                        true,
                                    )
                                    .to_string(),
                                ),
                            );
                        }
                    }
                    let response = client
                        .request(method, url)
                        .headers(headers)
                        .body(body)
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
        ctx.http
            .set_application_id(serenity::ApplicationId::new(discord_mock::BOT_ID));
        let payload = if network.payload {
            json!({"id": discord_mock::CHANNEL_ID.to_string(), "name": name,
                "type": if network.thread {11} else {0},
                "parent_id": network.thread.then(|| parent.to_string())})
        } else {
            Value::Null
        };
        Self {
            ctx,
            replies,
            writes,
            messages,
            edits,
            mock,
            url: format!("http://{address}"),
            payload,
            age_ms: network.age_ms,
            servers: vec![server, frontend],
        }
    }

    async fn invoke(&self, data: &Data, name: &str, focused: Option<(&str, &str)>) -> Value {
        self.invoke_inner(data, name, focused, focused.is_some())
            .await
    }

    #[cfg(unix)]
    async fn invoke_cancel(&self, data: &Data, id: Option<&str>) -> Value {
        let _ = data.shared.http.cached_serenity_ctx.set(self.ctx.clone());
        self.invoke_inner(
            data,
            "cancel-queued",
            id.map(|id| ("message_id", id)),
            false,
        )
        .await
    }

    async fn invoke_inner(
        &self,
        data: &Data,
        name: &str,
        focused: Option<(&str, &str)>,
        autocomplete: bool,
    ) -> Value {
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
                {"type": 3, "name": parameter, "value": value, "focused": autocomplete}
            ])
        });
        if let Some(child) = child_name {
            raw_options = json!([{"type": 1, "name": child, "options": raw_options}]);
        }
        let mut user = serenity::User::default();
        user.id = serenity::UserId::new(42);
        user.name = "slash-fixture".into();
        let millis = chrono::Utc::now().timestamp_millis() as u64 - self.age_ms;
        let interaction_id = (millis - 1_420_070_400_000) << 22;
        let interaction: serenity::CommandInteraction = serde_json::from_value(json!({
            "id": interaction_id.to_string(), "application_id": discord_mock::BOT_ID.to_string(),
            "data": {"id": "7678002", "name": root_name, "type": 1, "resolved": {}, "options": raw_options},
            "guild_id": null, "channel": self.payload, "channel_id": discord_mock::CHANNEL_ID.to_string(),
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
            interaction_type: if autocomplete {
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
        if autocomplete {
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
            assert!(
                self.writes.lock().unwrap().is_empty(),
                "unexpected non-callback HTTP write"
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
            let mut tree = Vec::new();
            let result = poise::dispatch::dispatch_interaction(
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
            let allowed = result.is_ok();
            assert!(
                name == "cancel-queued" || self.writes.lock().unwrap().is_empty(),
                "unexpected non-callback HTTP write"
            );
            json!({"allowed": allowed, "replies": *self.replies.lock().unwrap(), "messages": *self.messages.lock().unwrap()})
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
        let allowed = rig.invoke(&data(ProviderKind::Claude), "pwd", None).await;
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
    isolated(session_catalog_fixture(Network::default(), false));
}

#[test]
fn slow_session_discovery_returns_partial_results_before_the_shared_deadline_pg() {
    isolated(session_catalog_fixture(
        Network {
            delay: Duration::from_millis(400),
            ..Default::default()
        },
        true,
    ));
}

async fn session_catalog_fixture(network: Network, slow: bool) {
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
    let rig = Rig::network("example-cc", network).await;
    let _delay = slow.then(|| {
        crate::services::session_resume::catalog::delay_previews_for_tests(
            directory.clone(),
            Duration::from_millis(90),
        )
    });
    crate::services::session_resume::catalog::scope_claude_home_for_tests(home, async {
        if slow {
            let result = tokio::time::timeout(
                Duration::from_millis(2750),
                rig.invoke(&data, "resume", Some(("session_id", ""))),
            )
            .await
            .unwrap();
            let choices = result["choices"].as_array().unwrap();
            assert!(
                !choices.is_empty() && choices.len() < 25,
                "expected partial results: {choices:?}"
            );
            return;
        }
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
        for n in 0..30 {
            let id = uuid::Uuid::from_u128(n + 1);
            std::fs::write(
                directory.join(format!("{id}.jsonl")),
                "{\"type\":\"ai-title\",\"aiTitle\":\"fed details\"}\n",
            )
            .unwrap();
        }
        let target = "fed00000-0000-0000-0000-000000000001";
        let transcript = directory.join(format!("{target}.jsonl"));
        std::fs::write(
            &transcript,
            "{\"type\":\"ai-title\",\"aiTitle\":\"older target\"}\n",
        )
        .unwrap();
        filetime::set_file_mtime(&transcript, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        let preferred = rig
            .invoke(&data, "resume", Some(("session_id", "fed")))
            .await;
        assert_eq!(
            preferred["choices"][0]["value"], target,
            "an older matching ID must outrank 30 matching titles before truncation"
        );
        assert_eq!(preferred["choices"].as_array().unwrap().len(), 25);
    })
    .await;
    drop(data);
    pool.close().await;
    database.drop().await;
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

#[test]
fn slow_thread_guidance_keeps_provider_name_within_the_shared_deadline() {
    isolated(async {
        let rig = Rig::network(
            "example-cc",
            Network {
                delay: Duration::from_millis(200),
                thread: true,
                payload: true,
                age_ms: 1800,
                ..Default::default()
            },
        )
        .await;
        let result = tokio::time::timeout(
            Duration::from_millis(2750),
            rig.invoke(&data(ProviderKind::Codex), "cancel-queued", None),
        )
        .await
        .unwrap();
        let content = result["replies"][0]["data"]["content"].as_str().unwrap();
        assert!(content.contains("claude"), "{content}");
    });
}

#[test]
fn slow_thread_autocomplete_uses_remaining_time_instead_of_a_fixed_guard_cutoff() {
    isolated(async {
        let rig = Rig::network(
            "example-cc",
            Network {
                delay: Duration::from_millis(300),
                thread: true,
                ..Default::default()
            },
        )
        .await;
        let data = data(ProviderKind::Claude);
        data.shared.settings.write().await.owner_user_id = Some(42);
        let result = tokio::time::timeout(
            Duration::from_millis(2750),
            rig.invoke(&data, "skill", Some(("skill", "health"))),
        )
        .await
        .unwrap();
        assert_eq!(result["choices"][0]["value"], "health");
    });
}

#[test]
fn registered_autocomplete_prioritizes_value_prefix_then_value_contains_then_label() {
    isolated(async {
        let rig = Rig::new().await;
        let data = data(ProviderKind::Claude);
        data.shared.settings.write().await.owner_user_id = Some(42);
        let mut skills: Vec<_> = (0..30)
            .map(|n| (format!("a-{n:02}"), "read this".into()))
            .collect();
        skills.extend([
            ("x-relay".into(), "value contains".into()),
            ("relay-scan".into(), "prefix".into()),
            ("restart-scan".into(), "prefix".into()),
        ]);
        *data.shared.skills_cache.write().await = skills;
        let result = rig.invoke(&data, "skill", Some(("skill", "re"))).await;
        let choices = result["choices"].as_array().unwrap();
        assert_eq!(choices.len(), 25);
        let values: Vec<_> = choices
            .iter()
            .map(|c| c["value"].as_str().unwrap())
            .collect();
        assert_eq!(
            &values[..6],
            &[
                "relay-scan",
                "restart-scan",
                "x-relay",
                "pwd",
                "stop",
                "a-00"
            ]
        );
    });
}

#[test]
fn transient_channel_lookup_failure_asks_to_retry_instead_of_switching_bots() {
    isolated(async {
        let rig = Rig::network(
            "example-cc",
            Network {
                failed: true,
                ..Default::default()
            },
        )
        .await;
        let result = rig.invoke(&data(ProviderKind::Claude), "pwd", None).await;
        assert_eq!(result["allowed"], false);
        let replies = result["replies"].as_array().unwrap();
        assert_eq!(replies.len(), 1);
        let content = replies[0]["data"]["content"].as_str().unwrap();
        assert!(content.contains("확인하지 못했어요"), "{content}");
        assert!(!content.contains("선택해"), "{content}");
        assert_eq!(replies[0]["data"]["flags"], 64);
    });
}

#[test]
fn rig_records_non_callback_http_writes() {
    isolated(async {
        let rig = Rig::new().await;
        assert_eq!(
            rig.ctx.http.application_id(),
            Some(serenity::ApplicationId::new(discord_mock::BOT_ID)),
            "follow-up writes must reach HTTP with a configured application ID"
        );
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("{}/api/v10/webhooks/123/fixture-token", rig.url))
            .json(&json!({"content": "write probe"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            rig.writes.lock().unwrap().len(),
            1,
            "follow-up POST must be visible"
        );
    });
}

fn registered_census(
    commands: &[poise::Command<Data, Error>],
) -> std::collections::BTreeSet<(String, String)> {
    fn visit(
        command: &Value,
        path: String,
        census: &mut std::collections::BTreeSet<(String, String)>,
    ) {
        let options = command["options"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        if options.is_empty() {
            census.insert((path.clone(), "없음".into()));
        }
        if options
            .iter()
            .any(|p| matches!(p["type"].as_u64(), Some(1 | 2)))
        {
            census.insert((path.clone(), "하위 명령".into()));
        }
        for parameter in options {
            let name = parameter["name"].as_str().unwrap();
            if matches!(parameter["type"].as_u64(), Some(1 | 2)) {
                visit(parameter, format!("{path} {name}"), census);
                continue;
            }
            census.insert((path.clone(), name.into()));
            if parameter["type"] == 3 {
                let explicit_exception = matches!((path.as_str(), name), ("/skill", "args"));
                assert!(
                    parameter["autocomplete"] == true
                        || parameter["choices"]
                            .as_array()
                            .is_some_and(|c| !c.is_empty())
                        || explicit_exception,
                    "uncovered STRING parameter: {path}.{name}"
                );
            }
        }
    }
    let mut census = std::collections::BTreeSet::new();
    for command in commands {
        let registered = serde_json::to_value(command.create_as_slash_command().unwrap()).unwrap();
        visit(&registered, format!("/{}", command.name), &mut census);
    }
    census
}

#[test]
fn registered_slash_census_matches_documented_parameters_and_string_completion() {
    let document = include_str!("../../../../../docs/discord-slash-autocomplete.md");
    assert_census_matches(&run_bot_build_slash_commands(), document);
}

fn assert_census_matches(commands: &[poise::Command<Data, Error>], document: &str) {
    assert_eq!(
        registered_census(commands),
        documented_census(document),
        "slash census drift"
    );
}

fn documented_census(document: &str) -> std::collections::BTreeSet<(String, String)> {
    document
        .lines()
        .filter_map(|line| {
            let cells: Vec<_> = line.split('|').map(str::trim).collect();
            let command = cells.get(1)?.trim_matches('`');
            if !command.starts_with('/') {
                return None;
            }
            Some((
                command.to_owned(),
                cells.get(2)?.trim_matches('`').to_owned(),
            ))
        })
        .collect()
}

#[test]
fn census_guard_rejects_a_documented_parameter_omission() {
    let document = include_str!("../../../../../docs/discord-slash-autocomplete.md");
    let omitted = document
        .lines()
        .filter(|line| !line.starts_with("| `/resume` | `cwd` |"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| assert_census_matches(
            &run_bot_build_slash_commands(),
            &omitted
        )))
        .is_err(),
        "census guard accepted a missing documented parameter"
    );
}

#[test]
fn census_guard_rejects_an_uncovered_registered_string_parameter() {
    let mut commands = run_bot_build_slash_commands();
    commands
        .iter_mut()
        .find(|c| c.name == "resume")
        .unwrap()
        .parameters
        .iter_mut()
        .find(|p| p.name == "cwd")
        .unwrap()
        .autocomplete_callback = None;
    let document = include_str!("../../../../../docs/discord-slash-autocomplete.md");
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| assert_census_matches(
            &commands, document
        )))
        .is_err(),
        "census guard accepted a STRING without completion"
    );
}

#[test]
fn one_deadline_is_not_renewed_between_guard_and_completion() {
    isolated(async {
        let rig = Rig::network(
            "example-cc",
            Network {
                delay: Duration::from_millis(300),
                thread: true,
                ..Default::default()
            },
        )
        .await;
        let data = data(ProviderKind::Claude);
        data.shared.settings.write().await.owner_user_id = Some(42);
        let _busy = data.shared.skills_cache.write().await;
        let result = tokio::time::timeout(
            Duration::from_millis(2750),
            rig.invoke(&data, "skill", Some(("skill", ""))),
        )
        .await
        .expect("guard and completion must consume the same deadline");
        assert_eq!(result["choices"], json!([]));
    });
}

#[test]
fn interaction_channel_payload_routes_without_a_rest_lookup() {
    isolated(async {
        let rig = Rig::network(
            "example-cc",
            Network {
                failed: true,
                payload: true,
                ..Default::default()
            },
        )
        .await;
        let data = data(ProviderKind::Claude);
        data.shared.settings.write().await.owner_user_id = Some(42);
        let result = rig.invoke(&data, "skill", Some(("skill", "health"))).await;
        assert_eq!(result["choices"][0]["value"], "health");
    });
}

#[test]
fn rejected_command_still_replies_when_no_other_provider_name_is_available() {
    isolated(async {
        let rig = Rig::new().await;
        let data = data(ProviderKind::Claude);
        data.shared.settings.write().await.allowed_channel_ids = vec![discord_mock::CHANNEL_ID + 1];
        let result = rig.invoke(&data, "cancel-queued", None).await;
        assert_eq!(result["allowed"], false);
        let replies = result["replies"].as_array().unwrap();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["data"]["flags"], 64);
        assert!(
            replies[0]["data"]["content"]
                .as_str()
                .unwrap()
                .contains("담당 봇")
        );
    });
}

fn request_uri_is_webhook(url: &str) -> bool {
    url.contains("/webhooks/")
}

#[cfg(unix)]
mod cancel_queued_tests;
