use super::*;
use crate::services::discord::input_runtime::supervisor::{Registration, command::SupervisorCmd};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::tui_input::ledger::{Ledger, Presence};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    http::{Method, StatusCode, Uri},
    response::IntoResponse,
};
use futures::StreamExt;
use poise::serenity_prelude as serenity;
use serde_json::json;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Notify, mpsc};

#[derive(Clone, Copy)]
enum Mode {
    PauseReply,
    PauseDownload,
    FailReply,
    FailDownload,
    Complete,
}

struct State {
    channel: u64,
    mode: Mode,
    reply: Message,
    calls: Mutex<Vec<(Method, String)>>,
    downloads: AtomicUsize,
    entered: Notify,
    release: Notify,
}

struct Transport {
    http: Arc<Http>,
    state: Arc<State>,
    download: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Transport {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Transport {
    async fn new(channel: u64, mode: Mode) -> Self {
        let mut reply = discord_mock::user_message(channel + 1, "actual referenced message");
        reply.channel_id = ChannelId::new(channel);
        reply.guild_id = Some(serenity::GuildId::new(channel + 900));
        let state = Arc::new(State {
            channel,
            mode,
            reply,
            calls: Mutex::new(vec![]),
            downloads: AtomicUsize::new(0),
            entered: Notify::new(),
            release: Notify::new(),
        });
        let route = state.clone();
        let app = Router::new().fallback(move |method: Method, uri: Uri| {
            let state = route.clone();
            async move {
                let path = uri.path().to_string();
                state.calls.lock().unwrap().push((method.clone(), path.clone()));
                if method != Method::GET { return (StatusCode::FORBIDDEN, Json(json!({"code":50013,"message":"no mutation allowed"}))).into_response(); }
                if path.ends_with(&format!("/channels/{}", state.channel)) {
                    return Json(json!({"id":state.channel.to_string(),"type":0,"guild_id":(state.channel+900).to_string(),"name":"c2b-io-cc","position":0,"permission_overwrites":[],"nsfw":false,"parent_id":null})).into_response();
                }
                if path.ends_with(&format!("/channels/{}/messages/{}", state.channel, state.channel+1)) {
                    state.entered.notify_one();
                    if matches!(state.mode, Mode::PauseReply) { state.release.notified().await; }
                    if matches!(state.mode, Mode::FailReply) { return (StatusCode::FORBIDDEN, Json(json!({"code":10008,"message":"fixture reply unavailable"}))).into_response(); }
                    return Json(serde_json::to_value(&state.reply).unwrap()).into_response();
                }
                if path == "/download" {
                    let index = state.downloads.fetch_add(1, Ordering::SeqCst);
                    if matches!(state.mode, Mode::FailDownload) { state.entered.notify_one(); return StatusCode::FORBIDDEN.into_response(); }
                    if matches!(state.mode, Mode::PauseDownload) {
                        let tail = state.clone();
                        let first = futures::stream::once(async { Ok::<_, std::io::Error>(Bytes::from_static(b"prefix-")) });
                        let second = futures::stream::once(async move { tail.entered.notify_one(); tail.release.notified().await; Ok::<_, std::io::Error>(Bytes::from_static(b"tail")) });
                        return Body::from_stream(first.chain(second)).into_response();
                    }
                    let body = if index == 0 { b"first bytes".as_slice() } else { b"second bytes".as_slice() };
                    return body.to_vec().into_response();
                }
                (StatusCode::NOT_FOUND, Json(json!({"code":10003,"message":"unexpected fixture endpoint"}))).into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy(&base)
                .ratelimiter_disabled(true)
                .build(),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            http,
            state,
            download: format!("{base}/download"),
            task,
        }
    }
}

fn original(channel: u64, reply: bool, attachments: usize) -> Message {
    let mut message = discord_mock::user_message(channel + 2, "  actual original input  ");
    message.channel_id = ChannelId::new(channel);
    message.guild_id = Some(serenity::GuildId::new(channel + 900));
    message.kind = if reply {
        MessageType::InlineReply
    } else {
        MessageType::Regular
    };
    if reply {
        message.message_reference = Some(
            serenity::MessageReference::new(
                serenity::MessageReferenceKind::Default,
                message.channel_id,
            )
            .message_id(serenity::MessageId::new(channel + 1)),
        );
    }
    message.attachments = (0..attachments).map(|index| serde_json::from_value(json!({"id":(channel+10+index as u64).to_string(),"filename":format!("{index}.txt"),"url":format!("https://cdn.discordapp.com/attachments/{channel}/{index}.txt"),"proxy_url":format!("https://media.discordapp.net/attachments/{channel}/{index}.txt"),"size":12,"content_type":"text/plain"})).unwrap()).collect();
    message
}

async fn shared(message: &Message) -> Arc<SharedData> {
    let shared = crate::services::discord::make_shared_data_for_tests();
    {
        let mut settings = shared.settings.write().await;
        settings.provider = ProviderKind::Claude;
        settings.owner_user_id = Some(message.author.id.get());
        settings.allowed_channel_ids = vec![message.channel_id.get()];
    }
    crate::services::discord::host_defer_gate::tests::map_channel(
        &shared,
        message.channel_id,
        "c2b-io-cc",
    )
    .await;
    shared
}

fn ready(
    channel: u64,
    root: &std::path::Path,
) -> (
    Registration,
    Arc<runtime::intake::Client>,
    mpsc::Receiver<SupervisorCmd>,
) {
    let registry = runtime::runtime::registry();
    let registration = registry
        .register(&ProviderKind::Claude, channel, root)
        .unwrap();
    let (sender, receiver) = mpsc::channel(4);
    let client = registration.ready_intake_for_test(sender);
    (registration, client, receiver)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn run_hold_case(root: &std::path::Path, channel: u64, mode: Mode, cancel: bool) {
    let runtime = runtime();
    let transport = runtime.block_on(Transport::new(channel, mode));
    let _download = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_C2B_TEST_ATTACHMENT_URL",
        std::ffi::OsStr::new(&transport.download),
    );
    runtime.block_on(hold_case(root, channel, mode, cancel, transport));
}

async fn hold_case(
    root: &std::path::Path,
    channel: u64,
    mode: Mode,
    cancel: bool,
    transport: Transport,
) {
    let reply = matches!(mode, Mode::PauseReply | Mode::FailReply);
    let message = original(channel, reply, usize::from(!reply));
    let unchanged = serde_json::to_value(&message).unwrap();
    let shared = shared(&message).await;
    let _gate = runtime::fence::Gate::protect(ProviderKind::Claude, channel).unwrap();
    let (mut registration, _client, mut receiver) = ready(channel, root);
    let source = admit::prepare(
        SourceKind::LiveMessage,
        draft(&message, channel, message.content.clone()),
        Some(if reply { "inline_reply" } else { "regular" }),
    )
    .unwrap();
    let facts = runtime::source_policy_facts(&shared, &transport.http, &source).await;
    assert_eq!(
        runtime::intake::classify(&facts, &source),
        Classification::Target,
        "fixture policy provider={:?} original={:?} execution={:?} voice={:?} authorized={:?} supported={:?} disposition={:?} ready={} open={} source={:?} HTTP={:?}",
        facts.provider,
        facts.original,
        facts.execution,
        facts.voice_target,
        facts.authorized,
        facts.supported,
        facts.disposition,
        facts.client_ready,
        facts.admission_open,
        source.input(),
        transport.state.calls.lock().unwrap()
    );
    transport.state.calls.lock().unwrap().clear();
    let overflow = registration.admission_order_for_test().pending_overflow();
    let generation = overflow.generation();
    let population = runtime::runtime::registry().population_for_test();
    let provider = ProviderKind::Claude;
    let mut pending = Box::pin(hold_live(
        &shared,
        &transport.http,
        &provider,
        &message,
        SourceKind::LiveMessage,
        None,
    ));
    let command = tokio::select! {
        command = receiver.recv() => command.unwrap(),
        result = &mut pending => panic!("live input ended before pending acknowledgement: {}", result.is_ok()),
        () = transport.state.entered.notified() => panic!("actual reply/download HTTP began before original pending"),
    };
    assert!(
        transport.state.calls.lock().unwrap().is_empty(),
        "pending precedes all source-policy/reply/download HTTP in this branch"
    );
    let SupervisorCmd::PendingSource {
        sources,
        reply: ack,
    } = command
    else {
        panic!("the original source must be the first supervisor command")
    };
    assert_eq!(sources, [message.id.get()]);
    ack.send(Ok(registration
        .admission_order_for_test()
        .admit_pending(&sources)
        .unwrap()))
        .unwrap();
    if cancel {
        tokio::select! { () = transport.state.entered.notified() => {}, result = &mut pending => panic!("live source did not pause at real HTTP: {}", result.is_ok()) }
        drop(pending);
    } else {
        assert!(
            pending.await.is_err(),
            "real HTTP failure never becomes a successful admission"
        );
    }
    assert_eq!(serde_json::to_value(&message).unwrap(), unchanged);
    assert_eq!(
        overflow.generation(),
        generation + 1,
        "the live guard preserves the same catch-up obligation"
    );
    assert_eq!(
        registration
            .admission_order_for_test()
            .pending_snapshot()
            .sources,
        [message.id.get()]
    );
    assert_eq!(Ledger::probe(root, channel), Presence::Absent);
    assert!(shared.last_message_ids.get(&message.channel_id).is_none());
    assert!(
        matches!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "reply/download IO never sends commit, external submission or success acknowledgement"
    );
    assert_eq!(
        runtime::runtime::registry().population_for_test(),
        population,
        "IO never constructs a runtime or closing"
    );
    let calls = transport.state.calls.lock().unwrap();
    assert!(
        calls.iter().all(|(method, _)| method == Method::GET),
        "no success ACK or mutation HTTP"
    );
    if reply {
        assert!(calls.iter().any(|(_, path)| path.contains("/messages/")));
    } else {
        assert_eq!(transport.state.downloads.load(Ordering::SeqCst), 1);
    }
    drop(calls);
    let slot = registration.slot().unwrap();
    assert!(registration.release(slot));
}

#[test]
fn c2b_actual_reply_and_download_cancellation_keep_original_pending_without_success_effects() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let config = root.path().join("voice-fixture.yml");
    let fixture: crate::config::Config =
        serde_json::from_value(json!({"server": {}, "agents": []})).unwrap();
    std::fs::write(&config, serde_yaml::to_string(&fixture).unwrap()).unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    run_hold_case(root.path(), 6_328_420, Mode::PauseReply, true);
    run_hold_case(root.path(), 6_328_421, Mode::PauseDownload, true);
}

#[test]
fn c2b_actual_reply_and_download_failure_keep_original_pending_without_success_effects() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let config = root.path().join("voice-fixture.yml");
    let fixture: crate::config::Config =
        serde_json::from_value(json!({"server": {}, "agents": []})).unwrap();
    std::fs::write(&config, serde_yaml::to_string(&fixture).unwrap()).unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    run_hold_case(root.path(), 6_328_422, Mode::FailReply, false);
    run_hold_case(root.path(), 6_328_423, Mode::FailDownload, false);
}

#[test]
fn c2b_actual_enrich_fetches_two_attachments_and_retains_whole_source_after_kind_labeling() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let channel = 6_328_424;
    let runtime = runtime();
    let transport = runtime.block_on(Transport::new(channel, Mode::Complete));
    let _download = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_C2B_TEST_ATTACHMENT_URL",
        std::ffi::OsStr::new(&transport.download),
    );
    runtime.block_on(async {
        let message = original(channel, true, 2);
        let unchanged = serde_json::to_value(&message).unwrap();
        let material = enrich(
            draft(&message, channel, "transformed prompt".into()),
            &message,
            &transport.http,
        )
        .await
        .unwrap();
        let mut expected = SourceDraft {
            origin: Origin::Message {
                message_id: message.id.get(),
                source_ids: vec![message.id.get()],
                author_id: message.author.id.get(),
                original_channel_id: message.channel_id.get(),
                execution_channel_id: channel,
            },
            author_is_bot: message.author.bot,
            original_text: message.content.clone(),
            prompt: "transformed prompt".into(),
            reply_context: Some(serde_json::to_string(&transport.state.reply).unwrap()),
            attachments: message
                .attachments
                .iter()
                .map(|attachment| StagedAttachment {
                    filename: attachment.filename.clone(),
                    url: attachment.url.clone(),
                    staged: None,
                })
                .collect(),
            voice_announcement: None,
        };
        for (attachment, bytes) in expected
            .attachments
            .iter_mut()
            .zip([b"first bytes".as_slice(), b"second bytes".as_slice()])
        {
            attachment.staged = Some(
                crate::services::cluster::attachment_transfer::AttachmentEntryV1 {
                    filename: attachment.filename.clone(),
                    sha256: hex::encode(Sha256::digest(bytes)),
                    bytes: bytes.into(),
                },
            );
        }
        let expected =
            admit::prepare(SourceKind::RawAttachment, expected, Some("inline_reply")).unwrap();
        let prepared =
            admit::prepare(SourceKind::RawAttachment, material, Some("inline_reply")).unwrap();
        assert_eq!(prepared.input()["original_text"], message.content);
        assert_eq!(prepared.input()["text"], "transformed prompt");
        assert_eq!(prepared.input()["source_kind"], "raw_attachment");
        assert_eq!(prepared.input()["message_kind"], "inline_reply");
        assert_eq!(prepared.into_parts(), expected.into_parts());
        assert_eq!(transport.state.downloads.load(Ordering::SeqCst), 2);
        assert_eq!(serde_json::to_value(&message).unwrap(), unchanged);
        assert!(
            transport
                .state
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|(method, _)| method == Method::GET)
        );
        assert_eq!(Ledger::probe(root.path(), channel), Presence::Absent);
    });
}
