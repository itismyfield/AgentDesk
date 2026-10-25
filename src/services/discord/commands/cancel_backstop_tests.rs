use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use poise::serenity_prelude as serenity;
use serde_json::{Value, json};

use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{Data, Error, inflight};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};

#[derive(Clone)]
struct ActionProxyState {
    backend: String,
    replies: Arc<Mutex<Vec<Value>>>,
    client: reqwest::Client,
}

async fn acknowledge(
    State(state): State<ActionProxyState>,
    axum::Json(body): axum::Json<Value>,
) -> StatusCode {
    state.replies.lock().expect("slash responses").push(body);
    StatusCode::NO_CONTENT
}

async fn followup(
    State(state): State<ActionProxyState>,
    axum::Json(body): axum::Json<Value>,
) -> axum::Json<Value> {
    state
        .replies
        .lock()
        .expect("slash responses")
        .push(json!({"type":"followup", "data":body}));
    let response = state
        .client
        .post(format!(
            "{}/api/v10/channels/{}/messages",
            state.backend,
            discord_mock::CHANNEL_ID
        ))
        .json(&body)
        .send()
        .await
        .expect("followup response");
    axum::Json(response.json().await.expect("followup message"))
}

async fn relay(State(state): State<ActionProxyState>, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let suffix = parts.uri.path_and_query().expect("mock request path");
    let body = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .expect("mock request body");
    let response = state
        .client
        .request(parts.method, format!("{}{suffix}", state.backend))
        .headers(parts.headers)
        .body(body)
        .send()
        .await
        .expect("loopback Discord request");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.bytes().await.expect("loopback Discord response");
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Loopback Discord whose interaction callbacks and followups are recorded.
pub(super) struct ActionTransport {
    pub(super) ctx: serenity::Context,
    replies: Arc<Mutex<Vec<Value>>>,
    backend: tokio::task::JoinHandle<()>,
    frontend: tokio::task::JoinHandle<()>,
}

impl Drop for ActionTransport {
    fn drop(&mut self) {
        self.frontend.abort();
        self.backend.abort();
    }
}

impl ActionTransport {
    pub(super) async fn new(mock: discord_mock::DiscordMockState) -> Self {
        let (backend_url, gateway, backend) = discord_mock::start(mock).await;
        let replies = Arc::new(Mutex::new(Vec::new()));
        let state = ActionProxyState {
            backend: backend_url,
            replies: replies.clone(),
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("loopback proxy client"),
        };
        let app = axum::Router::new()
            .route(
                "/api/v10/interactions/{id}/{token}/callback",
                post(acknowledge),
            )
            .route("/api/v10/webhooks/{application}/{token}", post(followup))
            .fallback(relay)
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind slash proxy");
        let address = listener.local_addr().expect("slash proxy address");
        let frontend = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve slash proxy");
        });
        let ctx = discord_mock::serenity_context(format!("http://{address}"), gateway).await;
        Self {
            ctx,
            replies,
            backend,
            frontend,
        }
    }
}

async fn invoke_real_stop_action(
    ctx: &serenity::Context,
    data: &Data,
    channel: serenity::ChannelId,
    author: serenity::UserId,
) {
    let command = super::control::cmd_stop();
    assert!(
        invoke_real_slash_action(ctx, data, channel, author, command, "stop").await,
        "the actual stop action must acknowledge the invocation"
    );
}

/// Runs `command`'s production slash action for one interaction; whether it answered it.
pub(super) async fn invoke_real_slash_action(
    ctx: &serenity::Context,
    data: &Data,
    channel: serenity::ChannelId,
    author: serenity::UserId,
    command: poise::Command<Data, Error>,
    name: &str,
) -> bool {
    invoke_real_slash_action_with_options(ctx, data, channel, author, command, name, json!([]))
        .await
}

