//! Codex and Claude takeovers through the gateway's own facts, fence, custody and mailbox: what O
//! takes is O's to post, and a Legacy body claimed around the takeover waits on its lock.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use super::*;
use crate::config::Config;
use crate::services::agent_protocol::RuntimeHandoffKind::CodexTui;
use crate::services::discord::tui_direct_pending_start::turn_retirement::codex::confirm_codex_channels;
use crate::services::tui_o::channel_policy::BootChannels;
use crate::services::tui_o::cutover::{o_owns_tui_output_for_channel_reserving, test_override};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::turn_mode::{TurnConfig, transcript_turns};
use crate::services::tui_o::writer::activation::ActivationFacts;
use crate::services::tui_o::writer::activation::test_hook::{self, Step};
use crate::services::tui_o::writer::adoption::{LegacyCursor, LegacyView};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvents, BindingEvidence, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::late_adopt;
use poise::serenity_prelude as serenity;

/// Binds as the hook writer would; reads are counted so a bind can land after the baseline.
struct Log {
    channel: u64,
    provider: ShadowProvider,
    events: Mutex<Vec<BindingEvent>>,
    notice: watch::Sender<u64>,
    reads: AtomicUsize,
}

impl Log {
    fn new(channel: u64) -> Arc<Self> {
        Self::of(channel, Codex)
    }

    fn of(channel: u64, provider: ShadowProvider) -> Arc<Self> {
        Arc::new(Self {
            channel,
            provider,
            events: Mutex::default(),
            notice: watch::channel(0).0,
            reads: AtomicUsize::new(0),
        })
    }

    /// Binds `source` once the log was read more than `reads` times.
    async fn bind_after(&self, reads: usize, source: &SourceId) {
        while self.reads.load(Ordering::SeqCst) <= reads {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.bind_now(source);
    }

    fn bind_now(&self, source: &SourceId) {
        let mut events = self.events.lock().unwrap();
        let seq = events.len() as u64 + 1;
        let evidence = BindingEvidence {
            hook_event: "session_start".into(),
            received_at: chrono::Utc::now(),
            reclaims: false,
        };
        events.push(BindingEvent {
            seq,
            channel_id: self.channel,
            provider: self.provider,
            tmux_session: "takeover".into(),
            execution_nonce: "takeover".into(),
            record: BindingRecord::Bound {
                old: None,
                new: BindingTarget::Source(source.clone()),
                cause: BindingCause::Clear,
                parent_hint: None,
                evidence,
            },
            committed_at: chrono::Utc::now(),
        });
        drop(events);
        self.notice.send_replace(seq);
    }
}

impl BindingEvents for Log {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        assert_eq!(channel, self.channel);
        self.reads.fetch_add(1, Ordering::SeqCst);
        let events = self.events.lock().unwrap();
        Ok(events.iter().filter(|e| e.seq > after).cloned().collect())
    }

    fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
        self.notice.subscribe()
    }
}

/// The gateway host with late adoption on, the test's binding log and, when given, Legacy's
/// delivery as `stub` reports it; all else is the gateway's.
struct Late<H> {
    host: H,
    log: Arc<Log>,
    stub: Option<Arc<dyn LegacyView>>,
}

impl<H: HostIo> HostIo for Late<H> {
    type Port = H::Port;
    type Lease = H::Lease;
    type Alarms = H::Alarms;
    type Bindings = Log;
    fn port(&self) -> impl Future<Output = Arc<H::Port>> + Send {
        self.host.port()
    }
    fn port_for(&self, channel: u64) -> impl Future<Output = Result<Arc<H::Port>, String>> + Send {
        self.host.port_for(channel)
    }
    fn execution_permit(
        &self,
        channel: u64,
    ) -> Option<Result<Arc<crate::services::tui_o::ownership::execution::OwnerPermit>, String>>
    {
        self.host.execution_permit(channel)
    }
    fn lease(&self) -> H::Lease {
        self.host.lease()
    }
    fn writer_unavailable(&self, channel: u64) {
        self.host.writer_unavailable(channel);
    }
    fn alarms(&self) -> H::Alarms {
        self.host.alarms()
    }
    fn bindings(&self, _: u64, _: ShadowProvider) -> Arc<Log> {
        Arc::clone(&self.log)
    }
    fn late_adopt_enabled(&self) -> bool {
        true
    }
    fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        self.host.activation_facts(channel, provider)
    }
    fn intake_fence(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<FencedFacts, String>> + Send {
        self.host.intake_fence(channel, provider)
    }
    fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<Custody, String> {
        self.host.local_custody(channel, provider)
    }
    fn legacy(&self) -> Arc<dyn LegacyView> {
        self.stub.clone().unwrap_or_else(|| self.host.legacy())
    }
    fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send {
        self.host.legacy_busy(channel)
    }
    fn relaying(&self, channel: u64) -> bool {
        self.host.relaying(channel)
    }
    fn adopted(&self, channel: u64, provider: ShadowProvider) {
        self.host.adopted(channel, provider);
    }
}

