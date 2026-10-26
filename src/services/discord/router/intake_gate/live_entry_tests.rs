#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use poise::serenity_prelude as serenity;
use serde_json::{Value, json};

use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{Data, mailbox_snapshot, router};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};

#[derive(Clone, Copy, Debug)]
enum Branch {
    ThreadGuard,
    DispatchGuard,
    BusyActiveTurn,
    ReconcileGate,
    DrainMode,
    IdleBacklog,
}

impl Branch {
    const ALL: [Self; 6] = [
        Self::ThreadGuard,
        Self::DispatchGuard,
        Self::BusyActiveTurn,
        Self::ReconcileGate,
        Self::DrainMode,
        Self::IdleBacklog,
    ];

    fn channel(self) -> u64 {
        940_632_600_000_000 + self as u64 * 10
    }
}

#[derive(Clone)]
struct HttpState {
    calls: Arc<Mutex<Vec<(Method, String)>>>,
}

async fn discord_http(State(state): State<HttpState>, request: Request<Body>) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    state
        .calls
        .lock()
        .unwrap()
        .push((method.clone(), path.clone()));
    let channel = path
        .strip_prefix("/api/v10/channels/")
        .and_then(|rest| rest.split('/').next())
        .and_then(|id| id.parse::<u64>().ok());
    if method == Method::GET && channel.is_some_and(|id| path.ends_with(&id.to_string())) {
        return axum::Json(json!({
            "id": channel.unwrap().to_string(), "type": 0,
            "guild_id": "96326000999", "name": "c2b-live-cc",
            "position": 0, "permission_overwrites": [], "nsfw": false,
            "parent_id": null
        }))
        .into_response();
    }
    if method == Method::GET && path.ends_with("/messages") {
        return axum::Json(json!([])).into_response();
    }
    if method == Method::PUT || method == Method::DELETE {
        return StatusCode::NO_CONTENT.into_response();
    }
    if method == Method::POST && path.ends_with("/messages") {
        let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        if matches!(body["content"].as_str(), Some("..." | "⠋ 계속 처리 중")) {
            return (
                StatusCode::FORBIDDEN,
                axum::Json(json!({"message": "fixture refuses provider start", "code": 50001})),
            )
                .into_response();
        }
        let mut reply = discord_mock::user_message(
            940_632_699_000_001,
            body["content"].as_str().unwrap_or_default(),
        );
        reply.channel_id = serenity::ChannelId::new(channel.unwrap());
        reply.guild_id = Some(serenity::GuildId::new(96_326_000_999));
        reply.author.bot = true;
        return axum::Json(serde_json::to_value(reply).unwrap()).into_response();
    }
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({"message": "unexpected fixture endpoint", "code": 0})),
    )
        .into_response()
}

struct Transport {
    ctx: serenity::Context,
    calls: Arc<Mutex<Vec<(Method, String)>>>,
    http: tokio::task::JoinHandle<()>,
    gateway: tokio::task::JoinHandle<()>,
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.http.abort();
        self.gateway.abort();
    }
}

