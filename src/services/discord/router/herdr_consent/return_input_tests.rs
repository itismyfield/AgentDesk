//! Owner return and retry paths through the real consent store, approval store and mailbox.
use super::*;
use crate::config::session_hosts::{
    BootSessionHosts, ForcedSessionHosts,
    approval_store::{ApprovalStore as _, FileApprovalStore},
    migration,
};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{IdleQueueKickoffChannelOutcome, queue_io};
use crate::services::turn_orchestrator::{Intervention, InterventionMode, QueuePersistenceContext};
use serde_json::json;
use std::sync::Mutex;

const CHANNEL: u64 = discord_mock::CHANNEL_ID;
const BOT: u64 = 940_487_400_000_003;
const SOCKET: &str = "/adk/herdr/agentdesk.sock";
type Calls = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

#[derive(Clone)]
struct Http(Calls);
async fn http(
    axum::extract::State(Http(calls)): axum::extract::State<Http>,
    request: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let (path, method) = (request.uri().path().to_owned(), request.method().clone());
    let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    calls.lock().unwrap().push((path.clone(), body.clone()));
    if path.ends_with("/callback") {
        return axum::http::StatusCode::NO_CONTENT.into_response();
    }
    if method == axum::http::Method::GET && path.ends_with("/users/@me") {
        let mut user = serde_json::to_value(discord_mock::user_message(1, "bot").author).unwrap();
        (user["id"], user["bot"]) = (json!(BOT.to_string()), json!(true));
        return axum::Json(user).into_response();
    }
    if method == axum::http::Method::GET && path.ends_with(&format!("/channels/{CHANNEL}")) {
        return axum::Json(json!({"id":CHANNEL.to_string(),"type":0,"guild_id":"96326000999","name":"return-consent-cc","position":0,"permission_overwrites":[],"nsfw":false,"parent_id":null})).into_response();
    }
    let mut response =
        serde_json::to_value(discord_mock::user_message(940_487_400_000_051, "reply")).unwrap();
    response["content"] = body.get("content").cloned().unwrap_or_default();
    if let Some(components) = body.get("components") {
        response["components"] = components.clone();
    }
    axum::Json(response).into_response()
}

struct Fixture {
    runtime: tempfile::TempDir,
    workspace: tempfile::TempDir,
    _env: (
        crate::config::TestEnvVarGuard,
        crate::config::TestEnvVarGuard,
    ),
    ctx: serenity::Context,
    data: Data,
    calls: Calls,
    servers: [tokio::task::JoinHandle<()>; 2],
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.servers.iter().for_each(tokio::task::JoinHandle::abort);
    }
}

impl Fixture {
    async fn new() -> Self {
        let runtime = tempfile::tempdir().unwrap();
        let root = crate::config::set_agentdesk_root_for_test(runtime.path());
        let workspace = tempfile::tempdir().unwrap();
        let path = crate::runtime_layout::config_file_path(runtime.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &path,
        );
        let (_, gateway, gateway_server) =
            discord_mock::start(discord_mock::DiscordMockState::new()).await;
        let calls = Calls::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new()
            .fallback(http)
            .with_state(Http(calls.clone()));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let ctx = discord_mock::serenity_context(proxy, gateway).await;
        let data = restarted().await;
        let fixture = Self {
            runtime,
            workspace,
            _env: (root, config),
            ctx,
            data,
            calls,
            servers: [server, gateway_server],
        };
        fixture.legacy(SOCKET);
        fixture
    }

    /// Writes the config file with a legacy binding and returns that boot candidate.
    fn legacy(&self, socket: &str) -> BootSessionHosts {
        let path = crate::runtime_layout::config_file_path(self.runtime.path());
        let config = json!({"server":{},"cluster":{"instance_id":"node"},
            "agents":[{"id":"return-consent","name":"Return fixture","provider":"claude","voice_enabled":false,
                "channels":{"claude":{"id":CHANNEL.to_string(),"name":"return-consent-cc","workspace":self.workspace.path(),"runtime":"tui"}}}],
            "session_hosts":{"herdr":{"endpoints":{"node-endpoint":{"execution_node":"node","socket_path":socket,
                "herdr_home":"/adk/herdr","herdr_session":"agentdesk"}},"channels":{CHANNEL.to_string():"node-endpoint"}}}});
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        BootSessionHosts::from_config(&crate::config::load().unwrap()).unwrap()
    }