/// Each of `channels` reserved for late adoption as the boot seeds it on the home.
fn reserve(channels: &[u64]) -> test_override::ChannelsGuard {
    reserve_as("codex", channels)
}

fn reserve_as(provider: &str, channels: &[u64]) -> test_override::ChannelsGuard {
    let agents: Vec<_> = (channels.iter())
        .map(|id| {
            serde_json::json!({"id": format!("w{id}"), "name": "W",
            "channels": {provider: {"id": id.to_string(), "runtime": "tui"}}})
        })
        .collect();
    let config: Config = serde_json::from_value(serde_json::json!({
        "server": {}, "agents": agents,
        "tui_o": {"writer": {"channels": channels, "late_adopt": true}}
    }))
    .unwrap();
    let states: BTreeMap<_, _> = channels.iter().map(|&c| (c, Adoption::Pending)).collect();
    let boot = BootChannels::validate(&config).unwrap();
    test_override::force_boot(boot.seeded(&config, |_, _| Ok(states)).unwrap())
}

fn candidate(channel: u64) -> Candidate {
    let candidate = test_override::with_channels(|boot| boot?.candidate(channel).cloned());
    candidate.unwrap()
}

/// A rollout after `/clear`: its header, the turn opener and the prompt Legacy took in.
fn rollout(dir: &std::path::Path, name: &str) -> SourceId {
    let path = dir.join(name);
    let lines = [
        json!({"type":"session_meta","payload":{"id":"takeover","cwd":"/tmp","source":"cli"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"hi"}]}}),
    ];
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&path, text).unwrap();
    source_id_for("takeover", &path).unwrap()
}

/// From its own runtime and connection, an intake insert that has not finished after 300ms.
fn insert_waits(url: String) -> (bool, std::thread::JoinHandle<()>) {
    let (waited, reported) = std::sync::mpsc::channel();
    let insert = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let pool = crate::db::postgres::connect_test_pool_with_max_connections(
                &url,
                "takeover fence probe",
                1,
            )
            .await
            .unwrap();
            let insert = open_intake(&pool, 64_700_599, "codex");
            tokio::pin!(insert);
            let timed = tokio::time::timeout(Duration::from_millis(300), &mut insert).await;
            waited.send(timed.is_err()).unwrap();
            if timed.is_err() {
                insert.await;
            }
            pool.close().await;
        });
    });
    let waited = reported.recv_timeout(Duration::from_secs(30)).unwrap();
    (waited, insert)
}

fn init_sources(store: &OStore, channel: u64) -> Vec<(SourceId, u64)> {
    let init = store.read_init(channel).unwrap().unwrap();
    (init.sources.into_iter())
        .map(|source| (source.source_id, source.delivery_start))
        .collect()
}

/// Over Legacy's open intake and Codex pending start an intake insert waits on the fence through
/// the write and turn mode stays off; a body claimed between custody's reads waits, then is O's.
#[test]
fn codex_takeover_holds_the_gateway_fence_through_its_write_and_its_adoption_lock_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(gateway_takeover());
}