impl Transport {
    async fn start() -> Self {
        let (_, gateway_url, gateway) =
            discord_mock::start(discord_mock::DiscordMockState::new()).await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .fallback(discord_http)
            .with_state(HttpState {
                calls: calls.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let ctx = discord_mock::serenity_context(proxy, gateway_url).await;
        Self {
            ctx,
            calls,
            http,
            gateway,
        }
    }
}

struct Outcome {
    held: bool,
    original: Value,
    after: Value,
    queued: Vec<Intervention>,
    durable: Vec<Intervention>,
    checkpoint: Option<u64>,
    http: Vec<(Method, String)>,
    message: serenity::MessageId,
}

async fn invoke(branch: Branch, root: &Path) -> Outcome {
    let channel = serenity::ChannelId::new(branch.channel());
    let thread = serenity::ChannelId::new(branch.channel() + 1);
    let message_id = serenity::MessageId::new(branch.channel() + 4);
    assert!(crate::services::discord::reaction_lifecycle::is_real_discord_message_id(message_id));
    let mut message = discord_mock::user_message(message_id.get(), "  original live input  ");
    message.channel_id = channel;
    message.guild_id = Some(serenity::GuildId::new(96_326_000_999));
    if matches!(branch, Branch::ThreadGuard | Branch::DispatchGuard) {
        message.content = "DISPATCH: original live input".into();
    }
    message.author.bot = matches!(branch, Branch::ThreadGuard);
    let shared = crate::services::discord::make_shared_data_for_tests();
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(message.author.id.get());
        settings.allow_all_users = true;
        settings.allowed_channel_ids = vec![channel.get()];
        if message.author.bot {
            settings.allowed_bot_ids.push(message.author.id.get());
        }
    }
    crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "c2b-live-cc")
        .await;
    {
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.current_path = Some(root.join("workspace").display().to_string());
        session.pending_uploads = vec!["retained-upload".into()];
    }
    match branch {
        Branch::ThreadGuard | Branch::DispatchGuard | Branch::BusyActiveTurn => {
            let active = if matches!(branch, Branch::ThreadGuard) {
                thread
            } else {
                channel
            };
            assert!(
                shared
                    .mailbox(active)
                    .try_start_turn(
                        Arc::new(CancelToken::new()),
                        message.author.id,
                        serenity::MessageId::new(active.get() + 2)
                    )
                    .await,
                "fixture installs a real active-turn claim"
            );
            if matches!(branch, Branch::ThreadGuard) {
                shared.dispatch.thread_parents.insert(channel, thread);
            }
        }
        Branch::ReconcileGate => shared
            .restart
            .reconcile_done
            .store(false, Ordering::Relaxed),
        Branch::DrainMode => shared
            .restart
            .restart_pending
            .store(true, Ordering::Relaxed),
        Branch::IdleBacklog => {
            let older = serenity::MessageId::new(channel.get() + 3);
            let backlog = Intervention {
                author_id: serenity::UserId::new(message.author.id.get() + 1),
                author_is_bot: false,
                message_id: older,
                queued_generation: 0,
                source_message_ids: vec![older],
                source_message_queued_generations: Vec::new(),
                source_text_segments: Vec::new(),
                text: "older backlog".into(),
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            };
            crate::services::discord::mailbox_enqueue_intervention(
                &shared,
                &ProviderKind::Claude,
                channel,
                backlog,
            )
            .await;
            for id in [older, message_id] {
                for _ in
                    0..crate::services::discord::busy_followup_retry_store::MAX_BUSY_RETRY_COUNT
                {
                    crate::services::discord::busy_followup_retry_store::record_busy_retry(
                        &ProviderKind::Claude,
                        channel.get(),
                        id.get(),
                        channel.get() + 8,
                    )
                    .unwrap();
                }
            }
            assert!(
                crate::services::discord::mailbox_has_pending_soft_queue(
                    &shared,
                    &ProviderKind::Claude,
                    channel
                )
                .await
                .has_pending,
                "fixture retains the idle Soft backlog"
            );
        }
    }
    let mut transport = Transport::start().await;
    assert!(
        shared
            .http
            .cached_serenity_ctx
            .set(transport.ctx.clone())
            .is_ok()
    );
    shared.turn_view_reconciler.enable_test_http_reactions();
    assert_eq!(
        crate::services::discord::resolve_runtime_channel_binding_status(
            &transport.ctx.http,
            channel
        )
        .await,
        crate::services::discord::RuntimeChannelBindingStatus::Owned,
        "fixture has a real AgentDef-owned GuildText binding"
    );
    assert!(
        crate::services::discord::provider_handles_channel(
            &transport.ctx,
            &ProviderKind::Claude,
            &shared.settings.read().await.clone(),
            channel
        )
        .await,
        "fixture passes current provider routing"
    );
    assert!(
        crate::services::discord::check_auth(
            message.author.id,
            &message.author.name,
            &shared,
            "test-token"
        )
        .await,
        "fixture passes fresh user authorization"
    );
    transport.calls.lock().unwrap().clear();
    let original = serde_json::to_value(&message).unwrap();
    let mut voice_config = crate::voice::VoiceConfig::default();
    voice_config.audio.recordings_dir = root.join("recordings");
    let data = Arc::new(Data {
        shared: shared.clone(),
        token: "test-token".into(),
        provider: ProviderKind::Claude,
        voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
        voice_config,
    });
    let ctx = transport.ctx.clone();
    let event = serenity::FullEvent::Message {
        new_message: message,
    };
    let mut called = tokio::spawn(async move {
        let result = router::handle_event(&ctx, &event, &data).await;
        (result, event)
    });
    let completed = tokio::time::timeout(Duration::from_secs(10), &mut called).await;
    if completed.is_err() {
        called.abort();
        let _ = called.await;
    }
    let (result, event) = completed
        .expect("fixture: native FullEvent completed; timeout is not a live-gate mutant kill")
        .expect("fixture: native handler task remained healthy");
    let serenity::FullEvent::Message { new_message } = event else {
        unreachable!()
    };
    let after = serde_json::to_value(new_message).unwrap();
    let snapshot = mailbox_snapshot(&shared, channel).await;
    let queued = snapshot.intervention_queue;
    let (durable, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
        &ProviderKind::Claude,
        &shared.token_hash,
        channel,
    );
    let checkpoint = shared.last_message_ids.get(&channel).map(|entry| *entry);
    let http = transport.calls.lock().unwrap().clone();
    shared.mailboxes.remove_fixture_for_test(channel);
    shared.mailboxes.remove_fixture_for_test(thread);
    transport.http.abort();
    transport.gateway.abort();
    let _ = (&mut transport.http).await;
    let _ = (&mut transport.gateway).await;
    Outcome {
        held: result.is_err(),
        original,
        after,
        queued,
        durable,
        checkpoint,
        http,
        message: message_id,
    }
}