    async fn command(&self, id: u64, text: &str) -> String {
        self.command_as(&self.data, id, text).await
    }

    /// Sends `text` through the real text-command dispatch and returns the reply.
    async fn command_as(&self, data: &Data, id: u64, text: &str) -> String {
        let message = discord_mock::user_message(id, text);
        let before = self.calls.lock().unwrap().len();
        let handled = crate::services::discord::commands::handle_text_command_with_uploads(
            &self.ctx,
            &message,
            data,
            message.channel_id,
            text,
            &[],
            &mut None,
        )
        .await;
        assert!(matches!(handled, Ok(true)), "{text}: {handled:?}");
        let calls = self.calls.lock().unwrap();
        let replies = calls[before..]
            .iter()
            .filter(|(path, body)| path.ends_with("/messages") && body.get("components").is_none());
        replies
            .filter_map(|(_, body)| body["content"].as_str())
            .collect()
    }

    fn cards(&self) -> usize {
        let calls = self.calls.lock().unwrap();
        calls
            .iter()
            .filter(|(path, body)| path.ends_with("/messages") && body.get("components").is_some())
            .count()
    }

    async fn enqueue(&self, source: u64, generation: u64) {
        let item = Intervention {
            author_id: discord_mock::user_message(1, "").author.id,
            author_is_bot: false,
            message_id: serenity::MessageId::new(source),
            queued_generation: generation,
            source_message_ids: vec![serenity::MessageId::new(source)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: format!("payload {source}"),
            mode: InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: Some(format!("reply {source}")),
            has_reply_boundary: true,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };
        let enqueued = self.mailbox().enqueue(item, self.persistence()).await;
        assert!(enqueued.enqueued && enqueued.persistence_error.is_none());
    }

    fn mailbox(&self) -> crate::services::turn_orchestrator::ChannelMailboxHandle {
        self.data.shared.mailbox(serenity::ChannelId::new(CHANNEL))
    }

    fn persistence(&self) -> QueuePersistenceContext {
        QueuePersistenceContext::new(&ProviderKind::Claude, &self.data.shared.token_hash, None)
    }

    async fn queued(&self) -> Vec<u64> {
        let queue = self.mailbox().snapshot().await.intervention_queue;
        queue.iter().map(|item| item.message_id.get()).collect()
    }
}

/// Runtime state as a fresh process has it before any queue restore.
async fn restarted() -> Data {
    let shared = crate::services::discord::make_shared_data_for_tests();
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(discord_mock::user_message(1, "").author.id.get());
        settings.allowed_channel_ids = vec![CHANNEL];
    }
    let voice_config = crate::voice::VoiceConfig::default();
    Data {
        shared,
        token: "test-token".into(),
        provider: ProviderKind::Claude,
        voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
        voice_config,
    }
}

/// Takes the head as a finished turn would; records each payload it ran.
async fn drain(shared: &Arc<SharedData>, ran: &Mutex<Vec<String>>) -> bool {
    let channel = serenity::ChannelId::new(CHANNEL);
    let context = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    let taken = shared.mailbox(channel).take_next_soft(context).await;
    let Some(item) = taken.intervention else {
        return false;
    };
    ran.lock().unwrap().push(item.text.clone());
    let lease = taken.dispatch_lease.expect("dequeue lease");
    crate::services::discord::mailbox_abandon_unclaimed_dispatch_after_success(
        shared,
        &ProviderKind::Claude,
        channel,
        item.message_id,
        lease,
    )
    .await;
    true
}

fn key(config: &std::path::Path, target: &std::path::Path) -> consent::ConsentKey {
    consent::ConsentKey {
        node: "node".into(),
        // SAFETY: geteuid has no preconditions and cannot fail.
        user: unsafe { libc::geteuid() },
        socket: SOCKET.into(),
        named_session: "agentdesk".into(),
        config: config.into(),
        target: target.into(),
    }
}

fn question(key: &consent::ConsentKey, preview: &Preview, source: u64, fifo: u64) -> Question {
    Question {
        key: key.clone(),
        preview: preview.clone(),
        owner: discord_mock::user_message(1, "").author.id.get(),
        bot: BOT,
        provider: "claude".into(),
        channel: CHANNEL,
        message: 0,
        intent: consent::IntentRef {
            store: "mailbox".into(),
            id: format!("{source}:{fifo}"),
            provider: "claude".into(),
            channel: CHANNEL,
            source_message: source,
            fifo,
        },
    }
}