/// [`invoke_real_slash_action`] with the interaction's raw `options` payload.
pub(super) async fn invoke_real_slash_action_with_options(
    ctx: &serenity::Context,
    data: &Data,
    channel: serenity::ChannelId,
    author: serenity::UserId,
    command: poise::Command<Data, Error>,
    name: &str,
    raw_options: Value,
) -> bool {
    ctx.http.set_application_id(serenity::ApplicationId::new(
        ctx.cache.current_user().id.get(),
    ));
    let options = poise::FrameworkOptions::<Data, Error>::default();
    let (manager, _) =
        serenity::gateway::ShardManager::new(serenity::gateway::ShardManagerOptions {
            data: ctx.data.clone(),
            event_handlers: vec![],
            raw_event_handlers: vec![],
            framework: Arc::new(std::sync::OnceLock::new()),
            shard_index: 0,
            shard_init: 0,
            shard_total: 1,
            voice_manager: None,
            ws_url: Arc::new(tokio::sync::Mutex::new("ws://127.0.0.1:1".into())),
            cache: ctx.cache.clone(),
            http: ctx.http.clone(),
            intents: serenity::GatewayIntents::empty(),
            presence: None,
        });
    let mut user = serenity::User::default();
    user.id = author;
    user.name = "cancel-command-action".into();
    let interaction: serenity::CommandInteraction = serde_json::from_value(json!({
        "id": "60163003",
        "application_id": ctx.cache.current_user().id.get().to_string(),
        "data": {"id": "60163004", "name": name, "type": 1, "resolved": {}, "options": raw_options},
        "guild_id": null,
        "channel": null,
        "channel_id": channel.get().to_string(),
        "member": null,
        "user": user,
        "token": "cancel-action-test-token",
        "version": 1,
        "app_permissions": null,
        "locale": "en-US",
        "guild_locale": null,
        "entitlements": [],
        "context": null,
        "attachment_size_limit": 8388608
    }))
    .expect("real command interaction fixture");
    let args = interaction.data.options();
    let has_sent_initial_response = AtomicBool::new(false);
    let invocation_data: tokio::sync::Mutex<Box<dyn Any + Send + Sync>> =
        tokio::sync::Mutex::new(Box::new(()));
    let app = poise::ApplicationContext {
        interaction: &interaction,
        interaction_type: poise::CommandInteractionType::Command,
        args: &args,
        has_sent_initial_response: &has_sent_initial_response,
        framework: poise::FrameworkContext {
            serenity_context: ctx,
            options: &options,
            user_data: data,
            shard_manager: &manager,
        },
        command_tree: &[&command],
        invocation_data: &invocation_data,
        __non_exhaustive: (),
    };
    let result = command.slash_action.expect("production slash action")(app).await;
    if let Err(poise::FrameworkError::Command { error, .. }) = &result {
        eprintln!("slash action failed: {error}");
    }
    assert!(
        result.is_ok(),
        "the actual {name} body must finish successfully"
    );
    has_sent_initial_response.load(Ordering::SeqCst)
}