async fn gateway_takeover() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let local = crate::services::cluster::node_registry::SELF_INSTANCE_ID.get();
    let local = local.map_or("takeover-local", String::as_str);
    let log = Log::new(64_700_501);
    let io = Late {
        host: test_host::over(shared, Duration::ZERO, Some(local)),
        log: Arc::clone(&log),
        stub: None,
    };
    let stores = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, stores.path());
    let store = store.unwrap().unwrap();
    let rollouts = tempfile::tempdir().unwrap();
    let (fenced_channel, raced) = (64_700_501, 64_700_502);
    let _reserved = reserve(&[fenced_channel, raced]);
    let gate = OwnershipGate::default();
    gate.acquired();

    open_intake(&pool, fenced_channel, "codex").await;
    let record = pending(Codex, fenced_channel);
    persist(&record).unwrap();
    let taken = rollout(rollouts.path(), "rollout-taken.jsonl");
    let probe = Arc::new(Mutex::new(None));
    let (slot, url) = (Arc::clone(&probe), fixture.database_url.clone());
    test_hook::set(fenced_channel, Step::BeforeWrite, move || {
        *slot.lock().unwrap() = Some(insert_waits(url));
        Ok(())
    });
    let adopting = candidate(fenced_channel);
    let activated =
        late_adopt::activate(&io, fenced_channel, Codex, &store, &*log, &adopting, &gate);
    let (committed, ()) = tokio::join!(activated, log.bind_after(0, &taken));
    assert!(committed);
    assert_eq!(adopting.peek(), Adoption::Committed);
    assert_eq!(init_sources(&store, fenced_channel), [(taken, 0)]);
    let (waited, insert) = probe
        .lock()
        .unwrap()
        .take()
        .expect("the takeover wrote its init");
    assert!(
        waited,
        "an intake insert waits on the fence until the init is written"
    );
    insert.join().unwrap();
    let custody = io.local_custody(fenced_channel, Codex);
    assert_eq!(custody, Ok(Custody::PendingStart), "Legacy's start is kept");
    let turn = TurnConfig {
        channels: [fenced_channel].into(),
        all_owned: Some(false),
    };
    assert!(confirm_codex_channels(Some(&turn), || vec![fenced_channel]).is_empty());
    delete(&record);
    // With Legacy's start gone the takeover still waits for its turn in the rollout bound now.
    assert!(confirm_codex_channels(Some(&turn), || vec![fenced_channel]).is_empty());
    assert!(!transcript_turns(fenced_channel));

    let raced_log = Log::new(raced);
    let io = Late {
        host: io.host,
        log: Arc::clone(&raced_log),
        stub: None,
    };
    // Legacy's body claim lands between custody's two reads and waits on the adoption lock.
    let claimer = Arc::new(Mutex::new(None));
    let (slot, share) = (Arc::clone(&claimer), test_override::shared_channels());
    test_hook::set(raced, Step::CustodyRead, move || {
        let (at_claim, reached) = std::sync::mpsc::channel();
        let claim = std::thread::spawn(move || {
            let _channels = share();
            at_claim.send(()).unwrap();
            o_owns_tui_output_for_channel_reserving(raced, Some(CodexTui)).map(|c| c.owned)
        });
        reached.recv().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        *slot.lock().unwrap() = Some((!claim.is_finished(), claim));
        Ok(())
    });
    let raced_rollout = rollout(rollouts.path(), "rollout-raced.jsonl");
    let adopting = candidate(raced);
    let activated = late_adopt::activate(&io, raced, Codex, &store, &*raced_log, &adopting, &gate);
    let (committed, ()) = tokio::join!(activated, raced_log.bind_after(0, &raced_rollout));
    assert!(committed);
    assert_eq!(init_sources(&store, raced), [(raced_rollout, 0)]);
    let (waited, claim) = claimer.lock().unwrap().take().expect("custody was read");
    assert!(waited, "the claim waited on the adoption lock");
    assert!(
        matches!(claim.join().unwrap(), Ok(true)),
        "O owns the body: Legacy sends nothing"
    );
    pool.close().await;
    fixture.drop().await;
}

/// Legacy's delivery for a Claude transcript: its cursor at EOF and the frontier the test sets.
struct Delivered {
    path: std::path::PathBuf,
    frontier: std::sync::atomic::AtomicU64,
}

impl LegacyView for Delivered {
    fn started(&self) -> bool {
        true
    }
    fn cursor(&self, _: &str) -> LegacyCursor {
        let offset = std::fs::metadata(&self.path).map_or(0, |meta| meta.len());
        LegacyCursor::Bound {
            path: self.path.clone(),
            offset,
        }
    }
    fn late_cursor(&self, tmux: &str) -> Result<LegacyCursor, String> {
        Ok(self.cursor(tmux))
    }
    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        Some(self.frontier.load(Ordering::SeqCst))
    }
    fn tail_running(&self, _: &str) -> bool {
        false
    }
}