use consent::{Preview, Question, settlement::Resolution};

fn preview() -> Preview {
    let mut witness = crate::services::session_host::ServerWitness::for_test(7);
    witness.socket = SOCKET.into();
    Preview {
        revision: 1,
        witness,
        target_hash: [1; 32],
        chain_hash: [2; 32],
        checker_hash: [3; 32],
    }
}

fn user_default() -> BootSessionHosts {
    let forced = session_hosts::force_for_test(Some("node"), &[(CHANNEL, "node")])
        .with_mode(EndpointMode::UserDefault);
    let hosts = session_hosts::with_boot(|boot| boot.unwrap().clone());
    drop(forced);
    hosts
}

struct Rows(bool);
impl migration::RowSource for Rows {
    async fn rows(
        &self,
        _: &std::collections::BTreeSet<u64>,
    ) -> Result<Vec<migration::MigrationRow>, String> {
        use crate::db::dispatched_sessions::hosted_execution::{self as hosted, tests};
        let mut bound = tests::record(
            &tests::owner(&CHANNEL.to_string()),
            "e",
            hosted::HostedState::Bound,
        );
        bound.location = Some(hosted::HostedLocation {
            host: "herdr".into(),
            execution_node: "node".into(),
            endpoint_config_key: "node-endpoint".into(),
            socket_addr: SOCKET.into(),
            named_session: "agentdesk".into(),
            pane_id: "pane-1".into(),
        });
        let row = migration::MigrationRow {
            id: 7,
            channel: CHANNEL,
            record: hosted::HostedRecord::Known(bound),
        };
        Ok(self.0.then_some(row).into_iter().collect())
    }
}

/// Real consent holds whose post-save adoption record is lost, as after a crash right after save.
struct LostAdoption;
impl migration::HoldSource for LostAdoption {
    const INTRODUCED: bool = true;
    async fn unresolved(
        &self,
        channels: &std::collections::BTreeSet<u64>,
    ) -> Result<Vec<u64>, String> {
        migration::ConsentHolds.unresolved(channels).await
    }
    async fn blocking(
        &self,
        channels: &std::collections::BTreeSet<u64>,
        candidate: &BootSessionHosts,
    ) -> Result<Vec<u64>, String> {
        migration::ConsentHolds.blocking(channels, candidate).await
    }
}

/// Selects and installs one boot through the real approval and consent stores.
async fn boot(
    slot: &mut Option<ForcedSessionHosts>,
    approvals: &FileApprovalStore,
    candidate: &BootSessionHosts,
    rows: bool,
    adopt: bool,
) -> Option<&'static str> {
    slot.take();
    let candidate = candidate.clone();
    let selected = if adopt {
        migration::select(
            candidate,
            approvals,
            &Rows(rows),
            &migration::ConsentHolds,
            false,
        )
        .await
    } else {
        migration::select(candidate, approvals, &Rows(rows), &LostAdoption, false).await
    };
    *slot = Some(session_hosts::force_selected_for_test(selected));
    session_hosts::with_boot_migration(|meta| {
        meta.and_then(|meta| meta.migration()).map(|d| d.reason)
    })
}

fn resolution(consent: &Consent, source: u64) -> Option<Resolution> {
    let latest = store().load(&consent.key).unwrap().unwrap();
    let intent = latest
        .intents
        .iter()
        .find(|i| i.source_message == source)
        .unwrap();
    latest.resolution(intent)
}

fn consent_for(source: u64) -> Consent {
    let consents = store().load_all().unwrap();
    let found = consents
        .into_iter()
        .find(|c| c.intents.iter().any(|i| i.source_message == source));
    found.expect("consent holding the source")
}

/// Holds `source` in the channel's consent as admission does, creating it denied when absent.
fn hold_denied(source: u64, generation: u64) -> Consent {
    let key = key(
        "/adk/return-fixture/config.toml".as_ref(),
        "/adk/return-fixture/config.toml".as_ref(),
    );
    let held = store()
        .hold(
            Question {
                message: 100,
                ..question(&key, &preview(), source, generation)
            },
            now(),
        )
        .unwrap()
        .into_consent();
    match held.state {
        State::Pending => store().decide(&held, held.owner, false, now()).unwrap(),
        _ => held,
    }
}

