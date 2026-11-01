use super::*;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use serde_json::json;
use std::sync::atomic::Ordering;

fn component_fixture(custom_id: &str) -> serenity::ComponentInteraction {
    let message = discord_mock::user_message(940_487_400_000_041, "a card");
    serde_json::from_value(json!({
        "id":"940487400000042", "application_id":"940487400000003",
        "data":{"custom_id":custom_id,"component_type":2},
        "channel_id":discord_mock::CHANNEL_ID.to_string(),"token":"test-token","version":1,
        "message":message,"locale":"ko","entitlements":[],"attachment_size_limit":10485760,
        "user":message.author
    }))
    .expect("complete component fixture")
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_actual_component_text_and_startup_preserve_results_without_consent_io() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
    let state = discord_mock::DiscordMockState::new();
    let (proxy, gateway, server) = discord_mock::start(state.clone()).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    ctx.http.set_application_id(serenity::ApplicationId::new(
        ctx.cache.current_user().id.get(),
    ));
    let shared = crate::services::discord::make_shared_data_for_tests();
    let voice_config = crate::voice::VoiceConfig::default();
    let data = Data {
        shared: shared.clone(),
        token: "test-token".into(),
        provider: ProviderKind::Claude,
        voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
        voice_config,
    };
    let reads = STORE_READS.load(Ordering::SeqCst);
    let clicks = [
        "hresume:v1:valid_nonce:yes",
        "hresume:v1:valid_nonce:no",
        "unrelated:component",
    ];
    for id in clicks {
        let component = component_fixture(id);
        let result = super::component(&ctx, &component, &data).await;
        assert!(
            matches!(result, Ok(false)),
            "Legacy mode must return the base unmatched result before consent custom-ID parsing"
        );
        let event = serenity::FullEvent::InteractionCreate {
            interaction: serenity::Interaction::Component(component),
        };
        assert!(
            super::super::handle_event(&ctx, &event, &data)
                .await
                .is_ok(),
            "Legacy dispatcher must preserve the base successful unknown-component result"
        );
    }
    let message = discord_mock::user_message(940_487_400_000_043, "!herdr consent retry");
    for text in ["!herdr consent retry", "!herdr malformed", "!unknown"] {
        assert!(
            matches!(
                retry(&ctx, &message, &data, message.channel_id, text).await,
                Ok(false)
            ),
            "Legacy retry must remain unmatched before command policy evaluation"
        );
    }
    assert!(
        recover(
            &ctx.http,
            &shared,
            &ProviderKind::Claude,
            Replay::Excluding(&std::collections::BTreeSet::new())
        )
        .await
        .is_ok(),
        "Legacy startup must preserve the base successful no-op"
    );
    assert_eq!(
        STORE_READS.load(Ordering::SeqCst),
        reads,
        "Legacy entrypoints must perform zero new consent store reads"
    );
    assert_eq!(
        state.local_note_posts.load(Ordering::SeqCst),
        0,
        "Legacy entrypoints must send zero consent cards"
    );
    assert!(
        state.unhandled.lock().unwrap().is_empty(),
        "Legacy entrypoints must not make new Discord requests"
    );
    for text in ["!unknown", "!herdr consent retry"] {
        let result = crate::services::discord::commands::handle_text_command(
            &ctx,
            &message,
            &data,
            message.channel_id,
            text,
        )
        .await;
        assert!(
            matches!(result, Ok(false)),
            "Legacy text entry leaves the retry and other `!` text to chat"
        );
    }
    assert_eq!(
        state.local_note_posts.load(Ordering::SeqCst),
        0,
        "Legacy text entry answers neither the retry nor other `!` text"
    );
    assert_eq!(
        STORE_READS.load(Ordering::SeqCst),
        reads,
        "the complete Legacy text entry must not read consent state"
    );
    server.abort();
}

#[test]
fn consent_card_ids_carry_only_an_opaque_nonce() {
    for id in ["hresume:v1:nonce_123:yes", "hresume:v1:nonce_123:no"] {
        let click = consent::card::parse(id).expect("valid consent click");
        assert_eq!(click.nonce, "nonce_123");
    }
    for id in [
        "hresume:v1:/home/me/config.toml:yes",
        "hresume:v1:nonce:yes:extra",
        "hresume:v1:nonce:maybe",
    ] {
        assert!(
            consent::card::parse(id).is_none(),
            "custom IDs must not accept a path or arbitrary action"
        );
    }
}

#[derive(Clone)]
struct InteractionHttp {
    calls: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
}
async fn interaction_http(
    axum::extract::State(state): axum::extract::State<InteractionHttp>,
    request: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    state
        .calls
        .lock()
        .unwrap()
        .push((path.clone(), body.clone()));
    if path.ends_with("/callback") {
        return axum::http::StatusCode::NO_CONTENT.into_response();
    }
    if method == axum::http::Method::GET && path.ends_with("/users/@me") {
        let mut user =
            serde_json::to_value(discord_mock::user_message(1, "fixture").author).unwrap();
        user["id"] = json!("940487400000003");
        user["bot"] = json!(true);
        return axum::Json(user).into_response();
    }
    if method == axum::http::Method::GET
        && path.ends_with(&format!("/channels/{}", discord_mock::CHANNEL_ID))
    {
        return axum::Json(json!({"id":discord_mock::CHANNEL_ID.to_string(),"type":0,"guild_id":"96326000999","name":"service-consent-cc","position":0,"permission_overwrites":[],"nsfw":false,"parent_id":null})).into_response();
    }
    let mut response = serde_json::to_value(discord_mock::user_message(
        940_487_400_000_051,
        "fixture reply",
    ))
    .unwrap();
    response["content"] = body.get("content").cloned().unwrap_or_default();
    if let Some(components) = body.get("components") {
        response["components"] = components.clone();
    }
    axum::Json(response).into_response()
}