/// Discord's REST API as the writer's port meets it, recording each posted message's content.
struct Rest {
    http: Arc<serenity::Http>,
    posts: Arc<Mutex<Vec<String>>>,
    server: tokio::task::AbortHandle,
}

impl Rest {
    async fn start(channel: u64) -> Self {
        use axum::{body::Bytes, http::Method, http::Uri, response::IntoResponse};
        let posts: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorded = Arc::clone(&posts);
        let app = axum::Router::new().fallback(axum::routing::any(
            move |method: Method, uri: Uri, body: Bytes| {
                let recorded = Arc::clone(&recorded);
                async move {
                    let bot = json!({"id": "1", "username": "o-bot", "discriminator": "0001",
                        "avatar": null, "bot": true});
                    if uri.path().ends_with("/users/@me") {
                        return axum::Json(bot).into_response();
                    }
                    if method == Method::GET {
                        return axum::Json(json!([])).into_response();
                    }
                    let payload: serde_json::Value =
                        serde_json::from_slice(&body).unwrap_or_default();
                    let content = payload["content"].as_str().unwrap_or_default().to_owned();
                    let mut posts = recorded.lock().unwrap();
                    posts.push(content.clone());
                    let message = json!({"id": (900_001 + posts.len() as u64).to_string(),
                        "channel_id": channel.to_string(), "content": content, "author": bot,
                        "timestamp": "2026-11-01T00:00:00+00:00", "edited_timestamp": null,
                        "tts": false, "mention_everyone": false, "mentions": [],
                        "mention_roles": [], "attachments": [], "embeds": [], "pinned": false,
                        "type": 0});
                    axum::Json(message).into_response()
                }
            },
        ));
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let http = serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://127.0.0.1:{port}"))
            .ratelimiter_disabled(true)
            .build();
        let server = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
        Self {
            http: Arc::new(http),
            posts,
            server: server.abort_handle(),
        }
    }

    fn posts_with(&self, text: &str) -> usize {
        let posts = self.posts.lock().unwrap();
        posts.iter().filter(|post| post.contains(text)).count()
    }
}

async fn within(seconds: u64, mut ready: impl FnMut() -> bool) -> bool {
    for _ in 0..seconds * 20 {
        if ready() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ready()
}

/// The real Claude gateway writer: the old transcript delivered and sampled, a new Discord turn
/// claims the real mailbox and Legacy's inflight row as O commits, and a cleared transcript binds.
/// O takes it while busy and Row stand, posts its answer once over REST, and Legacy sends none.
#[test]
fn claude_takeover_commits_while_a_discord_turn_holds_the_mailbox_and_its_row_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(busy_takeover());
}