fn kicks_drain(ran: &Arc<Mutex<Vec<String>>>) -> queue_io::IdleQueueKickHookResetForTests {
    let ran = ran.clone();
    queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(move |shared, _, _, _| {
        let ran = ran.clone();
        Box::pin(async move {
            let started = drain(&shared, &ran).await;
            Some(IdleQueueKickoffChannelOutcome { started })
        })
    }))
}

// P1-R chain: return to a legacy binding, adoption, legacy FIFO, closure, user-default again, then a
// second return to the same or another binding. Each original runs once, in FIFO order, never
// under the user-default mapping, and a closed return never blocks the next one.
#[tokio::test(flavor = "current_thread")]
async fn returned_inputs_roll_back_twice_and_each_original_runs_once_in_fifo() {
    for (second, adopt) in [("same", true), ("other", true), ("same", false)] {
        let f = Fixture::new().await;
        let ran = Arc::new(Mutex::new(Vec::new()));
        let _kicks = kicks_drain(&ran);
        let approvals = FileApprovalStore(Some(f.runtime.path().join("approvals")));
        let user = user_default();
        approvals.save(&user).unwrap();
        let mut slot = None;
        assert_eq!(boot(&mut slot, &approvals, &user, false, true).await, None);
        let (a, b, c) = (
            940_487_400_000_301,
            940_487_400_000_302,
            940_487_400_000_303,
        );
        f.enqueue(a, 17).await;
        f.enqueue(b, 18).await;
        hold_denied(a, 17);
        let held = hold_denied(b, 18);
        let first = f.legacy(SOCKET);
        let digest = |hosts: &BootSessionHosts| migration::binding_digest(hosts.config(), CHANNEL);
        let refused = Some("session_hosts_migration_refused");
        assert_eq!(
            boot(&mut slot, &approvals, &first, false, true).await,
            refused
        );
        let reply = f.command(1, "!herdr consent return").await;
        assert!(
            reply.contains("입력 2건") && reply.contains("반환 예약"),
            "{reply}"
        );
        let reserved = Resolution::Returned {
            target: digest(&first).unwrap(),
        };
        assert_eq!(
            [resolution(&held, a), resolution(&held, b)],
            [Some(reserved); 2]
        );
        let before = store().load_all().unwrap();
        assert!(
            f.command(2, "!herdr consent return")
                .await
                .contains("입력 2건")
        );
        assert_eq!(
            store().load_all().unwrap(),
            before,
            "a repeated return is a no-op"
        );
        startup::restore(&f.ctx.http, &f.data.shared, &ProviderKind::Claude)
            .await
            .unwrap();
        assert!(
            ran.lock().unwrap().is_empty(),
            "nothing runs under the user-default mapping"
        );
        assert_eq!(
            boot(&mut slot, &approvals, &first, false, adopt).await,
            None
        );
        assert!(
            endpoint(CHANNEL).is_none(),
            "the legacy approval owns the channel"
        );
        assert_eq!(
            boot(&mut slot, &approvals, &user, false, true).await,
            refused,
            "originals still queued"
        );
        let reply = f.command(3, "!herdr consent return").await;
        assert!(
            reply.contains("반환 종료 0건") && reply.contains("반환 2건"),
            "{reply}"
        );
        let unrestored = f
            .command_as(&restarted().await, 3, "!herdr consent return")
            .await;
        assert!(
            unrestored.contains("반환 종료 0건"),
            "an unrestored mailbox is no evidence: {unrestored}"
        );
        let adopted = Resolution::Adopted {
            target: digest(&first).unwrap(),
        };
        assert_eq!(
            resolution(&held, a),
            Some(adopted),
            "evidence of the approval is kept"
        );
        while drain(&f.data.shared, &ran).await {}
        assert!(
            f.command(4, "!herdr consent return")
                .await
                .contains("반환 종료 2건")
        );
        assert_eq!(boot(&mut slot, &approvals, &user, false, true).await, None);
        f.enqueue(c, 19).await;
        hold_denied(c, 19);
        let target = f.legacy(if second == "same" {
            SOCKET
        } else {
            "/adk/herdr/legacy-two.sock"
        });
        assert_eq!(
            boot(&mut slot, &approvals, &target, false, true).await,
            refused
        );
        let reply = f.command(5, "!herdr consent return").await;
        assert!(
            reply.contains("입력 1건"),
            "closed returns are not checked again: {reply}"
        );
        assert_eq!(
            boot(&mut slot, &approvals, &target, false, true).await,
            None
        );
        while drain(&f.data.shared, &ran).await {}
        assert!(
            f.command(6, "!herdr consent return")
                .await
                .contains("반환 종료 1건")
        );
        let closed = Resolution::Closed {
            target: digest(&target).unwrap(),
        };
        assert_eq!(resolution(&held, c), Some(closed));
        let payloads: Vec<_> = [a, b, c].iter().map(|id| format!("payload {id}")).collect();
        assert_eq!(*ran.lock().unwrap(), payloads, "{second} adopt={adopt}");
        assert!(f.queued().await.is_empty());
    }
}