fn missing_tmux(root: &tempfile::TempDir) -> crate::config::TestEnvVarGuard {
    use std::os::unix::fs::PermissionsExt;
    let binary = root.path().join("tmux");
    std::fs::write(&binary, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\necho 'no server running on test socket' >&2\nexit 1\n").expect("mock tmux");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
        .expect("executable mock tmux");
    crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(root.path())
}

fn pending(author: serenity::UserId, id: serenity::MessageId) -> Intervention {
    Intervention {
        author_id: author,
        author_is_bot: false,
        message_id: id,
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: vec![id],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "queued after real slash stop".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

#[test]
fn actual_stop_action_keeps_hold_and_arms_pending_queue_backstop_pg() {
    let root = tempfile::tempdir().expect("command runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _tmux = missing_tmux(&root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("command action runtime");
    runtime.block_on(async {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "cancel_command_action",
            "actual stop action",
        )
        .await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let author = serenity::UserId::new(42);
        let channel = serenity::ChannelId::new(discord_mock::CHANNEL_ID);
        let active_id = serenity::MessageId::new(60163001);
        let queued_id = serenity::MessageId::new(60163002);
        shared.settings.write().await.owner_user_id = Some(author.get());
        let token = Arc::new(CancelToken::new());
        *token.tmux_binding.lock().expect("token binding") = Some(
            crate::services::provider::cancel_token_cleanup::authority::TmuxBinding::NameOnly {
                name: "AgentDesk-claude-command-action".into(),
            },
        );
        assert!(
            shared
                .mailbox(channel)
                .try_start_turn(token.clone(), author, active_id)
                .await
        );
        let row = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            channel.get(),
            None,
            author.get(),
            60163005,
            active_id.get(),
            "command anchor".into(),
            None,
            Some("AgentDesk-claude-command-action".into()),
            None,
            None,
            0,
        );
        inflight::save_inflight_state(&row).expect("hold inflight row");
        let queued =
            crate::services::discord::queue_io::with_post_enqueue_idle_queue_kick_suppressed(
                crate::services::discord::mailbox_enqueue_intervention(
                    &shared,
                    &ProviderKind::Claude,
                    channel,
                    pending(author, queued_id),
                ),
            )
            .await;
        assert!(queued.enqueued);
        assert!(
            !shared.restart.deferred_hook_channels.contains_key(&channel),
            "fixture enqueue must not arm the command's backstop"
        );
        let transport = ActionTransport::new(discord_mock::DiscordMockState::new()).await;
        let voice_config = crate::voice::VoiceConfig::default();
        let data = Data {
            shared: shared.clone(),
            token: "test-token".into(),
            provider: ProviderKind::Claude,
            voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
            voice_config,
        };
        invoke_real_stop_action(&transport.ctx, &data, channel, author).await;
        {
            let replies = transport.replies.lock().expect("command responses");
            assert_eq!(replies.len(), 2);
            assert_eq!(
                replies[0]["type"], 5,
                "defer precedes asynchronous stop confirmation"
            );
            assert_eq!(replies[1]["type"], "followup");
            assert_eq!(replies[1]["data"]["content"], super::STOPPING_RESPONSE);
        }
        assert!(
            token.cancelled.load(Ordering::SeqCst),
            "the command action must publish stop for this token"
        );
        let mailbox = shared.mailbox(channel).snapshot().await;
        assert!(
            mailbox
                .cancel_token
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &token))
        );
        assert_eq!(mailbox.active_user_message_id, Some(active_id));
        assert_eq!(mailbox.intervention_queue.len(), 1);
        assert_eq!(mailbox.intervention_queue[0].message_id, queued_id);
        assert_eq!(
            mailbox.intervention_queue[0].text,
            "queued after real slash stop"
        );
        assert!(
            inflight::inflight_state_file_exists(&ProviderKind::Claude, channel.get()),
            "a live inflight row must hold the original anchor"
        );
        assert!(
            shared.restart.deferred_hook_channels.contains_key(&channel),
            "actual cmd_stop reply.finish must arm a backstop when the queued source remains held"
        );
        drop(data);
        drop(shared);
        drop(transport);
        pool.close().await;
        db.drop().await;
    });
}