#[tokio::test(flavor = "current_thread")]
async fn actual_component_owner_guard_precedes_store_for_allowed_allowall_and_missing_owner() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    let (_, gateway, gateway_server) =
        discord_mock::start(discord_mock::DiscordMockState::new()).await;
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .fallback(interaction_http)
        .with_state(InteractionHttp {
            calls: calls.clone(),
        });
    let http_server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    ctx.http.set_application_id(serenity::ApplicationId::new(
        ctx.cache.current_user().id.get(),
    ));
    let shared = crate::services::discord::make_shared_data_for_tests();
    let voice_config = crate::voice::VoiceConfig::default();
    let data = Data {
        shared: shared.clone(),
        token: "test-token".into(),
        provider: ProviderKind::Claude,
        voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
        voice_config,
    };
    let component = component_fixture("hresume:v1:owner_guard_fixture:yes");
    let reads = STORE_READS.load(Ordering::SeqCst);
    for (owner, allow_all, listed) in [
        (Some(111), false, true),
        (Some(111), true, false),
        (None, true, true),
    ] {
        {
            let mut settings = shared.settings.write().await;
            settings.owner_user_id = owner;
            settings.allow_all_users = allow_all;
            settings.allowed_user_ids = if listed {
                vec![component.user.id.get()]
            } else {
                Vec::new()
            };
        }
        let event = serenity::FullEvent::InteractionCreate {
            interaction: serenity::Interaction::Component(component.clone()),
        };
        assert!(
            super::super::handle_event(&ctx, &event, &data)
                .await
                .is_ok(),
            "recognized UserDefault card must ACK and reject a non-owner without running apply"
        );
        assert_eq!(
            STORE_READS.load(Ordering::SeqCst),
            reads,
            "check_owner must precede consent store/nonce lookup even for allowed and allow-all users"
        );
    }
    let calls = calls.lock().unwrap();
    let callbacks: Vec<_> = calls
        .iter()
        .filter(|(path, _)| path.ends_with("/callback"))
        .collect();
    assert_eq!(
        callbacks.len(),
        3,
        "recognized clicks must receive a quick ACK"
    );
    assert!(
        callbacks.iter().all(|(_, body)| body["type"] == 6),
        "ACK must precede work without replacing the card"
    );
    assert!(
        !root.path().join("runtime/herdr_policy").exists(),
        "denied owner decisions must create no policy files"
    );
    assert!(
        !calls.iter().any(|(path, _)| path.contains("/channels/")),
        "owner denial must not send cards, modify config or launch sessions"
    );
    drop(calls);
    http_server.abort();
    gateway_server.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn actual_component_rejects_stale_nonce_and_wrong_message_before_peer_or_apply() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    let (_, gateway, gateway_server) =
        discord_mock::start(discord_mock::DiscordMockState::new()).await;
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .fallback(interaction_http)
        .with_state(InteractionHttp {
            calls: calls.clone(),
        });
    let http_server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    ctx.http.set_application_id(serenity::ApplicationId::new(
        ctx.cache.current_user().id.get(),
    ));
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mut clicked = component_fixture("hresume:v1:stale_nonce:yes");
    let actor = clicked.user.id.get();
    shared.settings.write().await.owner_user_id = Some(actor);
    let voice_config = crate::voice::VoiceConfig::default();
    let data = Data {
        shared,
        token: "test-token".into(),
        provider: ProviderKind::Claude,
        voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
        voice_config,
    };
    let config = root.path().join("config.toml");
    let snapshot = store()
        .hold(
            consent::Question {
                key: consent::ConsentKey {
                    node: "node".into(),
                    user: unsafe { libc::geteuid() },
                    socket: "/adk/herdr/agentdesk.sock".into(),
                    named_session: "agentdesk".into(),
                    config: config.clone(),
                    target: config,
                },
                preview: consent::Preview {
                    revision: 1,
                    witness: crate::services::session_host::ServerWitness::for_test(777),
                    target_hash: [1; 32],
                    chain_hash: [2; 32],
                    checker_hash: [3; 32],
                },
                owner: actor,
                bot: ctx.cache.current_user().id.get(),
                provider: "claude".into(),
                channel: clicked.channel_id.get(),
                message: clicked.message.id.get() + 1,
                intent: consent::IntentRef {
                    store: "mailbox".into(),
                    id: "123:0".into(),
                    provider: "claude".into(),
                    channel: clicked.channel_id.get(),
                    source_message: 123,
                    fifo: 0,
                },
            },
            now(),
        )
        .unwrap()
        .into_consent();
    clicked.message.author.id = serenity::UserId::new(snapshot.bot);
    for custom in [
        "hresume:v1:stale_nonce:yes".to_string(),
        format!("hresume:v1:{}:yes", snapshot.nonce),
    ] {
        clicked.data.custom_id = custom;
        let result = super::component(&ctx, &clicked, &data).await;
        assert!(
            matches!(result, Ok(true)),
            "stale card rejection must complete without peer access: {result:?}"
        );
    }
    assert_eq!(
        store().load(&snapshot.key).unwrap(),
        Some(snapshot),
        "stale nonce and wrong-message clicks must preserve the exact pending consent"
    );
    {
        let calls = calls.lock().unwrap();
        let replies: Vec<_> = calls
            .iter()
            .filter(|(path, _)| path.contains("/webhooks/"))
            .collect();
        assert_eq!(
            replies.len(),
            2,
            "each stale click needs an explicit refusal"
        );
        assert!(
            replies[0].1["content"]
                .as_str()
                .unwrap()
                .contains("stale or missing"),
            "nonce lookup must refuse a stale nonce before selecting a record"
        );
        assert!(
            replies[1].1["content"]
                .as_str()
                .unwrap()
                .contains("identity changed"),
            "a correct nonce must not authorize a different message"
        );
        assert!(
            !calls
                .iter()
                .any(|(path, _)| path.contains("/channels/") || path.contains("/users/")),
            "stale clicks must perform zero card, owner-probe and launch requests"
        );
    }
    let records_before = store().load_all().unwrap();
    let extra_channel = discord_mock::CHANNEL_ID + 100;
    {
        let _channels = session_hosts::force_for_test(
            Some("node"),
            &[(discord_mock::CHANNEL_ID, "node"), (extra_channel, "node")],
        )
        .with_mode(EndpointMode::UserDefault);
        let mut crossed = clicked.clone();
        crossed.channel_id = serenity::ChannelId::new(extra_channel);
        crossed.message.channel_id = crossed.channel_id;
        crossed.message.id = serenity::MessageId::new(940_487_400_000_042);
        assert!(
            matches!(super::component(&ctx, &crossed, &data).await, Ok(true)),
            "a valid nonce copied into another mapped channel must be explicitly refused"
        );
        let last = calls.lock().unwrap().last().unwrap().1["content"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            last.contains("identity changed"),
            "another channel must fail the card identity guard before peer or apply"
        );
    }
    assert_eq!(
        store().load_all().unwrap(),
        records_before,
        "cross-channel card reuse must preserve every original consent and custody reference"
    );
    let before_remote = STORE_READS.load(Ordering::SeqCst);
    let before_remote_calls = calls.lock().unwrap().len();
    {
        let _remote = session_hosts::force_for_test(
            Some("node"),
            &[(discord_mock::CHANNEL_ID, "remote-node")],
        )
        .with_mode(EndpointMode::UserDefault);
        assert!(
            matches!(super::component(&ctx, &clicked, &data).await, Ok(true)),
            "remote consent must ACK and refuse without dispatching apply"
        );
    }
    assert_eq!(
        STORE_READS.load(Ordering::SeqCst),
        before_remote,
        "remote consent must refuse before store, checker, apply or release"
    );
    assert_eq!(
        store().load_all().unwrap(),
        records_before,
        "remote consent must preserve every original consent and custody reference"
    );
    let remote_guard = calls.lock().unwrap();
    let remote_calls = &remote_guard[before_remote_calls..];
    assert_eq!(
        remote_calls.len(),
        2,
        "remote consent may only ACK and send the execution-node instruction"
    );
    assert!(remote_calls[0].0.ends_with("/callback"));
    let instruction = remote_calls[1].1["content"].as_str().unwrap();
    assert!(
        instruction.contains("실행 노드 remote-node의 gateway에서 !herdr consent retry")
            && instruction.contains("현재 gateway는 원격 apply와 입력 해제를 전달하지 않습니다"),
        "remote consent must name the real execution node and explain that this gateway cannot apply or release remotely"
    );
    assert!(
        !remote_calls
            .iter()
            .any(|(path, _)| path.contains("/channels/") || path.contains("/users/")),
        "remote consent must perform zero card, owner-probe and launch requests"
    );
    drop(remote_guard);
    http_server.abort();
    gateway_server.abort();
}