// Return writes nothing unless it runs on the local gateway, this boot's rollback was refused only
// by this channel's hold, the config is unchanged since boot and each input waits in both queues.
#[tokio::test(flavor = "current_thread")]
async fn return_refuses_and_writes_nothing_until_every_precondition_holds() {
    let f = Fixture::new().await;
    let approvals = FileApprovalStore(Some(f.runtime.path().join("approvals")));
    let user = user_default();
    approvals.save(&user).unwrap();
    let a = 940_487_400_000_311;
    f.enqueue(a, 17).await;
    let held = hold_denied(a, 17);
    let unchanged = store().load_all().unwrap();
    let remote = session_hosts::force_for_test(Some("gateway"), &[(CHANNEL, "node")])
        .with_mode(EndpointMode::UserDefault);
    let reads = STORE_READS.load(std::sync::atomic::Ordering::SeqCst);
    let reply = f.command(1, "!herdr consent return").await;
    assert!(
        reply.contains("실행 노드 node의 gateway에서 !herdr consent return"),
        "{reply}"
    );
    assert_eq!(STORE_READS.load(std::sync::atomic::Ordering::SeqCst), reads);
    drop(remote);
    let mut slot = None;
    let first = f.legacy(SOCKET);
    for (case, rows, refusal) in [
        ("row", true, "거부되지 않았습니다"),
        ("config", false, "설정이 바뀌었습니다"),
    ] {
        boot(&mut slot, &approvals, &first, rows, true).await;
        if case == "config" {
            f.legacy("/adk/herdr/edited.sock");
        }
        let reply = f.command(2, "!herdr consent return").await;
        assert!(reply.contains(refusal), "{case}: {reply}");
        assert_eq!(store().load_all().unwrap(), unchanged, "{case}");
    }
    f.legacy(SOCKET);
    boot(&mut slot, &approvals, &first, false, true).await;
    let missing = 940_487_400_000_312;
    hold_denied(missing, 18);
    let reply = f.command(3, "!herdr consent return").await;
    assert!(reply.contains(&format!("입력 {missing}")), "{reply}");
    let consent = consent_for(missing);
    store()
        .acknowledge_discard(&consent, &consent.intents[1])
        .unwrap();
    let unchanged = store().load_all().unwrap();
    let queued = f.mailbox().snapshot().await.intervention_queue[0].clone();
    let channel = serenity::ChannelId::new(CHANNEL);
    let token = &f.data.shared.token_hash;
    let provider = ProviderKind::Claude;
    crate::services::turn_orchestrator::save_channel_pending_dispatch_marker(
        &provider, token, channel, &queued, None,
    )
    .unwrap();
    let reply = f.command(4, "!herdr consent return").await;
    assert!(
        reply.contains(&format!("입력 {a}")),
        "a drain in transit is not returnable: {reply}"
    );
    assert_eq!(store().load_all().unwrap(), unchanged);
    assert_eq!(resolution(&held, a), None);
}