fn run(test: &str, branches: &[Branch]) {
    let qualified = format!("{}::{test}", module_path!().split_once("::").unwrap().1);
    if !crate::services::discord::admin_host_guard::tests::api_child(&qualified) {
        return;
    }
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/c2b-live-gate");
    std::fs::create_dir_all(&base).unwrap();
    let scratch = tempfile::tempdir_in(base).unwrap();
    let root = std::fs::canonicalize(scratch.path()).unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        &root,
    );
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    for name in ["tmux", "claude", "codex"] {
        let binary = bin.join(name);
        std::fs::write(&binary, "#!/bin/sh\nprintf '%s %s\\n' \"$0\" \"$*\" >> \"$ADK_C2B_TEST_PROCESS_LOG\"\nexit 97\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let _process_log = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "ADK_C2B_TEST_PROCESS_LOG",
        &root.join("process-calls"),
    );
    let _path = crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(&bin);
    let _provider = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CLAUDE_PATH",
        &bin.join("claude"),
    );
    let config_path = crate::runtime_layout::config_file_path(&root);
    assert_eq!(crate::config::runtime_root().unwrap(), root);
    assert!(config_path.starts_with(&root));
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    assert!(
        std::fs::canonicalize(config_path.parent().unwrap())
            .unwrap()
            .starts_with(&root)
    );
    std::fs::create_dir(root.join("workspace")).unwrap();
    let agents: Vec<_> = Branch::ALL.into_iter().map(|branch| json!({
        "id": format!("c2b-live-{}", branch as u64), "name": format!("C2b fixture {}", branch as u64), "voice_enabled": false,
        "provider": "claude", "channels": {"claude": {"id": branch.channel().to_string(), "name": "c2b-live-cc", "workspace": root.join("workspace"), "runtime": "tui"}}
    })).collect();
    std::fs::write(
        &config_path,
        serde_json::to_vec(
            &json!({"server": {}, "data": {"dir": root.join("data")}, "agents": agents}),
        )
        .unwrap(),
    )
    .unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config_path,
    );
    assert_eq!(std::fs::canonicalize(&config_path).unwrap(), config_path);
    assert_eq!(crate::config::resolved_config_path(), config_path);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let outcomes = runtime.block_on(async {
        let mut outcomes = Vec::new();
        for &branch in branches {
            outcomes.push((branch, Box::pin(invoke(branch, &root)).await));
        }
        outcomes
    });
    drop(runtime);
    for (branch, outcome) in outcomes {
        assert_eq!(
            outcome.original, outcome.after,
            "{branch:?}: the whole gateway Message stays original"
        );
        assert!(
            !outcome.held,
            "{branch:?}: actual intake executes the existing queue branch"
        );
        for rows in [&outcome.queued, &outcome.durable] {
            assert!(
                rows.iter().any(|row| row.message_id == outcome.message
                    || row.source_message_ids.contains(&outcome.message)),
                "{branch:?}: actual Legacy input is present in mailbox and durable queue"
            );
        }
        assert_eq!(
            outcome.checkpoint,
            Some(outcome.message.get()),
            "{branch:?}: Legacy queue commit advances the actual checkpoint"
        );
        assert!(
            outcome
                .http
                .iter()
                .any(|(method, path)| *method == Method::PUT && path.contains("/reactions/")),
            "{branch:?}: Legacy queue commit emits the pending reaction"
        );
    }
}

#[test]
fn actual_live_entry_keeps_all_six_queue_branches() {
    run(
        "actual_live_entry_keeps_all_six_queue_branches",
        &Branch::ALL,
    );
}