struct ScopedPeer(Option<Arc<dyn PeerSource>>);
impl Drop for ScopedPeer {
    fn drop(&mut self) {
        TEST_SOURCE.with(|source| *source.borrow_mut() = self.0.take());
    }
}

// Real Discord adapters and durable mailbox sink use a fake checker and a drain executor.
#[tokio::test(flavor = "current_thread")]
async fn actual_owner_retry_card_apply_and_duplicate_use_one_original_mailbox_payload() {
    use crate::services::discord::{IdleQueueKickoffChannelOutcome, queue_io};
    use crate::services::turn_orchestrator::{
        Intervention, InterventionMode, QueuePersistenceContext,
    };
    let runtime = tempfile::tempdir().unwrap();
    let _runtime = crate::config::set_agentdesk_root_for_test(runtime.path());
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    let (config_dir, peer) = preview::tests::fixture(
        r#"if [ "$HERDR_CONFIG_PATH" = "$HOME/config.toml" ]; then printf 'apply\n' >> "$HOME/apply.calls"; fi; printf 'config: ok\n'"#,
    );
    peer.0.lock().unwrap().witness.socket = "/adk/herdr/agentdesk.sock".into();
    let previous = TEST_SOURCE.with(|source| source.replace(Some(peer.clone())));
    let _peer = ScopedPeer(previous);
    let fixture_config = crate::runtime_layout::config_file_path(runtime.path());
    std::fs::create_dir_all(fixture_config.parent().unwrap()).unwrap();
    std::fs::write(&fixture_config,serde_json::to_vec(&json!({"server":{},"agents":[{"id":"service-consent","name":"Consent fixture","provider":"claude","voice_enabled":false,"channels":{"claude":{"id":discord_mock::CHANNEL_ID.to_string(),"name":"service-consent-cc","workspace":config_dir.path(),"runtime":"tui"}}}]})).unwrap()).unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &fixture_config,
    );
    let (_, gateway, gateway_server) =
        discord_mock::start(discord_mock::DiscordMockState::new()).await;
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .fallback(interaction_http)
        .with_state(InteractionHttp {
            calls: calls.clone(),
        });
    let http_server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    ctx.http.set_application_id(serenity::ApplicationId::new(
        ctx.cache.current_user().id.get(),
    ));
    let shared = crate::services::discord::make_shared_data_for_tests();
    let message = discord_mock::user_message(940_487_400_000_061, "!herdr consent retry");
    let binding = crate::services::discord::settings::resolve_role_binding(
        message.channel_id,
        Some("service-consent-cc"),
    );
    assert!(
        binding
            .as_ref()
            .is_some_and(|binding| binding.provider.as_ref() == Some(&ProviderKind::Claude)),
        "actual service fixture must use its canonical configured provider binding"
    );
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(message.author.id.get());
        settings.allowed_channel_ids = vec![message.channel_id.get()];
    }
    startup::restore(&ctx.http, &shared, &ProviderKind::Claude)
        .await
        .expect("empty consent boot completes before apply");
    let prepared = preview::prepare(peer, 1).await.unwrap();
    let key = consent::ConsentKey {
        node: "node".into(),
        user: unsafe { libc::geteuid() },
        socket: "/adk/herdr/agentdesk.sock".into(),
        named_session: "agentdesk".into(),
        config: prepared.logical().into(),
        target: prepared.target().into(),
    };
    let original = Intervention {
        author_id: message.author.id,
        author_is_bot: false,
        message_id: message.id,
        queued_generation: 17,
        source_message_ids: vec![message.id],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "original preserved payload".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: Some("original reply boundary".into()),
        has_reply_boundary: true,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    };
    let enqueued = shared
        .mailbox(message.channel_id)
        .enqueue(
            original.clone(),
            QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None),
        )
        .await;
    assert!(
        enqueued.enqueued && enqueued.persistence_error.is_none(),
        "positive service fixture must own one durable original mailbox input"
    );
    let pending = store()
        .hold(
            consent::Question {
                key: key.clone(),
                preview: prepared.token,
                owner: message.author.id.get(),
                bot: ctx.cache.current_user().id.get(),
                provider: "claude".into(),
                channel: message.channel_id.get(),
                message: 940_487_400_000_062,
                intent: consent::IntentRef {
                    store: "mailbox".into(),
                    id: format!("{}:17", message.id.get()),
                    provider: "claude".into(),
                    channel: message.channel_id.get(),
                    source_message: message.id.get(),
                    fifo: 17,
                },
            },
            now(),
        )
        .unwrap()
        .into_consent();
    store()
        .decide(&pending, message.author.id.get(), false, now())
        .unwrap();
    let drained = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = drained.clone();
    let receipt_key = key.clone();
    let _executor = queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
        move |shared, provider, channel, _| {
            let count = count.clone();
            let receipt_key = receipt_key.clone();
            Box::pin(async move {
                assert_eq!(
                    store().load(&receipt_key).unwrap().unwrap().state,
                    State::Applied,
                    "actual service must durably record the receipt before the existing mailbox drain"
                );
                let taken = shared
                    .mailbox(channel)
                    .take_next_soft(QueuePersistenceContext::new(
                        &provider,
                        &shared.token_hash,
                        None,
                    ))
                    .await;
                if let Some(item) = taken.intervention {
                    assert_eq!(item.text, "original preserved payload");
                    assert_eq!(
                        item.reply_context.as_deref(),
                        Some("original reply boundary")
                    );
                    crate::services::discord::mailbox_abandon_unclaimed_dispatch_after_success(
                        &shared,
                        &provider,
                        channel,
                        item.message_id,
                        taken
                            .dispatch_lease
                            .as_ref()
                            .expect("fake executor owns dequeue lease")
                            .clone(),
                    )
                    .await;
                    count.fetch_add(1, Ordering::SeqCst);
                }
                Some(IdleQueueKickoffChannelOutcome { started: true })
            })
        },
    ));
    let voice_config = crate::voice::VoiceConfig::default();
    let data = Data {
        shared: shared.clone(),
        token: "test-token".into(),
        provider: ProviderKind::Claude,
        voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
        voice_config,
    };
    let ensured = crate::services::session_host::autostart::spawn::ensured_on_this_thread();
    let retry_result = crate::services::discord::commands::handle_text_command(
        &ctx,
        &message,
        &data,
        message.channel_id,
        &message.content,
    )
    .await;
    assert!(
        matches!(retry_result, Ok(true)),
        "actual owner retry must be handled before ordinary text-command policy"
    );
    assert!(
        crate::services::session_host::autostart::spawn::ensured_on_this_thread() > ensured,
        "a fresh card asks for its live server first"
    );
    let bound = store().load(&key).unwrap().unwrap();
    assert_eq!(
        bound.state,
        State::Pending,
        "owner retry must reserve, send and durably bind a new pending card"
    );
    assert_ne!(
        bound.message, 0,
        "the actual card must have a durable message binding"
    );
    assert_eq!(
        bound.card,
        consent::CardState::Bound,
        "buttons may be enabled only after binding"
    );
    let mut click = component_fixture(&format!("hresume:v1:{}:yes", bound.nonce));
    click.message.id = serenity::MessageId::new(bound.message);
    click.message.author.id = serenity::UserId::new(bound.bot);
    let mut stale = click.clone();
    stale.data.custom_id = "hresume:v1:obsolete_nonce:yes".into();
    let stale_result = super::component(&ctx, &stale, &data).await;
    assert!(
        stale_result.is_ok(),
        "actual stale callback must complete: {stale_result:?}"
    );
    assert_eq!(
        store().load(&key).unwrap().unwrap().state,
        State::Pending,
        "actual stale nonce must not apply or release the original input"
    );
    let mut wrong_message = click.clone();
    wrong_message.message.id = serenity::MessageId::new(bound.message + 10);
    assert!(super::component(&ctx, &wrong_message, &data).await.is_ok());
    assert_eq!(
        store().load(&key).unwrap().unwrap().state,
        State::Pending,
        "actual wrong message must not apply or release the original input"
    );
    assert_eq!(
        drained.load(Ordering::SeqCst),
        0,
        "stale interactions must preserve the original mailbox payload before approval"
    );
    for _ in 0..2 {
        assert!(
            matches!(super::component(&ctx, &click, &data).await, Ok(true)),
            "actual owner and duplicate clicks must finish without direct launch"
        );
    }
    let applied = store().load(&key).unwrap().unwrap();
    assert_eq!(
        applied.state,
        State::Applied,
        "actual owner click must publish false and record the durable Applied receipt"
    );
    assert!(
        std::fs::read_to_string(&key.config)
            .unwrap()
            .contains("resume_agents_on_restore=false"),
        "actual service must apply false with the fake full checker"
    );
    assert!(
        applied
            .apply
            .as_ref()
            .and_then(|result| result.backup.as_ref())
            .is_some_and(|path| path.exists()),
        "the actual edit must preserve its backup"
    );
    assert_eq!(
        drained.load(Ordering::SeqCst),
        1,
        "duplicate owner approval must release and drain the original payload exactly once"
    );
    assert_eq!(
        std::fs::read_to_string(config_dir.path().join("apply.calls"))
            .unwrap()
            .lines()
            .count(),
        1,
        "duplicate owner approval must perform zero additional config apply checks"
    );
    assert!(
        shared
            .mailbox(message.channel_id)
            .snapshot()
            .await
            .intervention_queue
            .is_empty(),
        "the existing drain must consume the original item without recreating it"
    );
    assert_eq!(
        applied.settlements.len(),
        1,
        "the exact original reference must be acknowledged once"
    );
    {
        let calls = calls.lock().unwrap();
        let sends: Vec<_> = calls
            .iter()
            .filter(|(path, body)| path.ends_with("/messages") && body.get("components").is_some())
            .collect();
        assert_eq!(
            sends.len(),
            1,
            "owner retry and duplicate decisions must send only one card"
        );
        assert_eq!(
            sends[0].1["components"][0]["components"][0]["disabled"], true,
            "the actual transport must initially send disabled buttons"
        );
        assert!(
            calls
                .iter()
                .any(|(_, body)| body["components"][0]["components"][0]["disabled"] == false),
            "the actual transport must enable buttons only after durable binding"
        );
        drop(calls);
    }
    // Reconstruct receipt-committed / release-not-yet-run crash state with an existing custody ref.
    let restored_id = serenity::MessageId::new(message.id.get() + 100);
    let mut restored = original;
    restored.message_id = restored_id;
    restored.queued_generation = 18;
    restored.source_message_ids = vec![restored_id];
    restored.source_message_queued_generations.clear();
    restored.source_text_segments.clear();
    let second = shared
        .mailbox(message.channel_id)
        .enqueue(
            restored,
            QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None),
        )
        .await;
    assert!(
        second.enqueued && second.persistence_error.is_none(),
        "startup replay fixture must retain its exact second original input durably"
    );
    let interrupted = store()
        .hold(
            consent::Question {
                key: key.clone(),
                preview: applied.preview.clone(),
                owner: applied.owner,
                bot: applied.bot,
                provider: applied.provider.clone(),
                channel: applied.channel,
                message: applied.message,
                intent: consent::IntentRef {
                    store: "mailbox".into(),
                    id: format!("{}:18", restored_id.get()),
                    provider: "claude".into(),
                    channel: message.channel_id.get(),
                    source_message: restored_id.get(),
                    fifo: 18,
                },
            },
            now(),
        )
        .unwrap()
        .into_consent();
    assert_eq!(
        interrupted.nonce, applied.nonce,
        "startup replay fixture must preserve the already applied generation"
    );
    assert_eq!(interrupted.state, State::Applied);
    assert_eq!(
        interrupted.releasable_intents().len(),
        1,
        "startup replay fixture must leave one receipt-backed original reference unresolved"
    );
    for _ in 0..2 {
        let recovered = startup::restore(&ctx.http, &shared, &ProviderKind::Claude).await;
        assert!(
            recovered.is_ok(),
            "actual Applied startup replay must complete: {recovered:?}"
        );
    }
    assert_eq!(
        drained.load(Ordering::SeqCst),
        2,
        "startup_applied_recovery_drains_original_once_contract"
    );
    let recovered = store().load(&key).unwrap().unwrap();
    assert_eq!(
        recovered.settlements.len(),
        2,
        "startup Applied replay must acknowledge each original reference once"
    );
    assert_eq!(
        recovered.nonce, applied.nonce,
        "startup Applied replay must use the original consent nonce"
    );
    assert_eq!(
        recovered.apply, applied.apply,
        "startup Applied replay must preserve the original durable receipt and backup"
    );
    assert!(
        shared
            .mailbox(message.channel_id)
            .snapshot()
            .await
            .intervention_queue
            .is_empty(),
        "startup Applied replay must drain its original payload without recreating it"
    );
    assert_eq!(
        std::fs::read_to_string(config_dir.path().join("apply.calls"))
            .unwrap()
            .lines()
            .count(),
        1,
        "startup Applied replay must perform zero new config apply checks"
    );
    let sends = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, body)| path.ends_with("/messages") && body.get("components").is_some())
        .count();
    assert_eq!(
        sends, 1,
        "startup Applied replay must send zero new consent cards"
    );
    http_server.abort();
    gateway_server.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn actual_applied_startup_keeps_failed_consent_retryable_and_releases_later_once() {
    use crate::services::discord::{IdleQueueKickoffChannelOutcome, queue_io};
    use crate::services::turn_orchestrator::{
        Intervention, InterventionMode, QueuePersistenceContext,
    };
    let runtime = tempfile::tempdir().unwrap();
    let _runtime = crate::config::set_agentdesk_root_for_test(runtime.path());
    let channel = serenity::ChannelId::new(discord_mock::CHANNEL_ID);
    let _boot = session_hosts::force_for_test(Some("node"), &[(channel.get(), "node")])
        .with_mode(EndpointMode::UserDefault);
    let (config_dir, peer) = preview::tests::fixture("printf 'config: ok\\n'");
    peer.0.lock().unwrap().witness.socket = "/adk/herdr/agentdesk.sock".into();
    let previous = TEST_SOURCE.with(|source| source.replace(Some(peer.clone())));
    let _peer = ScopedPeer(previous);
    let token = consent::Preview {
        revision: 1,
        witness: peer.snapshot().unwrap().witness,
        target_hash: [1; 32],
        chain_hash: [2; 32],
        checker_hash: [3; 32],
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .fallback(interaction_http)
        .with_state(InteractionHttp {
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
    let http_server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(proxy)
            .ratelimiter_disabled(true)
            .build(),
    );
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    for source in [940_487_400_000_071_u64, 940_487_400_000_072] {
        let path = config_dir.path().join(format!("{source}.toml"));
        let pending = store()
            .hold(
                consent::Question {
                    key: consent::ConsentKey {
                        node: "node".into(),
                        user: unsafe { libc::geteuid() },
                        socket: "/adk/herdr/agentdesk.sock".into(),
                        named_session: "agentdesk".into(),
                        config: path.clone(),
                        target: path,
                    },
                    preview: token.clone(),
                    owner: 1,
                    bot: 940_487_400_000_003,
                    provider: "claude".into(),
                    channel: channel.get(),
                    message: source + 100,
                    intent: consent::IntentRef {
                        store: "mailbox".into(),
                        id: format!("{source}:17"),
                        provider: "claude".into(),
                        channel: channel.get(),
                        source_message: source,
                        fifo: 17,
                    },
                },
                now(),
            )
            .unwrap()
            .into_consent();
        let accepted = store().decide(&pending, 1, true, now()).unwrap();
        store()
            .record_apply(
                &accepted,
                consent::ApplyResult {
                    before: Some([1; 32]),
                    after: Some([4; 32]),
                    backup: None,
                    detail: None,
                },
            )
            .unwrap();
    }
    let ordered = store().load_all().unwrap();
    assert_eq!(
        ordered.len(),
        2,
        "fixture must contain two actual Applied receipts"
    );
    let failed = &ordered[0];
    let later = &ordered[1];
    let effects = Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));
    let observed = effects.clone();
    let _executor = queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
        move |shared, provider, channel, _| {
            let observed = observed.clone();
            Box::pin(async move {
                let taken = shared
                    .mailbox(channel)
                    .take_next_soft(QueuePersistenceContext::new(
                        &provider,
                        &shared.token_hash,
                        None,
                    ))
                    .await;
                if let Some(item) = taken.intervention {
                    assert_eq!(item.text, format!("original {}", item.message_id));
                    crate::services::discord::mailbox_abandon_unclaimed_dispatch_after_success(
                        &shared,
                        &provider,
                        channel,
                        item.message_id,
                        taken
                            .dispatch_lease
                            .as_ref()
                            .expect("original lease")
                            .clone(),
                    )
                    .await;
                    *observed
                        .lock()
                        .unwrap()
                        .entry(item.message_id.get())
                        .or_insert(0_u32) += 1;
                }
                Some(IdleQueueKickoffChannelOutcome { started: true })
            })
        },
    ));
    // First recover has only the later receipt's original payload; the first release must fail.
    for (step, record) in [later, failed].into_iter().enumerate() {
        let source = serenity::MessageId::new(record.intents[0].source_message);
        let enqueued = shared
            .mailbox(channel)
            .enqueue(
                Intervention {
                    author_id: serenity::UserId::new(1),
                    author_is_bot: false,
                    message_id: source,
                    queued_generation: 17,
                    source_message_ids: vec![source],
                    source_message_queued_generations: Vec::new(),
                    source_text_segments: Vec::new(),
                    text: format!("original {source}"),
                    mode: InterventionMode::Soft,
                    created_at: std::time::Instant::now(),
                    reply_context: None,
                    has_reply_boundary: false,
                    merge_consecutive: false,
                    pending_uploads: Vec::new(),
                    voice_announcement: None,
                },
                QueuePersistenceContext::new(&provider, &shared.token_hash, None),
            )
            .await;
        assert!(enqueued.enqueued && enqueued.persistence_error.is_none());
        for _ in 0..2 {
            let result = recover(
                &http,
                &shared,
                &provider,
                Replay::Excluding(&std::collections::BTreeSet::new()),
            )
            .await;
            let later_now = store().load(&later.key).unwrap().unwrap();
            assert_eq!(
                (
                    effects
                        .lock()
                        .unwrap()
                        .get(&later.intents[0].source_message)
                        .copied()
                        .unwrap_or(0),
                    later_now.settlements.len()
                ),
                (1, 1),
                "startup_failed_applied_consent_never_skips_later_release_contract"
            );
            let failed_now = store().load(&failed.key).unwrap().unwrap();
            if step == 0 {
                assert!(
                    result.as_ref().is_err_and(
                        |error| error.contains("durable mailbox custody reference missing")
                    ) && failed_now.state == State::Applied
                        && failed_now.releasable_intents().len() == 1
                        && failed_now.settlements.is_empty(),
                    "startup_failed_applied_consent_stays_retryable_contract: {result:?}"
                );
            } else {
                assert!(
                    result.is_ok(),
                    "restored original reference must recover: {result:?}"
                );
                assert_eq!(
                    (
                        effects
                            .lock()
                            .unwrap()
                            .get(&failed.intents[0].source_message)
                            .copied()
                            .unwrap_or(0),
                        failed_now.settlements.len()
                    ),
                    (1, 1),
                    "startup_repaired_applied_consent_releases_and_acknowledges_once_contract"
                );
            }
        }
    }
    assert!(
        shared
            .mailbox(channel)
            .snapshot()
            .await
            .intervention_queue
            .is_empty()
    );
    http_server.abort();
}