// Boot replay of an Applied receipt releases only unreturned input.
#[tokio::test(flavor = "current_thread")]
async fn applied_boot_replay_never_runs_a_returned_input() {
    let f = Fixture::new().await;
    let ran = Arc::new(Mutex::new(Vec::new()));
    let _kicks = kicks_drain(&ran);
    let _boot = session_hosts::force_for_test(Some("node"), &[(CHANNEL, "node")])
        .with_mode(EndpointMode::UserDefault);
    let _peer = Peer::set(Arc::new(Broken));
    let (kept, returned) = (940_487_400_000_321, 940_487_400_000_322);
    f.enqueue(kept, 17).await;
    f.enqueue(returned, 18).await;
    let key = key(
        "/adk/return-fixture/config.toml".as_ref(),
        "/adk/return-fixture/config.toml".as_ref(),
    );
    store()
        .hold(
            Question {
                message: 100,
                ..question(&key, &preview(), kept, 17)
            },
            now(),
        )
        .unwrap();
    let held = store()
        .hold(
            Question {
                message: 100,
                ..question(&key, &preview(), returned, 18)
            },
            now(),
        )
        .unwrap()
        .into_consent();
    let intent = held.intents[1].clone();
    let held = store()
        .settle(&held, &[(intent, Resolution::Returned { target: [7; 32] })])
        .unwrap();
    applied(&held);
    for _ in 0..2 {
        startup::restore(&f.ctx.http, &f.data.shared, &ProviderKind::Claude)
            .await
            .unwrap();
    }
    assert_eq!(*ran.lock().unwrap(), vec![format!("payload {kept}")]);
    assert_eq!(f.queued().await, vec![returned]);
}

fn applied(held: &Consent) -> Consent {
    let accepted = store().decide(held, held.owner, true, now()).unwrap();
    let receipt = consent::ApplyResult {
        before: Some([1; 32]),
        after: Some([4; 32]),
        backup: None,
        detail: None,
    };
    store().record_apply(&accepted, receipt).unwrap()
}

struct Broken;
impl PeerSource for Broken {
    fn snapshot(
        &self,
    ) -> Result<
        crate::services::session_host::policy::peer::PeerSnapshot,
        crate::services::session_host::policy::Unverified,
    > {
        Err(crate::services::session_host::policy::Unverified::Identity)
    }
}

struct Peer(Option<Arc<dyn PeerSource>>);
impl Peer {
    fn set(source: Arc<dyn PeerSource>) -> Self {
        Self(TEST_SOURCE.with(|current| current.replace(Some(source))))
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        TEST_SOURCE.with(|current| *current.borrow_mut() = self.0.take());
    }
}

fn click(bound: &Consent) -> serenity::ComponentInteraction {
    let mut message = discord_mock::user_message(bound.message, "card");
    message.author.id = serenity::UserId::new(bound.bot);
    serde_json::from_value(json!({
        "id":"940487400000042", "application_id":"940487400000003",
        "data":{"custom_id":format!("hresume:v1:{}:yes", bound.nonce),"component_type":2},
        "channel_id":CHANNEL.to_string(),"token":"test-token","version":1,
        "message":message,"locale":"ko","entitlements":[],"attachment_size_limit":10485760,
        "user":discord_mock::user_message(1, "").author
    }))
    .unwrap()
}

// Retry replays an existing Applied receipt without fresh evidence, and opens a card with the
// current preview only for input that awaits a new generation.
#[tokio::test(flavor = "current_thread")]
async fn applied_retry_replays_its_receipt_without_preview_and_reconsents_only_new_input() {
    for reconsent in [false, true] {
        let f = Fixture::new().await;
        let ran = Arc::new(Mutex::new(Vec::new()));
        let _kicks = kicks_drain(&ran);
        let _boot = session_hosts::force_for_test(Some("node"), &[(CHANNEL, "node")])
            .with_mode(EndpointMode::UserDefault);
        let (config_dir, fixture) = preview::tests::fixture("printf 'config: ok\\n'");
        fixture.0.lock().unwrap().witness.socket = SOCKET.into();
        let prepared = preview::prepare(fixture.clone(), 1).await.unwrap();
        let _peer = Peer::set(if reconsent { fixture } else { Arc::new(Broken) });
        startup::restore(&f.ctx.http, &f.data.shared, &ProviderKind::Claude)
            .await
            .unwrap();
        let key = key(prepared.logical(), prepared.target());
        let (old, new) = (940_487_400_000_331, 940_487_400_000_332);
        f.enqueue(old, 17).await;
        let first = Question {
            message: 100,
            ..question(&key, &prepared.token, old, 17)
        };
        let held = store().hold(first, now()).unwrap().into_consent();
        let applied = applied(&held);
        if reconsent {
            drain(&f.data.shared, &ran).await;
            store()
                .acknowledge_discard(&applied, &applied.intents[0])
                .unwrap();
            f.enqueue(new, 18).await;
            let mut changed = prepared.token.clone();
            (changed.revision, changed.chain_hash) = (2, [9; 32]);
            let next = Question {
                message: 100,
                ..question(&key, &changed, new, 18)
            };
            store().hold(next, now()).unwrap();
            // The config changed after the receipt, so only fresh evidence can be approved.
            let edited = "[session]\nresume_agents_on_restore=true\n# edited\n";
            std::fs::write(config_dir.path().join("config.toml"), edited).unwrap();
        }
        let reply = f.command(1, "!herdr consent retry").await;
        assert!(reply.is_empty(), "{reply}");
        if reconsent {
            assert_eq!(f.cards(), 1, "one card with the current preview");
            let bound = consent_for(new);
            assert!(
                super::super::component(&f.ctx, &click(&bound), &f.data)
                    .await
                    .unwrap()
            );
            let config = std::fs::read_to_string(config_dir.path().join("config.toml")).unwrap();
            assert!(config.contains("resume_agents_on_restore=false"));
        } else {
            assert_eq!(f.cards(), 0);
        }
        let expected = if reconsent { vec![old, new] } else { vec![old] };
        let payloads: Vec<_> = expected.iter().map(|id| format!("payload {id}")).collect();
        assert_eq!(*ran.lock().unwrap(), payloads, "reconsent={reconsent}");
    }
}