async fn busy_takeover() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::tui_o::store::InitOrigin;
    use crate::services::tui_o::writer::host;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let channel = 64_700_511;
    let rest = Rest::start(channel).await;
    let _reserved = reserve_as("claude", &[channel]);
    let session = "c1a0de00-0000-4000-8000-000000700511";
    let record = |value: serde_json::Value| format!("{value}\n");
    let prompted = || {
        record(json!({"type": "mode", "mode": "normal", "sessionId": session}))
            + &record(json!({"type": "user", "sessionId": session, "uuid": "u1",
                "message": {"role": "user", "content": "hi"}}))
    };
    let answer = |id: &str, text: &str| {
        record(
            json!({"type": "assistant", "sessionId": session, "uuid": format!("a-{id}"),
            "apiBlockIndex": 0, "message": {"id": id, "role": "assistant",
            "content": [{"type": "text", "text": text}]}}),
        ) + &record(
            json!({"type": "system", "subtype": "turn_duration", "durationMs": 5,
                "sessionId": session}),
        )
    };
    let transcripts = tempfile::tempdir().unwrap();
    let old_path = transcripts.path().join("claude-old.jsonl");
    let old = prompted() + &answer("msg_0", "the delivered old answer");
    std::fs::write(&old_path, &old).unwrap();
    let log = Log::of(channel, Claude);
    log.bind_now(&source_id_for(session, &old_path).unwrap());
    let delivered = Arc::new(Delivered {
        path: old_path,
        frontier: (old.len() as u64).into(),
    });
    let refused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fired = Arc::clone(&refused);
    test_hook::set(channel, Step::AfterFence, move || {
        fired.store(true, Ordering::SeqCst);
        Err("a new Discord turn claimed the mailbox".into())
    });

    let local = crate::services::cluster::node_registry::SELF_INSTANCE_ID.get();
    let local = local.map_or("takeover-local", String::as_str);
    let prepared = test_host::claude_parts(&shared, Arc::clone(&rest.http), local);
    let Ok(gateway) = Arc::try_unwrap(prepared.io) else {
        panic!("the gateway host is shared")
    };
    let io = Arc::new(Late {
        host: gateway,
        log: Arc::clone(&log),
        stub: Some(delivered),
    });
    prepared.gate.acquired();
    let parts = host::HostParts {
        io: Arc::clone(&io),
        runtime_root: prepared.runtime_root,
        gate: prepared.gate,
        readiness: prepared.readiness,
    };
    let owned = crate::services::tui_o::cutover::boot_ownership();
    let mut handles = host::start_managed(Claude, true, owned, || parts);
    assert_eq!(handles.len(), 1);
    assert!(
        within(60, || refused.load(Ordering::SeqCst)).await,
        "the delivered old transcript's quiet commit reached the fence"
    );

    let token = Arc::new(CancelToken::new());
    let (owner, message) = (UserId::new(1), MessageId::new(10));
    let claim = crate::services::discord::mailbox_try_start_turn;
    let claimed = claim(&shared, ChannelId::new(channel), token, owner, message);
    assert!(claimed.await, "the new Discord turn claims the mailbox");
    let inflight = crate::services::discord::inflight::inflight_runtime_root().unwrap();
    let row = crate::services::discord::inflight::inflight_state_path(
        &inflight,
        &ProviderKind::Claude,
        channel,
    );
    std::fs::create_dir_all(row.parent().unwrap()).unwrap();
    std::fs::write(&row, "{}").unwrap();
    assert!(io.legacy_busy(channel).await);
    assert_eq!(io.local_custody(channel, Claude), Ok(Custody::Row));

    let new_path = transcripts.path().join("claude-new.jsonl");
    std::fs::write(&new_path, prompted()).unwrap();
    let taken = source_id_for(session, &new_path).unwrap();
    log.bind_now(&taken);
    let root = crate::config::runtime_root().unwrap();
    let store = OStore::existing(&root);
    let read_init = || {
        store
            .as_ref()
            .and_then(|s| s.read_init(channel).ok().flatten())
    };
    assert!(
        within(60, || read_init().is_some()).await,
        "busy and a Row do not stop the takeover"
    );
    let store = OStore::existing(&root).unwrap();
    assert_eq!(init_sources(&store, channel), [(taken, 0)]);
    let origin = store.read_init_origin(channel).unwrap();
    assert_eq!(origin, InitOrigin::LateClaudeTakeover);
    assert!(
        io.legacy_busy(channel).await,
        "the Discord turn still holds the mailbox"
    );
    assert_eq!(io.local_custody(channel, Claude), Ok(Custody::Row));

    let reply = "the new turn's answer";
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&new_path)
        .unwrap();
    std::io::Write::write_all(&mut file, answer("msg_1", reply).as_bytes()).unwrap();
    assert!(
        within(60, || rest.posts_with(reply) > 0).await,
        "O posts the answer while the turn holds the mailbox"
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(rest.posts_with(reply), 1, "O posts the answer once");
    assert_eq!(rest.posts_with("the delivered old answer"), 0);
    let claim = o_owns_tui_output_for_channel_reserving(channel, Some(ClaudeTui));
    assert!(
        matches!(claim.map(|claim| claim.owned), Ok(true)),
        "O owns the answer's body: Legacy sends none of it"
    );
    for handle in handles.drain(..) {
        let stopped = tokio::time::timeout(Duration::from_secs(20), handle.stop_and_join());
        stopped.await.expect("the writer stops").unwrap();
    }
    rest.server.abort();
    pool.close().await;
    fixture.drop().await;
}