fn native_idle_slash_recovery(missing: bool, completed: bool) {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _tmux = missing_tmux(&root);
    if !missing {
        std::fs::write(root.path().join("tmux"), "#!/bin/sh\n[ \"$1\" = -u ] && shift\ncase \"$1\" in capture-pane) printf 'Ready for input (type message + Enter)\\n❯\\n' ;; esac\nexit 0\n").unwrap();
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(Box::pin(async {
    let db=crate::dispatch::test_support::DispatchPostgresTestDb::create("native_slash_recovery","native slash recovery").await;
    let pool=db.connect_and_migrate_with_max_connections(4).await;
    let shared=crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let channel = serenity::ChannelId::new(discord_mock::CHANNEL_ID);
    let author = serenity::UserId::new(42);
    shared.settings.write().await.owner_user_id = Some(author.get());
    let session = "native-idle-slash-recovery";
    assert_eq!(crate::config::runtime_root().as_deref(),Some(root.path()));
    let transcript=std::path::PathBuf::from(crate::services::tmux_common::session_temp_path(session,"jsonl"));
    crate::services::tmux_common::write_tmux_runtime_kind_marker(session,crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui).unwrap();
    let mut content = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n{\"type\":\"assistant\",\"message\":{\"content\":[]}}\n".to_string();
    if !missing {
        content.push_str("{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n");
    }
    std::fs::write(&transcript, content).unwrap();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        session,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: transcript.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let token = Arc::new(CancelToken::new());
    token.bind_claude_tmux_session(session);
    if completed { token.native_stop.completed(); }
    if missing { let failed=token.native_stop.attempt("before_session_death".into(),Arc::new(||false)).unwrap();failed.complete(false); }
    assert!(
        shared
            .mailbox(channel)
            .try_start_turn(token.clone(), author, serenity::MessageId::new(71930001))
            .await
    );
    shared
        .mailbox(channel)
        .replace_queue(
            vec![pending(author, serenity::MessageId::new(71930002))],
            crate::services::discord::queue_persistence_context(
                &shared,
                &ProviderKind::Claude,
                channel,
            ),
        )
        .await;
    let evidence=crate::services::discord::zombie_foreground_release::collect_zombie_foreground_evidence(&ProviderKind::Claude,channel,Some(&token));
    eprintln!("native zombie fixture missing={missing} evidence={evidence:?}");
    assert!(evidence.mailbox_holds_active_turn && !evidence.active_turn_cancelled && !evidence.inflight_state_present);
    assert_eq!(evidence.tui_structurally_idle,Some(true),"the launch-shaped fixture is independently terminal");
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = count.clone();
    let _hook = crate::services::discord::queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
        move |_, _, at, _| {
            let seen = seen.clone();
            Box::pin(async move {
                if at != channel {
                    return None;
                }
                seen.fetch_add(1, Ordering::SeqCst);
                Some(crate::services::discord::IdleQueueKickoffChannelOutcome { started: true })
            })
        },
    ));
    let mut events = shared.turn_completion_events.subscribe();
    let transport = ActionTransport::new(discord_mock::DiscordMockState::new()).await;
    let voice_config = crate::voice::VoiceConfig::default();
    let data = Data {
        shared: shared.clone(),
        token: "test-token".into(),
        provider: ProviderKind::Claude,
        voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
        voice_config,
    };
    invoke_real_stop_action(&transport.ctx, &data, channel, author).await;
    assert!(
        shared
            .mailbox(channel)
            .snapshot()
            .await
            .cancel_token
            .is_none(),
        "idle native slash must release the zombie foreground: cancelled={} replies={:?}", token.cancelled.load(Ordering::Acquire), transport.replies.lock().unwrap()
    );
    assert_eq!(crate::services::discord::zombie_foreground_release::cancel_backstop_test_support::finish_status(channel), Some("finished"));
    // Judged zombie recovery uses main's QueueEligible completion; MailboxReleased is finalizer-only.
    let mut releases = 0;
    while let Ok(event) = events.try_recv() {
        if event.channel_id == channel && event.phase==crate::services::discord::turn_completion_events::TurnCompletionPhase::QueueEligible {releases+=1;}
    }
    assert_eq!(releases, 1, "one completion for the removed zombie token");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while count.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "zombie release schedules one queue kickoff"
    );
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(session);
    drop(data);drop(shared);drop(transport);pool.close().await;db.drop().await;
    }));
}

#[test]
fn preserve_stop_idle_native_slash_recovers_zombie_and_queue_pg() {
    native_idle_slash_recovery(false, false);
}

#[test]
fn preserve_stop_missing_native_slash_recovers_zombie_and_queue_pg() {
    native_idle_slash_recovery(true, false);
}

#[test]
fn preserve_stop_completed_native_zombie_slash_recovers_queue_pg() {
    native_idle_slash_recovery(false, true);
}