// A retry with no consent record re-runs admission at the held head once; when admission now asks
// for consent, the same retry continues to exactly one card and approval releases A then B.
#[tokio::test(flavor = "current_thread")]
async fn recordless_retry_reruns_admission_once_and_continues_to_one_card() {
    for resume in [false, true] {
        let f = Fixture::new().await;
        let ran = Arc::new(Mutex::new(Vec::new()));
        let _boot = session_hosts::force_for_test(Some("node"), &[(CHANNEL, "node")])
            .with_mode(EndpointMode::UserDefault);
        let (_config_dir, fixture) = preview::tests::fixture("printf 'config: ok\\n'");
        fixture.0.lock().unwrap().witness.socket = SOCKET.into();
        let prepared = preview::prepare(fixture.clone(), 1).await.unwrap();
        let _peer = Peer::set(fixture);
        startup::restore(&f.ctx.http, &f.data.shared, &ProviderKind::Claude)
            .await
            .unwrap();
        let key = key(prepared.logical(), prepared.target());
        let (a, b) = (940_487_400_000_341, 940_487_400_000_342);
        f.enqueue(a, 17).await;
        f.enqueue(b, 18).await;
        let kicks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (hook_ran, hook_kicks, token) = (ran.clone(), kicks.clone(), prepared.token.clone());
        let _hook =
            queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(move |shared, _, _, _| {
                let (ran, kicks, key, token) = (
                    hook_ran.clone(),
                    hook_kicks.clone(),
                    key.clone(),
                    token.clone(),
                );
                Box::pin(async move {
                    kicks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if resume && store().load_all().unwrap().is_empty() {
                        // Queued admission records every held source without a card.
                        for (source, generation) in [(a, 17), (b, 18)] {
                            store()
                                .hold_unbound(question(&key, &token, source, generation), now())
                                .unwrap();
                        }
                        return Some(IdleQueueKickoffChannelOutcome { started: false });
                    }
                    let started = drain(&shared, &ran).await;
                    Some(IdleQueueKickoffChannelOutcome { started })
                })
            }));
        let reply = f.command(1, "!herdr consent retry").await;
        assert!(reply.is_empty(), "{reply}");
        if resume {
            assert_eq!(
                (kicks.load(std::sync::atomic::Ordering::SeqCst), f.cards()),
                (1, 1)
            );
            let bound = consent_for(a);
            assert!(
                super::super::component(&f.ctx, &click(&bound), &f.data)
                    .await
                    .unwrap()
            );
        } else {
            assert_eq!(kicks.load(std::sync::atomic::Ordering::SeqCst), 1);
            drain(&f.data.shared, &ran).await;
        }
        let payloads: Vec<_> = [a, b].iter().map(|id| format!("payload {id}")).collect();
        assert_eq!(*ran.lock().unwrap(), payloads, "resume={resume}");
        assert_eq!(f.cards(), usize::from(resume));
    }
}