// Real boot recovery: one channel's queue merge is held at boot and settles on its retry.
#[tokio::test(flavor = "current_thread")]
async fn actual_held_boot_channel_replays_only_its_applied_consent_once_after_merge() {
    use crate::services::discord::{IdleQueueKickoffChannelOutcome, queue_io};
    use crate::services::turn_orchestrator::{
        Intervention, InterventionMode, QueuePersistenceContext,
    };
    let runtime = tempfile::tempdir().unwrap();
    let _runtime = crate::config::set_agentdesk_root_for_test(runtime.path());
    let (held, open) = (940_487_400_000_201_u64, 940_487_400_000_202_u64);
    let agents: String = [held, open]
        .iter()
        .map(|id| format!("  - id: consent-{id}\n    name: consent-{id}\n    provider: claude\n    channels:\n      claude:\n        id: \"{id}\"\n        name: consent-{id}\n"))
        .collect();
    let config = crate::runtime_layout::config_file_path(runtime.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, format!("server:\n  port: 8791\nagents:\n{agents}")).unwrap();
    let _boot = session_hosts::force_for_test(Some("node"), &[(held, "node"), (open, "node")])
        .with_mode(EndpointMode::UserDefault);
    let (config_dir, peer) = preview::tests::fixture("printf 'config: ok\\n'");
    peer.0.lock().unwrap().witness.socket = "/adk/herdr/agentdesk.sock".into();
    let previous = TEST_SOURCE.with(|source| source.replace(Some(peer.clone())));
    let _peer = ScopedPeer(previous);
    let token = consent::Preview {
        revision: 1,
        witness: peer.snapshot().unwrap().witness,
        target_hash: [1; 32],
        chain_hash: [2; 32],
        checker_hash: [3; 32],
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = axum::Router::new()
        .fallback(interaction_http)
        .with_state(InteractionHttp {
            calls: calls.clone(),
        });
    let http_server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(proxy)
            .ratelimiter_disabled(true)
            .build(),
    );
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let mut cards = std::collections::BTreeMap::new();
    for channel in [held, open] {
        let source = channel + 1_000;
        let path = config_dir.path().join(format!("{source}.toml"));
        let pending = store()
            .hold(
                consent::Question {
                    key: consent::ConsentKey {
                        node: "node".into(),
                        user: unsafe { libc::geteuid() },
                        socket: "/adk/herdr/agentdesk.sock".into(),
                        named_session: "agentdesk".into(),
                        config: path.clone(),
                        target: path,
                    },
                    preview: token.clone(),
                    owner: 1,
                    bot: 940_487_400_000_003,
                    provider: "claude".into(),
                    channel,
                    message: source + 100,
                    intent: consent::IntentRef {
                        store: "mailbox".into(),
                        id: format!("{source}:17"),
                        provider: "claude".into(),
                        channel,
                        source_message: source,
                        fifo: 17,
                    },
                },
                now(),
            )
            .unwrap()
            .into_consent();
        let accepted = store().decide(&pending, 1, true, now()).unwrap();
        let applied = store()
            .record_apply(
                &accepted,
                consent::ApplyResult {
                    before: Some([1; 32]),
                    after: Some([4; 32]),
                    backup: None,
                    detail: None,
                },
            )
            .unwrap();
        cards.insert(channel, (applied.key, source + 100));
        let source = serenity::MessageId::new(source);
        crate::services::turn_orchestrator::save_channel_queue(
            &provider,
            &shared.token_hash,
            serenity::ChannelId::new(channel),
            &[Intervention {
                author_id: serenity::UserId::new(1),
                author_is_bot: false,
                message_id: source,
                queued_generation: 17,
                source_message_ids: vec![source],
                source_message_queued_generations: Vec::new(),
                source_text_segments: Vec::new(),
                text: format!("original {source}"),
                mode: InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            }],
            None,
        )
        .unwrap();
    }
    let effects = Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));
    let observed = effects.clone();
    // Only the consent wake runs a payload; other kicks stand in for admission holding it.
    let _executor = queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
        move |shared, provider, channel, reason| {
            let observed = observed.clone();
            Box::pin(async move {
                if reason != "herdr_consent_release" {
                    return Some(IdleQueueKickoffChannelOutcome { started: false });
                }
                let context = QueuePersistenceContext::new(&provider, &shared.token_hash, None);
                let taken = shared.mailbox(channel).take_next_soft(context).await;
                if let Some(item) = taken.intervention {
                    let lease = taken.dispatch_lease.expect("original lease");
                    crate::services::discord::mailbox_abandon_unclaimed_dispatch_after_success(
                        &shared,
                        &provider,
                        channel,
                        item.message_id,
                        lease,
                    )
                    .await;
                    *observed
                        .lock()
                        .unwrap()
                        .entry(item.message_id.get())
                        .or_insert(0_u32) += 1;
                }
                Some(IdleQueueKickoffChannelOutcome { started: true })
            })
        },
    ));
    let queue_file = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&shared.token_hash)
        .join(format!("{held}.json"));
    let valid = std::fs::read(&queue_file).unwrap();
    // The held channel's file tears after boot read it, so only its merge fails.
    let settings = shared.settings.write().await;
    let boot = tokio::spawn({
        let (http, shared, provider) = (http.clone(), shared.clone(), provider.clone());
        async move {
            crate::services::discord::runtime_bootstrap::restore_queued_and_inflight_for_tests(
                &http, &shared, &provider,
            )
            .await
        }
    });
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    std::fs::write(&queue_file, br#"[{"message_id": 1, "te"#).unwrap();
    drop(settings);
    boot.await.unwrap();
    let replays = |channel: u64| {
        let card = format!("/messages/{}", cards[&channel].1);
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path.ends_with(&card))
            .count()
    };
    let ran = |channel: u64| {
        let source = channel + 1_000;
        effects.lock().unwrap().get(&source).copied().unwrap_or(0)
    };
    assert_eq!(
        (replays(open), ran(open), replays(held), ran(held)),
        (1, 1, 0, 0),
        "boot_replays_open_channel_and_skips_held_channel_contract"
    );
    std::fs::write(&queue_file, &valid).unwrap();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    tokio::time::resume();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while replays(held) == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let settled = store().load(&cards[&held].0).unwrap().unwrap();
    assert_eq!(
        (replays(held), ran(held), settled.settlements.len()),
        (1, 1, 1),
        "held_channel_merge_replays_its_applied_consent_once_contract"
    );
    let again = super::startup::restore_only(&http, &shared, &provider, held).await;
    assert!(again.is_ok(), "{again:?}");
    assert_eq!(
        (replays(open), ran(open), replays(held), ran(held)),
        (1, 1, 1, 1),
        "held_replay_never_repeats_open_or_already_replayed_consents_contract"
    );
    http_server.abort();
}
