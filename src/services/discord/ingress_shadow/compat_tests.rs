//! The ingress shadow beside the production event handler under real Poise dispatch: off, and on
//! with an answering, timing-out, full or closed observer, every run leaves the same effects.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::services::discord::ingress_shadow::shadow_tests::{Observer, Probe, observer};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{Data, SharedData, mailbox_snapshot};
use crate::services::provider::{CancelToken, ProviderKind};

const GUILD: u64 = 96_571_300_999;
/// The agent channel this runtime's provider owns.
const CHANNEL: u64 = 940_571_300_000_010;
/// A thread whose parent is [`CHANNEL`].
const THREAD: u64 = 940_571_300_000_011;
/// A guild channel no agent of this provider owns.
const FOREIGN: u64 = 940_571_300_000_020;
/// The owner's direct-message channel with the bot.
const DM: u64 = 940_571_300_000_030;
const USER: u64 = 940_571_300_000_002;
const BOT: u64 = 940_487_400_000_003;
const APPLICATION: u64 = 940_571_300_000_077;
const MESSAGE: u64 = 940_571_300_000_101;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Case {
    Message,
    Thread,
    Duplicate,
    Queued,
    ThreadQueued,
    Image,
    File,
    AttachmentOnly,
    PendingDmReply,
    ModelPicker,
    IdleRecap,
    NodePicker,
    Sidecar,
    HelpAllowed,
    HelpRefused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Run {
    Off,
    On(ObserverKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum ObserverKind {
    Answering,
    TimingOut,
    Full,
    Closed,
}

impl ObserverKind {
    const ALL: [Self; 4] = [Self::Answering, Self::TimingOut, Self::Full, Self::Closed];

    fn observer(self) -> Observer {
        match self {
            Self::Answering => Observer::Answering,
            Self::TimingOut => Observer::TimingOut,
            Self::Full => Observer::Full,
            Self::Closed => Observer::Closed,
        }
    }
}

/// Everything a run leaves behind that the shadow must not change.
/// Method, path with query, and raw body of one Discord request.
type Call = (String, String, Vec<u8>);

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Effects {
    results: Vec<String>,
    /// Every Discord request, in arrival order.
    http: Vec<Call>,
    /// Every file under the runtime root: provider launches, queues and uploads included.
    files: BTreeMap<String, Vec<u8>>,
    queue: String,
    checkpoint: Option<u64>,
    session: Value,
    /// The pending DM reply row, when the case has one.
    dm_reply: Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct Outcome {
    effects: Effects,
    submitted: u64,
    completed: u64,
    dropped: u64,
}

#[derive(Clone, Default)]
struct Recorder {
    calls: Arc<Mutex<Vec<Call>>>,
}

fn user_json() -> Value {
    json!({"id": USER.to_string(), "username": "shadow-user", "discriminator": "0"})
}

fn channel_json(id: u64) -> Value {
    if id == DM {
        return json!({"id": DM.to_string(), "type": 1, "recipients": [user_json()]});
    }
    let mut channel = json!({
        "id": id.to_string(), "type": 0, "guild_id": GUILD.to_string(), "name": "shadow-cc",
        "position": 0, "permission_overwrites": [], "nsfw": false, "parent_id": null
    });
    if id == THREAD {
        channel["type"] = json!(11);
        channel["parent_id"] = json!(CHANNEL.to_string());
        channel["name"] = json!("shadow-thread");
    }
    channel
}

async fn discord(State(recorder): State<Recorder>, request: Request<Body>) -> Response {
    let method = request.method().clone();
    let target = request
        .uri()
        .path_and_query()
        .map_or_else(String::new, ToString::to_string);
    let path = request.uri().path().to_owned();
    let body = axum::body::to_bytes(request.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    recorder
        .calls
        .lock()
        .unwrap()
        .push((method.to_string(), target, body.clone()));
    let channel = path
        .strip_prefix("/api/v10/channels/")
        .and_then(|rest| rest.split('/').next())
        .and_then(|id| id.parse::<u64>().ok());
    if method == Method::GET
        && let Some(channel) = channel
        && path.ends_with(&channel.to_string())
    {
        return axum::Json(channel_json(channel)).into_response();
    }
    if method == Method::GET && path.ends_with("/messages") {
        return axum::Json(json!([])).into_response();
    }
    if method == Method::PUT || method == Method::DELETE {
        return StatusCode::NO_CONTENT.into_response();
    }
    if method == Method::POST && path.ends_with("/callback") {
        return StatusCode::NO_CONTENT.into_response();
    }
    if (method == Method::POST || method == Method::PATCH)
        && (path.ends_with("/messages") || path.starts_with("/api/v10/webhooks/"))
    {
        let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if body["content"] == "..." {
            return (
                StatusCode::FORBIDDEN,
                axum::Json(json!({"message": "fixture refuses provider start", "code": 50001})),
            )
                .into_response();
        }
        let mut reply = discord_mock::user_message(MESSAGE + 900, "");
        reply.channel_id = serenity::ChannelId::new(channel.unwrap_or(CHANNEL));
        reply.content = body["content"].as_str().unwrap_or_default().to_owned();
        reply.author.bot = true;
        reply.author.id = serenity::UserId::new(BOT);
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
    manager: Arc<serenity::ShardManager>,
    recorder: Recorder,
    servers: [tokio::task::JoinHandle<()>; 2],
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.servers.iter().for_each(tokio::task::JoinHandle::abort);
    }
}

struct IdleFramework;

#[async_trait::async_trait]
impl serenity::framework::Framework for IdleFramework {
    async fn dispatch(&self, _ctx: serenity::Context, _event: serenity::FullEvent) {}
}

impl Transport {
    async fn start() -> Self {
        let (_, gateway_url, gateway) =
            discord_mock::start(discord_mock::DiscordMockState::new()).await;
        let recorder = Recorder::default();
        let app = axum::Router::new()
            .fallback(discord)
            .with_state(recorder.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let ctx = discord_mock::serenity_context(proxy, gateway_url.clone()).await;
        ctx.http
            .set_application_id(serenity::ApplicationId::new(APPLICATION));
        // Poise needs a manager in its context; nothing here starts a shard.
        let framework: Arc<dyn serenity::framework::Framework> = Arc::new(IdleFramework);
        let (manager, _) =
            serenity::gateway::ShardManager::new(serenity::gateway::ShardManagerOptions {
                data: ctx.data.clone(),
                event_handlers: vec![],
                raw_event_handlers: vec![],
                framework: Arc::new(std::sync::OnceLock::from(framework)),
                shard_index: 0,
                shard_init: 0,
                shard_total: 1,
                voice_manager: None,
                ws_url: Arc::new(tokio::sync::Mutex::new(gateway_url)),
                cache: ctx.cache.clone(),
                http: ctx.http.clone(),
                intents: serenity::GatewayIntents::empty(),
                presence: None,
            });
        Self {
            ctx,
            manager,
            recorder,
            servers: [http, gateway],
        }
    }

    /// The requests so far, once none arrived for a while: handler tasks finish their posts.
    async fn settled(&self) -> Vec<Call> {
        let mut seen = usize::MAX;
        for _ in 0..50 {
            let now = self.recorder.calls.lock().unwrap().len();
            if now == seen {
                break;
            }
            seen = now;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        self.recorder.calls.lock().unwrap().clone()
    }
}

/// Wall-clock stamps a run writes into local state; no seam fixes the clock, so these alone are
/// compared by presence and every other byte as written.
const CLOCK_FIELDS: [&str; 2] = ["created_at_wall_time_ms", "enqueued_at_epoch_us"];

fn without_clocks(bytes: Vec<u8>) -> Vec<u8> {
    fn strip(value: &mut Value) -> bool {
        match value {
            Value::Object(map) => map.iter_mut().fold(false, |hit, (key, inner)| {
                if CLOCK_FIELDS.contains(&key.as_str()) && !inner.is_null() {
                    *inner = json!("<clock>");
                    true
                } else {
                    strip(inner) || hit
                }
            }),
            Value::Array(items) => items.iter_mut().fold(false, |hit, item| strip(item) || hit),
            _ => false,
        }
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return bytes;
    };
    if strip(&mut value) {
        serde_json::to_vec_pretty(&value).unwrap()
    } else {
        bytes
    }
}

fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, entries: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, entries);
            } else {
                let name = path.strip_prefix(root).unwrap().display().to_string();
                entries.insert(name, without_clocks(std::fs::read(path).unwrap()));
            }
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

fn message(channel: u64, id: u64, content: &str) -> serenity::Message {
    let mut message = discord_mock::user_message(id, content);
    message.channel_id = serenity::ChannelId::new(channel);
    message.guild_id = (channel != DM).then_some(serenity::GuildId::new(GUILD));
    message.author.id = serenity::UserId::new(USER);
    message.author.name = "shadow-user".into();
    message
}

fn attached(mut message: serenity::Message, name: &str, kind: &str) -> serenity::Message {
    let attachment = json!({
        "id": (MESSAGE + 50).to_string(), "filename": name, "size": 11,
        "url": format!("https://cdn.example.invalid/attachments/{CHANNEL}/{name}"),
        "proxy_url": format!("https://media.example.invalid/{name}"), "content_type": kind
    });
    message.attachments = vec![serde_json::from_value(attachment).unwrap()];
    message
}

fn interaction(kind: u8, channel: u64, custom_id: &str) -> serenity::FullEvent {
    let mut json = json!({
        "id": (MESSAGE + 60).to_string(), "application_id": APPLICATION.to_string(),
        "type": kind, "token": "shadow-token", "channel_id": channel.to_string(),
        "guild_id": GUILD.to_string(), "version": 1, "locale": "en-US", "app_permissions": "0",
        "member": {"user": user_json(), "roles": [], "joined_at": "2026-10-13T00:00:00Z",
            "deaf": false, "mute": false, "flags": 0, "permissions": "0"},
        "authorizing_integration_owners": {}, "entitlements": [], "attachment_size_limit": 26214400
    });
    if kind == 3 {
        json["data"] = json!({"custom_id": custom_id, "component_type": 2});
        let mut card = discord_mock::user_message(MESSAGE + 61, "card");
        card.channel_id = serenity::ChannelId::new(channel);
        card.author.id = serenity::UserId::new(BOT);
        card.author.bot = true;
        json["message"] = serde_json::to_value(card).unwrap();
    } else {
        json["data"] = json!({"id": (MESSAGE + 62).to_string(), "name": custom_id, "type": 1});
    }
    let interaction = serde_json::from_value(json).expect("interaction fixture");
    serenity::FullEvent::InteractionCreate { interaction }
}

fn events(case: Case) -> Vec<serenity::FullEvent> {
    let text = |channel| message(channel, MESSAGE, "  shadow live input  ");
    let new = |new_message| serenity::FullEvent::Message { new_message };
    match case {
        Case::Message | Case::Queued => vec![new(text(CHANNEL))],
        Case::Thread | Case::ThreadQueued => vec![new(text(THREAD))],
        Case::Duplicate => vec![new(text(CHANNEL)), new(text(CHANNEL))],
        Case::Image => vec![new(attached(text(CHANNEL), "shot.png", "image/png"))],
        Case::File => vec![new(attached(text(CHANNEL), "notes.txt", "text/plain"))],
        Case::AttachmentOnly => {
            let only = message(CHANNEL, MESSAGE, "");
            vec![new(attached(only, "only.png", "image/png"))]
        }
        Case::PendingDmReply => vec![new(message(DM, MESSAGE, "yes, ship it"))],
        Case::ModelPicker => vec![interaction(3, CHANNEL, "agentdesk:model-picker")],
        Case::IdleRecap => vec![interaction(3, CHANNEL, "idle_recap:clear:0")],
        Case::NodePicker => vec![interaction(3, CHANNEL, "agentdesk:node-picker")],
        Case::Sidecar => vec![interaction(3, CHANNEL, "sidecar:cancel")],
        Case::HelpAllowed => vec![interaction(2, CHANNEL, "help")],
        Case::HelpRefused => vec![interaction(2, FOREIGN, "help")],
    }
}

async fn session(shared: &SharedData) -> Value {
    let core = shared.core.lock().await;
    let channel = serenity::ChannelId::new(CHANNEL);
    core.sessions.get(&channel).map_or(Value::Null, |session| {
        json!({
            "session_id": session.session_id, "current_path": session.current_path,
            "history": format!("{:?}", session.history), "uploads": session.pending_uploads,
            "cleared": session.cleared
        })
    })
}

fn generations(item: &crate::services::turn_orchestrator::Intervention) -> Vec<String> {
    let generation = |g: &crate::services::turn_orchestrator::SourceMessageQueuedGeneration| {
        let stamped = g.enqueued_at_epoch_us.is_some();
        format!(
            "{:?}",
            (
                g.message_id,
                g.queued_generation,
                stamped,
                g.preserve_on_cancel
            )
        )
    };
    item.source_message_queued_generations
        .iter()
        .map(generation)
        .collect()
}

/// The queued inputs field by field; arrival instants and wall stamps only by presence.
fn queued(queue: &[crate::services::turn_orchestrator::Intervention]) -> String {
    let item = |i: &crate::services::turn_orchestrator::Intervention| {
        format!(
            "{:?}",
            (
                (
                    i.author_id,
                    i.author_is_bot,
                    i.message_id,
                    i.queued_generation
                ),
                (&i.source_message_ids, generations(i)),
                (&i.source_text_segments, &i.text, &i.mode, &i.reply_context),
                (
                    i.has_reply_boundary,
                    i.merge_consecutive,
                    &i.pending_uploads
                ),
                &i.voice_announcement,
            )
        )
    };
    queue.iter().map(item).collect::<Vec<_>>().join("\n")
}

async fn dm_reply(pool: &sqlx::PgPool) -> Value {
    let row: (String, String, Option<String>, String, String, bool) = sqlx::query_as(
        "SELECT source_agent, user_id, channel_id, context::text, status, consumed_at IS NOT NULL
         FROM pending_dm_replies",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    json!(row)
}

/// Errors the handler gave Poise, in order; the production handler only logs them.
static HANDLER_ERRORS: Mutex<Vec<String>> = Mutex::new(Vec::new());

async fn record_error(error: poise::FrameworkError<'_, Data, crate::services::discord::Error>) {
    let inner = match &error {
        poise::FrameworkError::EventHandler { error, .. }
        | poise::FrameworkError::Command { error, .. } => format!(": {error}"),
        _ => String::new(),
    };
    HANDLER_ERRORS
        .lock()
        .unwrap()
        .push(format!("{error}{inner}"));
}

/// One run inside a child process, given the shared state its test built.
type Runner = fn(Case, Run, PathBuf) -> futures::future::LocalBoxFuture<'static, Outcome>;

fn without_storage(
    case: Case,
    run: Run,
    root: PathBuf,
) -> futures::future::LocalBoxFuture<'static, Outcome> {
    Box::pin(async move {
        let shared = crate::services::discord::make_shared_data_for_tests();
        invoke(case, run, &root, shared).await
    })
}

/// The run with a database holding one pending DM reply from the owner, read back afterwards.
fn with_pending_dm_reply(
    case: Case,
    run: Run,
    root: PathBuf,
) -> futures::future::LocalBoxFuture<'static, Outcome> {
    Box::pin(async move {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let pending = crate::services::discord_dm_reply_store::register_pending_dm_reply_db(
            Some(&pool),
            "shadow-compat",
            &USER.to_string(),
            Some(&CHANNEL.to_string()),
            r#"{"question":"ship?"}"#,
            0,
        )
        .await;
        assert!(pending.is_ok(), "fixture registers a pending DM reply");
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let mut outcome = invoke(case, run, &root, shared).await;
        outcome.effects.dm_reply = dm_reply(&pool).await;
        pool.close().await;
        db.drop().await;
        outcome
    })
}

/// One run in this process: the fixture, every event through Poise dispatch with the production
/// framework options, and what it left behind.
async fn invoke(case: Case, run: Run, root: &Path, shared: Arc<SharedData>) -> Outcome {
    let channel = serenity::ChannelId::new(CHANNEL);
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(USER);
        settings.allow_all_users = true;
        settings.allowed_channel_ids = vec![CHANNEL, THREAD];
    }
    crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "shadow-cc")
        .await;
    {
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.current_path = Some(root.join("workspace").display().to_string());
    }
    let busy = match case {
        Case::Queued => Some(channel),
        Case::ThreadQueued => Some(serenity::ChannelId::new(THREAD)),
        _ => None,
    };
    if let Some(busy) = busy {
        let claimed = shared
            .mailbox(busy)
            .try_start_turn(
                Arc::new(CancelToken::new()),
                serenity::UserId::new(USER),
                serenity::MessageId::new(MESSAGE - 1),
            )
            .await;
        assert!(claimed, "fixture holds a real active turn");
    }
    let transport = Transport::start().await;
    assert!(
        shared
            .http
            .cached_serenity_ctx
            .set(transport.ctx.clone())
            .is_ok()
    );
    shared.turn_view_reconciler.enable_test_http_reactions();
    assert!(
        crate::services::discord::provider_handles_channel(
            &transport.ctx,
            &ProviderKind::Claude,
            &shared.settings.read().await.clone(),
            channel
        )
        .await,
        "fixture passes provider routing"
    );
    transport.recorder.calls.lock().unwrap().clear();

    let (ingress_shadow, probe): (_, Option<Probe>) = match run {
        Run::Off => (None, None),
        Run::On(kind) => {
            let (shadow, probe) = observer(kind.observer(), &[CHANNEL, THREAD, FOREIGN, DM]);
            (Some(shadow), Some(probe))
        }
    };
    let mut voice_config = crate::voice::VoiceConfig::default();
    voice_config.audio.recordings_dir = root.join("recordings");
    let data = Data {
        shared: shared.clone(),
        token: "test-token".into(),
        provider: ProviderKind::Claude,
        voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
        voice_config,
        ingress_shadow,
    };
    let mut options = crate::services::discord::runtime_bootstrap::run_bot_framework_options(
        crate::services::discord::runtime_bootstrap::run_bot_build_slash_commands(),
    );
    poise::set_qualified_names(&mut options.commands);
    options.on_error = |error| Box::pin(record_error(error));
    let (options, data) = (Arc::new(options), Arc::new(data));
    let mut results = Vec::new();
    for event in events(case) {
        let (options, data) = (Arc::clone(&options), Arc::clone(&data));
        let (ctx, manager) = (transport.ctx.clone(), Arc::clone(&transport.manager));
        // Its own task, as the gateway runs each dispatch, so the handler's frames start fresh.
        let dispatched = tokio::spawn(async move {
            let framework = poise::FrameworkContext {
                bot_id: serenity::UserId::new(BOT),
                options: &options,
                user_data: &*data,
                shard_manager: &manager,
            };
            poise::dispatch_event(framework, &ctx, event).await;
        });
        let finished = tokio::time::timeout(Duration::from_secs(20), dispatched).await;
        results.push(format!("{finished:?}"));
    }
    let http = transport.settled().await;
    results.extend(HANDLER_ERRORS.lock().unwrap().drain(..));
    if let Some(probe) = &probe
        && run == Run::On(ObserverKind::Answering)
    {
        assert!(
            probe
                .judged(probe.submitted(), Duration::from_secs(5))
                .await
        );
    }
    let snapshot = mailbox_snapshot(&shared, channel).await;
    let effects = Effects {
        results,
        http,
        files: files(root),
        queue: queued(&snapshot.intervention_queue),
        checkpoint: shared.last_message_ids.get(&channel).map(|entry| *entry),
        session: session(&shared).await,
        dm_reply: Value::Null,
    };
    let (submitted, completed, dropped) = probe.as_ref().map_or((0, 0, 0), |probe| {
        (probe.submitted(), probe.completed(), probe.dropped())
    });
    shared.mailboxes.remove_fixture_for_test(channel);
    drop(data);
    Outcome {
        effects,
        submitted,
        completed,
        dropped,
    }
}

const RUN_ENV: &str = "ADK_INGRESS_SHADOW_COMPAT_RUN";
const OUT_ENV: &str = "ADK_INGRESS_SHADOW_COMPAT_OUT";

fn qualified(test: &str) -> String {
    format!("{}::{test}", module_path!().split_once("::").unwrap().1)
}

/// The same path for every run of a test, so the paths written into effects match.
fn root_of(test: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/ingress-shadow-compat")
        .join(test)
}

/// In a child process: runs the one case and run its parent named, and writes the outcome.
fn child(test: &str, runner: Runner) -> bool {
    let Ok(spec) = std::env::var(RUN_ENV) else {
        return false;
    };
    let (case, run): (Case, Run) = serde_json::from_str(&spec).unwrap();
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = std::fs::canonicalize(root_of(test)).unwrap();
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _root = set("AGENTDESK_ROOT_DIR", &root);
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    for name in ["tmux", "claude", "codex"] {
        let binary = bin.join(name);
        let script = "#!/bin/sh\nprintf '%s %s\\n' \"$0\" \"$*\" >> \"$ADK_SHADOW_TEST_PROCESS_LOG\"\nexit 97\n";
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let _log = set("ADK_SHADOW_TEST_PROCESS_LOG", &root.join("process-calls"));
    let _path = crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(&bin);
    let _claude = set("AGENTDESK_CLAUDE_PATH", &bin.join("claude"));
    // The root sits inside this checkout; git must not find it from the workspace.
    let _git = set("GIT_CEILING_DIRECTORIES", &root);
    let config_path = crate::runtime_layout::config_file_path(&root);
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::create_dir(root.join("workspace")).unwrap();
    let agent = json!({
        "id": "shadow-compat", "name": "Shadow compat", "voice_enabled": false, "provider": "claude",
        "channels": {"claude": {"id": CHANNEL.to_string(), "name": "shadow-cc",
            "workspace": root.join("workspace"), "runtime": "tui"}}
    });
    let config = json!({"server": {}, "data": {"dir": root.join("data")}, "agents": [agent]});
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let _config = set("AGENTDESK_CONFIG", &config_path);
    assert_eq!(crate::config::resolved_config_path(), config_path);
    // Debug builds of the gateway handler future outgrow the default test thread stack.
    let child_root = root.clone();
    let outcome = std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(runner(case, run, child_root))
        })
        .unwrap()
        .join()
        .unwrap();
    std::fs::write(
        std::env::var(OUT_ENV).unwrap(),
        serde_json::to_vec(&outcome).unwrap(),
    )
    .unwrap();
    true
}

/// In the parent: one child process per run, each on a fresh root at the same path.
fn spawn_run(test: &str, case: Case, run: Run) -> Outcome {
    let root = root_of(test);
    if root.exists() {
        std::fs::remove_dir_all(&root).unwrap();
    }
    std::fs::create_dir_all(&root).unwrap();
    let out = root.with_extension("outcome.json");
    let _ = std::fs::remove_file(&out);
    let child = {
        // Spawned under the env lock, so no other test's temporary variables reach the child.
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &qualified(test), "--nocapture"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env(RUN_ENV, serde_json::to_string(&(case, run)).unwrap())
            .env(OUT_ENV, &out)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{case:?} {run:?}: {stderr}\n{stdout}"
    );
    serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap()
}

fn clip(text: String) -> String {
    text.chars().take(400).collect()
}

/// Every field two runs disagree on, each named narrowly enough to read.
fn differences(left: &Effects, right: &Effects) -> Vec<String> {
    let mut found = Vec::new();
    if left.results != right.results {
        found.push(format!("results {:?} vs {:?}", left.results, right.results));
    }
    let shown = |call: &Call| format!("{} {} {}", call.0, call.1, String::from_utf8_lossy(&call.2));
    for index in 0..left.http.len().max(right.http.len()) {
        let (l, r) = (left.http.get(index), right.http.get(index));
        if l != r {
            found.push(clip(format!(
                "http #{index}: {:?} vs {:?}",
                l.map(shown),
                r.map(shown)
            )));
        }
    }
    let names: std::collections::BTreeSet<_> =
        left.files.keys().chain(right.files.keys()).collect();
    for name in names {
        let (l, r) = (left.files.get(name), right.files.get(name));
        if l != r {
            let text =
                |bytes: Option<&Vec<u8>>| bytes.map(|b| String::from_utf8_lossy(b).into_owned());
            found.push(clip(format!("file {name}: {:?} vs {:?}", text(l), text(r))));
        }
    }
    for (field, l, r) in [
        ("queue", json!(left.queue), json!(right.queue)),
        (
            "checkpoint",
            json!(left.checkpoint),
            json!(right.checkpoint),
        ),
        ("session", left.session.clone(), right.session.clone()),
        ("dm_reply", left.dm_reply.clone(), right.dm_reply.clone()),
    ] {
        if l != r {
            found.push(clip(format!("{field}: {l} vs {r}")));
        }
    }
    found
}

fn writes(effects: &Effects) -> Vec<(&str, &str)> {
    let calls = effects.http.iter().filter(|call| call.0 != "GET");
    calls
        .map(|call| (call.0.as_str(), call.1.as_str()))
        .collect()
}

/// What the shadow-off run must have done, so two empty runs never agree by accident.
fn expect_off(case: Case, off: &Effects) -> Vec<String> {
    let writes = writes(off);
    let callbacks = writes
        .iter()
        .filter(|(_, path)| path.ends_with("/callback"))
        .count();
    let followups = writes
        .iter()
        .filter(|(_, path)| path.starts_with("/api/v10/webhooks/"))
        .count();
    let reacted = writes
        .iter()
        .any(|(method, path)| *method == "PUT" && path.contains("/reactions/"));
    // The queued text as written to the durable queue: the input's payload bytes.
    let durable = |channel: u64| {
        let name = format!("runtime/discord_pending_queue/claude/test-token-hash/{channel}.json");
        let bytes = off
            .files
            .get(&name)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
        bytes.is_some_and(|text| text.contains("\"text\": \"shadow live input\""))
    };
    let held = match case {
        Case::Queued => {
            reacted
                && durable(CHANNEL)
                && off.checkpoint == Some(MESSAGE)
                && off.queue.contains("shadow live input")
        }
        Case::ThreadQueued => reacted && durable(THREAD),
        Case::PendingDmReply => {
            off.dm_reply[4] == "consumed"
                && off.dm_reply[3]
                    .as_str()
                    .is_some_and(|context| context.contains("yes, ship it"))
        }
        Case::ModelPicker | Case::IdleRecap | Case::NodePicker | Case::Sidecar => callbacks == 1,
        Case::HelpAllowed => {
            (callbacks, followups) == (1, 1)
                && writes.first().is_some_and(|w| w.1.ends_with("/callback"))
        }
        Case::HelpRefused => (callbacks, followups) == (0, 0),
        Case::Message
        | Case::Thread
        | Case::Duplicate
        | Case::Image
        | Case::File
        | Case::AttachmentOnly => {
            !writes.is_empty() || off.results.iter().any(|r| r.contains("event handler"))
        }
    };
    if held {
        return Vec::new();
    }
    let state = json!({"results": off.results, "queue": off.queue, "checkpoint": off.checkpoint, "dm_reply": off.dm_reply});
    vec![clip(format!(
        "{case:?}: the off run lacks its expected effect: {writes:?} {state}"
    ))]
}

/// Every case off twice, then on with each observer: the shadow took one submission per raw event,
/// the answering one judged them all, and every effect equals the off run.
fn compare(test: &str, cases: &[Case], runner: Runner) {
    if child(test, runner) {
        return;
    }
    let mut problems = Vec::new();
    for &case in cases {
        let off = spawn_run(test, case, Run::Off);
        let again = spawn_run(test, case, Run::Off);
        let unstable = differences(&off.effects, &again.effects);
        problems.extend(unstable.iter().map(|d| format!("{case:?} off twice: {d}")));
        problems.extend(expect_off(case, &off.effects));
        assert_eq!((off.submitted, off.completed, off.dropped), (0, 0, 0));
        let events = events(case).len() as u64;
        for kind in ObserverKind::ALL {
            let on = spawn_run(test, case, Run::On(kind));
            if kind == ObserverKind::Answering {
                assert_eq!(
                    on.completed, events,
                    "{case:?}: the answering observer judged each event"
                );
            }
            assert_eq!(
                on.submitted, events,
                "{case:?} {kind:?}: one submission per raw event"
            );
            let dropped = match kind {
                ObserverKind::Answering | ObserverKind::TimingOut => 0,
                ObserverKind::Full | ObserverKind::Closed => events,
            };
            assert_eq!(on.dropped, dropped, "{case:?} {kind:?}");
            let changed = differences(&off.effects, &on.effects);
            problems.extend(
                changed
                    .iter()
                    .map(|d| format!("{case:?} {kind:?}: the shadow changed {d}")),
            );
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn ingress_shadow_keeps_live_messages_byte_identical() {
    compare(
        "ingress_shadow_keeps_live_messages_byte_identical",
        &[
            Case::Message,
            Case::Thread,
            Case::Duplicate,
            Case::Queued,
            Case::ThreadQueued,
        ],
        without_storage,
    );
}

#[test]
fn ingress_shadow_keeps_attachment_messages_byte_identical() {
    compare(
        "ingress_shadow_keeps_attachment_messages_byte_identical",
        &[Case::Image, Case::File, Case::AttachmentOnly],
        without_storage,
    );
}

#[test]
fn ingress_shadow_keeps_components_byte_identical() {
    compare(
        "ingress_shadow_keeps_components_byte_identical",
        &[
            Case::ModelPicker,
            Case::IdleRecap,
            Case::NodePicker,
            Case::Sidecar,
        ],
        without_storage,
    );
}

#[test]
fn ingress_shadow_keeps_poise_commands_byte_identical() {
    compare(
        "ingress_shadow_keeps_poise_commands_byte_identical",
        &[Case::HelpAllowed, Case::HelpRefused],
        without_storage,
    );
}

#[test]
fn ingress_shadow_keeps_a_pending_dm_reply_byte_identical_pg() {
    compare(
        "ingress_shadow_keeps_a_pending_dm_reply_byte_identical_pg",
        &[Case::PendingDmReply],
        with_pending_dm_reply,
    );
}
