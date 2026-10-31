use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
};
use poise::serenity_prelude::{self as serenity, ChannelId, MessageId, UserId};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::inject_hook;
use crate::services::discord::health::{HealthRegistry, InjectPane, register_inject_runtime};
use crate::services::discord::inflight::TurnSource;
use crate::services::discord::{SharedData, queue_dispatch, queue_io, router, runtime_store};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, Intervention, QueueIdentity, QueuedHeadClaim,
};

/// Heads the queued-head kickoff offered to the injector, in order.
pub(super) static OFFERED_HEADS: std::sync::Mutex<Vec<MessageId>> =
    std::sync::Mutex::new(Vec::new());

/// Every attempt row the queued-head kickoff emitted, in order.
pub(super) static ATTEMPT_ROWS: std::sync::Mutex<Vec<super::AttemptRow>> =
    std::sync::Mutex::new(Vec::new());

#[path = "queued_head_auto_tests.rs"]
mod auto;

/// The server nickname the fake Discord API gives every member the queued paste looks up; the
/// member's username and global name differ from it.
const AUTHOR_NAME: &str = "s2b author";
/// The fake server every channel belongs to.
const GUILD: &str = "6813000";

struct Fixture {
    _db: TestPostgresDb,
    pool: sqlx::PgPool,
    context: serenity::Context,
    app: Router,
    shared: Arc<SharedData>,
    pane: InjectPane,
    channel: ChannelId,
    holder: Arc<CancelToken>,
    holder_message: MessageId,
    http: Arc<serenity::Http>,
    channel_gets: Arc<AtomicUsize>,
    parent_gets: Arc<AtomicUsize>,
    /// While set, every read of the channel itself fails; it counts the reads it refused.
    refuse_reads: Arc<(std::sync::atomic::AtomicBool, AtomicUsize)>,
    /// How the fake API answers name lookups ("answer", "fail" or "slow"), and who wrote every
    /// Discord message it serves.
    names: Arc<std::sync::Mutex<(&'static str, u64)>>,
    server: tokio::task::JoinHandle<()>,
    _rest: crate::services::discord::shared_state::test_rest::Guard,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new(channel: u64, mode: &str) -> Self {
        let fixture = Self::build(channel, mode).await;
        let channel = fixture.channel;
        // The real route must enqueue H, rather than inject it on arrival, and schedule nothing.
        let transition = fixture
            .shared
            .session_transition_lock(channel)
            .lock_owned()
            .await;
        queue_io::with_post_enqueue_idle_queue_kick_suppressed(async {
            fixture.enqueue("status?").await;
            fixture.enqueue("distinct tail B").await;
        })
        .await;
        drop(transition);
        let queue = fixture.queue().await;
        assert_eq!(queue.len(), 2);
        assert_ne!(queue[0].message_id, queue[1].message_id);
        assert_eq!(queue[0].source_message_ids, [queue[0].message_id]);
        assert_eq!(
            queue[0].source_text_segments.len(),
            1,
            "the live route's canonical singleton is not an empty-segment restore fixture"
        );
        assert_eq!(
            queue[0].source_text_segments[0].message_id,
            queue[0].message_id
        );
        assert_eq!(queue[0].source_text_segments[0].text, "status?");
        assert_eq!(
            fixture.pane.tmux_calls(),
            0,
            "setup must not call the executor"
        );
        fixture
    }

    /// A busy Claude channel behind the real route, with nothing queued yet.
    async fn build(channel: u64, mode: &str) -> Self {
        Self::build_in(channel, mode, None).await
    }

    /// As `build`, for a thread of `parent` when one is named.
    async fn build_in(channel: u64, mode: &str, parent: Option<ChannelId>) -> Self {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        crate::db::agents::insert_agent_channels_for_tests(
            &pool,
            "s2b-head-agent",
            Some(&channel.to_string()),
            None,
        )
        .await;
        let registry = Arc::new(HealthRegistry::new());
        let shared = register_inject_runtime(&registry, &[channel], Some(pool.clone())).await;
        // A thread's routing is checked against its parent, which the bot is allowed in.
        let allowed = parent.map(|parent| parent.get());
        shared
            .settings
            .write()
            .await
            .allowed_channel_ids
            .extend(allowed);
        shared
            .http
            .cached_bot_token
            .set("s2b-test-token".into())
            .unwrap();
        let pane = InjectPane::new(channel, mode);
        inject_hook::set_queued_head(channel, true);
        let holder_message = MessageId::new(channel + 500);
        pane.reseat_row(TurnSource::Managed, holder_message.get());
        let holder = Arc::new(CancelToken::new());
        let channel = ChannelId::new(channel);
        assert!(
            shared
                .mailbox(channel)
                .try_start_turn(holder.clone(), UserId::new(100), holder_message)
                .await
        );

        let channel_gets = Arc::new(AtomicUsize::new(0));
        let counted_gets = channel_gets.clone();
        let parent_gets = Arc::new(AtomicUsize::new(0));
        let counted_parent_gets = parent_gets.clone();
        let refuse_reads = Arc::new((
            std::sync::atomic::AtomicBool::new(false),
            AtomicUsize::new(0),
        ));
        let refused = refuse_reads.clone();
        let names = Arc::new(std::sync::Mutex::new(("answer", 200)));
        let named = names.clone();
        let queue_path = runtime_store::discord_pending_queue_root()
            .unwrap()
            .join("claude")
            .join(&shared.token_hash)
            .join(format!("{channel}.json"));
        let rest_app = Router::new().route("/gateway", axum::routing::get(
            |ws: axum::extract::ws::WebSocketUpgrade| async {
                ws.on_upgrade(|mut socket| async move {
                    while socket.recv().await.is_some() {}
                })
            },
        )).fallback(move |method: Method, uri: axum::http::Uri| {
            let (counted_gets, refused) = (counted_gets.clone(), refused.clone());
            let counted_parent_gets = counted_parent_gets.clone();
            let queue_path = queue_path.clone();
            let (lookup, writer) = *named.lock().unwrap();
            async move {
            use axum::response::IntoResponse;
            if method == Method::DELETE
                && let Some((message, reaction)) = uri.path()
                    .strip_prefix(&format!("/api/v10/channels/{channel}/messages/"))
                    .and_then(|tail| tail.split_once("/reactions/"))
                && reaction.strip_suffix("/@me").is_some_and(|emoji| !emoji.is_empty() && !emoji.contains('/'))
            {
                let message = message.parse::<u64>().expect("reaction source id");
                let disk: Value = serde_json::from_slice(&std::fs::read(&queue_path).unwrap()).unwrap();
                let committed = disk["terminal_identities"].as_array().is_some_and(|identities| {
                    identities.iter().any(|identity| identity["sources"].as_array().is_some_and(|sources| {
                        sources.iter().any(|source| source["message_id"].as_u64() == Some(message))
                    }))
                });
                assert!(committed, "queued reaction cannot be removed before its source consume commits: {uri}");
                return StatusCode::NO_CONTENT.into_response();
            }
            assert_eq!(method, Method::GET, "queued injection cannot publish or tear down a queued card before consume: {uri}");
            let user = |id: &str| json!({"id": id, "username": "s2b username",
                "global_name": "s2b global", "discriminator": "0", "avatar": null});
            // The paste names its author as the channel's server shows them.
            if let Some(id) = uri.path().strip_prefix(&format!("/api/v10/guilds/{GUILD}/members/")) {
                match lookup {
                    "fail" => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
                    "slow" => tokio::time::sleep(std::time::Duration::from_secs(5)).await,
                    _ => {}
                }
                let member = json!({"user": user(id), "nick": AUTHOR_NAME, "roles": [],
                    "joined_at": null, "deaf": false, "mute": false, "flags": 0});
                return axum::Json(member).into_response();
            }
            if let Some(id) = uri.path().strip_prefix(&format!("/api/v10/channels/{channel}/messages/")) {
                let message = json!({"id": id, "channel_id": channel.get().to_string(),
                    "author": user(&writer.to_string()), "content": "status?",
                    "timestamp": "2026-07-26T00:00:00.000000+00:00", "edited_timestamp": null,
                    "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
                    "attachments": [], "embeds": [], "pinned": false, "type": 0});
                return axum::Json(message).into_response();
            }
            assert!(uri.path().starts_with("/api/v10/channels/"), "{uri}");
            let mut answer = json!({
                "id": channel.get().to_string(), "type": 0,
                "name": "s2b-claude", "guild_id": "6813000",
                "position": 0, "permission_overwrites": [], "nsfw": false,
                "parent_id": null
            });
            if uri.path() == format!("/api/v10/channels/{}", channel.get()) {
                counted_gets.fetch_add(1, Relaxed);
                if refused.0.load(Relaxed) {
                    refused.1.fetch_add(1, Relaxed);
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                if let Some(parent) = parent {
                    answer["type"] = json!(11);
                    answer["parent_id"] = json!(parent.get().to_string());
                }
            } else if let Some(parent) = parent.filter(|p| uri.path().ends_with(&format!("/{p}"))) {
                counted_parent_gets.fetch_add(1, Relaxed);
                answer["id"] = json!(parent.get().to_string());
            }
            axum::Json(answer).into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let proxy = format!("http://{address}");
        let http = Arc::new(
            serenity::HttpBuilder::new("s2b-test-token")
                .proxy(proxy.clone())
                .ratelimiter_disabled(true)
                .build(),
        );
        let server = tokio::spawn(async move { axum::serve(listener, rest_app).await.unwrap() });
        let rest = crate::services::discord::shared_state::test_rest::install(http.clone());
        // A Managed row proceeds through the route's normal start veto before enqueue.
        // Give it a ready local gateway Context, retaining the real route and actor admission.
        let context =
            crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock::serenity_context(
                proxy,
                format!("ws://{address}/gateway"),
            )
            .await;
        shared
            .http
            .cached_serenity_ctx
            .set(context.clone())
            .unwrap();
        let config = crate::config::Config::default();
        let engine = crate::engine::PolicyEngine::new(&config).unwrap();
        let broadcast = crate::eventbus::new_broadcast();
        let buffer = crate::eventbus::spawn_batch_flusher(broadcast.clone());
        let app = crate::server::routes::api_router_with_pg(
            engine,
            config,
            broadcast,
            buffer,
            Some(registry),
            Some(pool.clone()),
        );
        Self {
            _db: db,
            pool,
            context,
            app,
            shared,
            pane,
            channel,
            holder,
            holder_message,
            http,
            channel_gets,
            parent_gets,
            refuse_reads,
            names,
            server,
            _rest: rest,
        }
    }

    /// A process restart: a new runtime with its own mailbox registry restores the queue from disk
    /// as startup does, on the same pane, transcript and database; `busy` holds the same turn again.
    async fn restart(&mut self, busy: bool) {
        let registry = HealthRegistry::new();
        let channels = [self.channel.get()];
        let shared = register_inject_runtime(&registry, &channels, Some(self.pool.clone())).await;
        let token = "s2b-test-token".to_string();
        shared.http.cached_bot_token.set(token).unwrap();
        let context = self.context.clone();
        shared.http.cached_serenity_ctx.set(context).unwrap();
        // Startup restores only channels the configuration binds, where the route read the allowlist.
        let role_map = runtime_store::role_map_path().unwrap();
        std::fs::create_dir_all(role_map.parent().unwrap()).unwrap();
        let bound = json!({ "byChannelId": { self.channel.get().to_string():
            { "roleId": "s2b-head-agent", "promptFile": "prompt.md", "provider": "claude" } } });
        std::fs::write(role_map, bound.to_string()).unwrap();
        let restore =
            crate::services::discord::runtime_bootstrap::restore_queued_and_inflight_for_tests;
        restore(&self.http, &shared, &ProviderKind::Claude).await;
        let mailbox = shared.mailbox(self.channel);
        let (holder, owner) = (self.holder.clone(), UserId::new(100));
        if busy && mailbox.snapshot().await.cancel_token.is_none() {
            mailbox
                .restore_active_turn(holder, owner, self.holder_message)
                .await;
        }
        self.shared = shared;
    }

    async fn enqueue(&self, text: &str) {
        let body = self.deliver(text).await;
        assert_eq!(body["delivery"], "queued", "{body}");
    }

    /// The route's answer to one external input.
    async fn deliver(&self, text: &str) -> Value {
        self.deliver_as(text, 200).await
    }

    async fn deliver_as(&self, text: &str, author: u64) -> Value {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/agents/s2b-head-agent/turn/deliver")
            .header("content-type", "application/json")
            .extension(crate::server::routes::auth::loopback_peer())
            .body(Body::from(
                json!({ "text": text, "author_discord_user_id": author.to_string(), "source": "imessage" })
                    .to_string(),
            ))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap())
                .unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    /// Spends every listed input's busy-retry budget.
    fn park(&self, ids: &[MessageId]) {
        use crate::services::discord::busy_followup_retry_store as retries;
        for id in ids {
            for _ in 0..retries::MAX_BUSY_RETRY_COUNT {
                retries::record_busy_retry(&ProviderKind::Claude, self.channel.get(), id.get(), 9)
                    .unwrap();
            }
        }
    }

    async fn queue(&self) -> Vec<Intervention> {
        self.shared
            .mailbox(self.channel)
            .snapshot()
            .await
            .intervention_queue
    }

    fn path(&self) -> std::path::PathBuf {
        runtime_store::discord_pending_queue_root()
            .unwrap()
            .join("claude")
            .join(&self.shared.token_hash)
            .join(format!("{}.json", self.channel.get()))
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(self.path()).unwrap()
    }

    fn gets(&self) -> usize {
        self.channel_gets.load(Relaxed)
    }

    /// Makes reads of the channel fail, or read again; returns how many it has refused so far.
    fn refuse_channel_reads(&self, refuse: bool) -> usize {
        self.refuse_reads.0.store(refuse, Relaxed);
        self.refuse_reads.1.load(Relaxed)
    }

    /// Makes name lookups answer, fail or answer too late, with every Discord message `writer`'s.
    fn answer_names(&self, lookup: &'static str, writer: u64) {
        *self.names.lock().unwrap() = (lookup, writer);
    }

    fn disk(&self) -> Value {
        serde_json::from_slice(&self.bytes()).unwrap()
    }

    /// Waiting notices queued for the channel's outbox by input preparation.
    async fn preparation_notices(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE target=$1 AND reason_code='claude.input.preparation_hold'")
            .bind(format!("channel:{}", self.channel))
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn kick(&self) {
        let deps = router::IntakeDeps {
            http: &self.http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared: &self.shared,
            token: "s2b-test-token",
        };
        // This is the production KICKOFF function, not a direct helper or actor claim.
        let _ = queue_dispatch::kickoff::kickoff_idle_queue_channel(
            &deps,
            &ProviderKind::Claude,
            self.channel,
        )
        .await;
    }

    fn spawn_kick(&self) -> tokio::task::JoinHandle<()> {
        let (shared, http, channel) = (self.shared.clone(), self.http.clone(), self.channel);
        tokio::spawn(async move {
            let deps = router::IntakeDeps {
                http: &http,
                cache: None,
                ctx_for_chained_dispatch: None,
                shared: &shared,
                token: "s2b-test-token",
            };
            let _ = queue_dispatch::kickoff::kickoff_idle_queue_channel(
                &deps,
                &ProviderKind::Claude,
                channel,
            )
            .await;
        })
    }

    async fn assert_unknown_barrier(&self, before: Value) {
        assert_eq!(payloads(items(self.disk())), before);
        assert_eq!(self.disk()["head_injection"]["phase"], "unknown");
        assert!(
            self.disk()["terminal_identities"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let original = self.bytes();
        let calls = self.pane.tmux_calls();
        self.kick().await;
        assert_eq!(
            self.pane.tmux_calls(),
            calls,
            "UNKNOWN cannot retry the executor"
        );
        assert_eq!(self.bytes(), original);
        let restarted = ChannelMailboxRegistry::default().handle(self.channel);
        let persistence = crate::services::discord::queue_persistence_context(
            &self.shared,
            &ProviderKind::Claude,
            self.channel,
        );
        let take = restarted.take_next_soft(persistence).await;
        assert!(
            take.intervention.is_none(),
            "a restarted reader cannot skip UNKNOWN H to B"
        );
        assert!(take.persistence_error.is_none());
        assert_eq!(self.bytes(), original);
    }

    async fn assert_holder_kept(&self) {
        let snapshot = self.shared.mailbox(self.channel).snapshot().await;
        assert!(
            snapshot
                .cancel_token
                .as_ref()
                .is_some_and(|token| Arc::ptr_eq(token, &self.holder))
        );
        assert_eq!(snapshot.active_user_message_id, Some(self.holder_message));
        assert_eq!(snapshot.active_request_owner, Some(UserId::new(100)));
        let row = crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            self.channel.get(),
        )
        .unwrap();
        assert_eq!(row.user_msg_id, self.holder_message.get());
        assert_eq!(row.turn_source, TurnSource::Managed);
    }
}

fn payloads(mut items: Value) -> Value {
    for item in items.as_array_mut().unwrap() {
        item.as_object_mut()
            .unwrap()
            .remove("created_at_wall_time_ms");
    }
    items
}

fn replace_same_path_binding(fixture: &Fixture, field: &str) {
    let session = fixture.pane.session();
    let mut binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&session)
            .expect("the original Claude binding");
    let same_paths = (
        binding.output_path.clone(),
        binding.relay_output_path.clone(),
    );
    match field {
        "session_id" => binding.session_id = Some("s2b-replacement-session-id".into()),
        "input_fifo_path" => {
            binding.input_fifo_path =
                Some(fixture.pane.path("replacement-fifo").display().to_string())
        }
        _ => panic!("unknown binding field"),
    }
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&session, binding);
    let replaced =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&session).unwrap();
    assert_eq!(
        (replaced.output_path, replaced.relay_output_path),
        same_paths,
        "the pane and transcript paths must remain identical"
    );
}

fn items(disk: Value) -> Value {
    if disk.is_array() {
        disk
    } else {
        disk["items"].clone()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_actual_router_kickoff_keeps_each_gate_independently_off_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, mode, discord_open, queued_open) in [
        (6_813_401, "off", true, true),
        (6_813_402, "all", false, true),
        (6_813_423, "all", true, false),
    ] {
        let fixture = Fixture::new(channel, mode).await;
        inject_hook::set_queued_head(channel, queued_open);
        let _gate = discord_open.then(|| router::busy_inject_support::open_gate(channel));
        let before = fixture.bytes();
        let queue = format!("{:?}", fixture.queue().await);
        let gets = fixture.gets();
        fixture.kick().await;
        assert_eq!(
            fixture.pane.tmux_calls(),
            0,
            "mode={mode}, discord_open={discord_open}"
        );
        assert_eq!(fixture.bytes(), before);
        assert_eq!(format!("{:?}", fixture.queue().await), queue);
        if !queued_open || mode == "off" {
            assert_eq!(fixture.gets() - gets, 1, "queued_open={queued_open}");
        }
        fixture.assert_holder_kept().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn kickoff_reuses_channel_routing_lookup_before_queued_head_attempt_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, parent, author, expected_child_gets) in [
        (7_484_210, None, 1, 1),
        (7_484_211, Some(ChannelId::new(7_484_209)), 1, 1),
        (7_484_212, None, 200, 2),
    ] {
        let fixture = Fixture::build_in(channel, "all", parent).await;
        fixture
            .shared
            .settings
            .write()
            .await
            .allowed_user_ids
            .push(1);
        let _gate = router::busy_inject_support::open_gate(parent.map_or(channel, |id| id.get()));
        if parent.is_some() {
            assert!(!router::queued_head_inject_covers(fixture.channel, None));
            assert!(router::queued_head_inject_covers(fixture.channel, parent));
        }
        let transition = fixture
            .shared
            .session_transition_lock(fixture.channel)
            .lock_owned()
            .await;
        // System heads isolate routing reads; a user head also reads its author-name context.
        let delivered = queue_io::with_post_enqueue_idle_queue_kick_suppressed(
            fixture.deliver_as("lookup budget", author),
        )
        .await;
        assert_eq!(delivered["delivery"], "queued", "{delivered}");
        drop(transition);
        let head = fixture.queue().await.remove(0);
        assert_eq!(head.author_id, UserId::new(author));
        let channel_gets = fixture.gets();
        let parent_gets = fixture.parent_gets.load(Relaxed);
        assert!(!ATTEMPT_ROWS.lock().unwrap().iter().any(|row| {
            row.channel_id == channel && row.head_message_id == head.message_id.get()
        }));

        fixture.kick().await;

        let rows = ATTEMPT_ROWS.lock().unwrap().clone();
        let row = rows
            .iter()
            .find(|row| row.channel_id == channel && row.head_message_id == head.message_id.get())
            .expect("the real kickoff must reach queued_head_attempt");
        assert_eq!((row.trigger, row.outcome), ("idle_kickoff", "injected"));
        let reads = (
            fixture.gets() - channel_gets,
            fixture.parent_gets.load(Relaxed) - parent_gets,
        );
        assert_eq!(reads, (expected_child_gets, usize::from(parent.is_some())));
        assert!(reads.0 + reads.1 <= 2);
        assert!(fixture.queue().await.is_empty());
        fixture.assert_holder_kept().await;
    }
}

/// A head is typed only under the name Discord shows for its own author: a lookup that fails or
/// answers too late, or a Discord message another person wrote, types nothing and keeps the queue.
#[tokio::test(flavor = "current_thread")]
async fn s2b_a_head_whose_author_is_not_named_stays_queued_pg() {
    use crate::services::turn_orchestrator::{InterventionMode, SourceMessageQueuedGeneration};
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, lookup, discord_message, reason) in [
        (6_813_431, "fail", false, "queued_name_unresolved"),
        (6_813_432, "slow", false, "queued_name_unresolved"),
        (6_813_433, "answer", true, "queued_author_mismatch"),
    ] {
        let fixture = Fixture::build(channel, "all").await;
        let _gate = router::busy_inject_support::open_gate(channel);
        fixture.answer_names(lookup, 300);
        let transition = fixture
            .shared
            .session_transition_lock(fixture.channel)
            .lock_owned()
            .await;
        if discord_message {
            // A message author 200 sent, as live intake queues it; the API names 300 as its writer.
            let id = MessageId::new(1_500_000_000_000_000_000 + channel);
            let generation = runtime_store::process_generation();
            let head = Intervention {
                author_id: UserId::new(200),
                author_is_bot: false,
                message_id: id,
                queued_generation: generation,
                source_message_ids: vec![id],
                source_message_queued_generations: vec![SourceMessageQueuedGeneration::new(
                    id, generation,
                )],
                source_text_segments: Vec::new(),
                text: "status?".to_string(),
                mode: InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            };
            let persistence = crate::services::discord::queue_persistence_context(
                &fixture.shared,
                &ProviderKind::Claude,
                fixture.channel,
            );
            let mailbox = fixture.shared.mailbox(fixture.channel);
            let enqueued = queue_io::with_post_enqueue_idle_queue_kick_suppressed(
                mailbox.enqueue(head, persistence),
            )
            .await;
            assert!(enqueued.enqueued, "{:?}", enqueued.refusal_reason);
        } else {
            queue_io::with_post_enqueue_idle_queue_kick_suppressed(fixture.enqueue("status?"))
                .await;
        }
        drop(transition);
        let (bytes, queue) = (fixture.bytes(), format!("{:?}", fixture.queue().await));
        fixture.kick().await;
        let rows = ATTEMPT_ROWS.lock().unwrap().clone();
        let row = rows.iter().rev().find(|row| row.channel_id == channel);
        let row = row.map(|row| (row.outcome, row.reason));
        assert_eq!(row, Some(("not_sent", reason)), "{lookup}");
        assert_eq!(fixture.pane.tmux_calls(), 0, "{lookup}");
        assert_eq!(format!("{:?}", fixture.queue().await), queue, "{lookup}");
        assert_eq!(fixture.bytes(), bytes, "{lookup}");
        fixture.assert_holder_kept().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_actual_router_kickoff_injects_only_the_canonical_head_and_settles_its_identity_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_813_403, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let before = fixture.queue().await;
    let original_tail = format!("{:?}", before[1]);
    fixture.kick().await;
    assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);
    assert!(fixture.pane.transcript_recorded_the_paste());
    let buffer = std::fs::read_to_string(fixture.pane.path("buffer")).unwrap();
    let (header, text) = crate::services::tui_input::human_frame::parse(&buffer).unwrap();
    assert_eq!(text, " status?", "the owner must paste the original H body");
    assert_eq!(
        (header.name, header.source, header.nonce),
        (AUTHOR_NAME, "", "")
    );
    assert_eq!(
        header.author, "200",
        "H's author must not become holder A's author"
    );
    assert_eq!(
        header.mode,
        crate::services::tui_input::human_frame::Mode::Inject
    );
    assert!(
        crate::services::tui_input::human_frame::history::registered(
            &fixture.pane.nonce().expect("hidden attempt id"),
            &fixture.pane.path(InjectPane::TRANSCRIPT),
            &buffer,
        )
    );
    let transcript = std::fs::read_to_string(fixture.pane.path(InjectPane::TRANSCRIPT)).unwrap();
    let acknowledged: Vec<Value> = transcript
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|record: &Value| record["type"] == "queue-operation")
        .collect();
    assert_eq!(acknowledged.len(), 1);
    assert_eq!(acknowledged[0]["content"], buffer);
    assert!(!buffer.contains("distinct tail B"));
    let after = fixture.queue().await;
    assert_eq!(after.len(), 1);
    assert_eq!(format!("{:?}", after[0]), original_tail);
    let disk = fixture.disk();
    assert!(disk["head_injection"].is_null());
    let terminal = disk["terminal_identities"].as_array().unwrap();
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0]["primary"], before[0].message_id.get());
    assert_eq!(
        terminal[0]["queued_generation"],
        before[0].queued_generation
    );
    assert_eq!(terminal[0]["sources"].as_array().unwrap().len(), 1);
    assert_eq!(
        terminal[0]["sources"][0]["message_id"],
        before[0].message_id.get()
    );
    assert_eq!(
        terminal[0]["sources"][0]["queued_generation"],
        before[0].queued_generation
    );
    assert_ne!(before[0].message_id, fixture.holder_message);
    fixture.assert_holder_kept().await;
}

/// A queued head stays settled at Enter, yet its transcript receipt still reaches the frame ledger,
/// so the same author's same body later is injected rather than refused as still unconfirmed.
#[tokio::test(flavor = "current_thread")]
async fn s2b_a_received_queued_head_lets_the_same_body_be_injected_again_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(7_510_101, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    fixture.kick().await;
    assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);
    let first = std::fs::read_to_string(fixture.pane.path("buffer")).unwrap();
    let nonce = fixture.pane.nonce().unwrap();
    let ledger = crate::config::runtime_root()
        .unwrap()
        .join("runtime/injected_frames.json");
    let received = || {
        let stored: Value = std::fs::read(&ledger)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        stored["entries"].as_array().is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry["id"] == nonce.as_str()
                    && entry["events"].as_array().is_some_and(|e| !e.is_empty())
            })
        })
    };
    let mut seen = received();
    for _ in 0..100 {
        if seen {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        seen = received();
    }
    assert!(
        seen,
        "the queued head's receipt must reach the frame ledger"
    );
    let transition = fixture
        .shared
        .session_transition_lock(fixture.channel)
        .lock_owned()
        .await;
    queue_io::with_post_enqueue_idle_queue_kick_suppressed(fixture.enqueue("status?")).await;
    drop(transition);
    for _ in 0..2 {
        std::fs::remove_file(fixture.pane.path("pasted")).unwrap();
        fixture.kick().await;
    }
    assert_eq!(fixture.pane.keys().len(), 6, "{:?}", fixture.pane.keys());
    let again = std::fs::read_to_string(fixture.pane.path("buffer")).unwrap();
    assert_eq!(again, first);
    assert!(fixture.queue().await.is_empty());
    fixture.assert_holder_kept().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_actual_router_kickoff_opens_both_gates_for_the_same_durable_head_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_813_422, "all").await;
    let enabled = inject_hook::forced(fixture.channel.get()).unwrap();
    inject_hook::clear(fixture.channel.get());
    inject_hook::set_queued_head(fixture.channel.get(), true);
    let before = fixture.queue().await;
    let before_bytes = fixture.bytes();
    let before_queue = format!("{before:?}");
    for global_open in [false, true] {
        if global_open {
            inject_hook::set(fixture.channel.get(), enabled.0, enabled.1.clone());
        }
        fixture.kick().await;
        assert_eq!(fixture.pane.tmux_calls(), 0, "global_open={global_open}");
        assert_eq!(fixture.bytes(), before_bytes);
        assert_eq!(format!("{:?}", fixture.queue().await), before_queue);
        fixture.assert_holder_kept().await;
    }

    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    assert!(router::queued_head_inject_covers(fixture.channel, None));
    fixture.kick().await;
    assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);
    assert!(fixture.pane.transcript_recorded_the_paste());
    let buffer = std::fs::read_to_string(fixture.pane.path("buffer")).unwrap();
    assert_eq!(
        buffer,
        format!("[User: {AUTHOR_NAME} (ID: 200)] {}", before[0].text)
    );
    assert!(!buffer.contains("distinct tail B"));
    let transcript = std::fs::read_to_string(fixture.pane.path(InjectPane::TRANSCRIPT)).unwrap();
    let acknowledged: Vec<Value> = transcript
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|record: &Value| record["type"] == "queue-operation")
        .collect();
    assert_eq!(acknowledged.len(), 1);
    assert_eq!(acknowledged[0]["content"], buffer);
    let after = fixture.queue().await;
    assert_eq!(after.len(), 1);
    assert_eq!(format!("{:?}", after[0]), format!("{:?}", before[1]));
    let disk = fixture.disk();
    assert!(disk["head_injection"].is_null());
    let terminal = disk["terminal_identities"].as_array().unwrap();
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0]["primary"], before[0].message_id.get());
    assert_eq!(
        terminal[0]["queued_generation"],
        before[0].queued_generation
    );
    assert_eq!(terminal[0]["sources"].as_array().unwrap().len(), 1);
    assert_eq!(
        terminal[0]["sources"][0]["message_id"],
        before[0].message_id.get()
    );
    assert_eq!(
        terminal[0]["sources"][0]["queued_generation"],
        before[0].queued_generation
    );
    fixture.assert_holder_kept().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_actual_router_kickoff_not_sent_preserves_the_original_head_without_handback_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_813_404, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    fixture.pane.menu();
    let before_queue = format!("{:?}", fixture.queue().await);
    let before_disk = payloads(items(fixture.disk()));
    fixture.kick().await;
    assert!(
        fixture.pane.tmux_calls() > 0,
        "the actual executor must read the menu before any key"
    );
    assert!(fixture.pane.keys().is_empty());
    assert_eq!(format!("{:?}", fixture.queue().await), before_queue);
    assert_eq!(payloads(items(fixture.disk())), before_disk);
    let disk = fixture.disk();
    assert!(disk.is_array() || disk["head_injection"].is_null());
    assert!(disk.is_array() || disk["terminal_identities"].as_array().unwrap().is_empty());
    fixture.assert_holder_kept().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_actual_router_kickoff_unconfirmed_preserves_head_and_tail_across_restart_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_813_405, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    fixture.pane.set("fail_paste", "");
    let before = payloads(items(fixture.disk()));
    fixture.kick().await;
    assert!(fixture.pane.tmux_calls() > 0);
    assert!(fixture.pane.keys().is_empty());
    assert_eq!(payloads(items(fixture.disk())), before);
    assert_eq!(fixture.disk()["head_injection"]["phase"], "unknown");
    let original = fixture.bytes();
    let calls = fixture.pane.tmux_calls();
    fixture.kick().await;
    assert_eq!(
        fixture.pane.tmux_calls(),
        calls,
        "UNKNOWN cannot retry the executor"
    );
    assert_eq!(fixture.bytes(), original);
    let restarted = ChannelMailboxRegistry::default().handle(fixture.channel);
    let persistence = crate::services::discord::queue_persistence_context(
        &fixture.shared,
        &ProviderKind::Claude,
        fixture.channel,
    );
    let take = restarted.take_next_soft(persistence).await;
    assert!(
        take.intervention.is_none(),
        "a restarted reader cannot skip UNKNOWN H to B"
    );
    assert!(take.persistence_error.is_none());
    assert_eq!(fixture.bytes(), original);
    fixture.assert_holder_kept().await;
}

/// Claude Code's paste block around `inner`, opened and closed with the given ids.
fn paste_block(open: &str, inner: &str, close: &str) -> String {
    format!("<pasted_content id=\"{open}\">\n{inner}\n</pasted_content id=\"{close}\">")
}

/// The pane's enqueue receipt with `content` as its prompt.
fn enqueue_receipt(content: Value) -> String {
    json!({"type": "queue-operation", "operation": "enqueue", "content": content}).to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_actual_router_kickoff_settles_a_head_claude_recorded_in_a_paste_block_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_813_430, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let block = paste_block("b0ac", "@BODY@", "b0ac");
    fixture.pane.set("receipt", &enqueue_receipt(json!(block)));
    let before = fixture.queue().await;
    fixture.kick().await;
    assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);
    let buffer = std::fs::read_to_string(fixture.pane.path("buffer")).unwrap();
    assert_eq!(buffer, format!("[User: {AUTHOR_NAME} (ID: 200)] status?"));
    let disk = fixture.disk();
    assert!(disk["head_injection"].is_null(), "{disk}");
    let after = fixture.queue().await;
    assert_eq!(after.len(), 1);
    assert_eq!(format!("{:?}", after[0]), format!("{:?}", before[1]));

    // The composer is empty again, so the next kick types B once in the same way.
    std::fs::remove_file(fixture.pane.path("pasted")).unwrap();
    fixture.kick().await;
    assert_eq!(
        fixture.pane.keys(),
        ["paste-buffer", "send-keys", "paste-buffer", "send-keys"]
    );
    let buffer = std::fs::read_to_string(fixture.pane.path("buffer")).unwrap();
    assert_eq!(
        buffer,
        format!("[User: {AUTHOR_NAME} (ID: 200)] distinct tail B")
    );
    assert!(fixture.queue().await.is_empty());
    let disk = fixture.disk();
    assert!(disk["head_injection"].is_null(), "{disk}");
    let terminal = disk["terminal_identities"].as_array().unwrap();
    assert_eq!(terminal.len(), 2);
    for (identity, head) in terminal.iter().zip(&before) {
        assert_eq!(identity["primary"], head.message_id.get());
        assert_eq!(identity["queued_generation"], head.queued_generation);
        assert_eq!(identity["sources"].as_array().unwrap().len(), 1);
        assert_eq!(identity["sources"][0]["message_id"], head.message_id.get());
    }
    fixture.assert_holder_kept().await;
}

/// Enter settles the head whatever the transcript records after it, so no receipt can hold it
/// for a resend; the next kick types the tail.
#[tokio::test(flavor = "current_thread")]
async fn s2b_actual_router_kickoff_settles_the_head_at_enter_whatever_receipt_follows_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let block = |inner: &str| paste_block("b0ac", inner, "b0ac");
    let quoted = block("@BODY@");
    let cases = [
        ("suffix inside", enqueue_receipt(json!(block("@BODY@\nX")))),
        (
            "changed body",
            enqueue_receipt(json!(block("@HEAD@\nstatus!"))),
        ),
        (
            "assistant quote",
            json!({"type": "assistant", "message": {"content": quoted}}).to_string(),
        ),
    ];
    for (offset, (case, receipt)) in cases.into_iter().enumerate() {
        let fixture = Fixture::new(6_813_431 + offset as u64, "all").await;
        let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
        fixture.pane.set("receipt", &receipt);
        let before = fixture.queue().await;
        fixture.kick().await;
        assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"], "{case}");
        let disk = fixture.disk();
        assert!(disk["head_injection"].is_null(), "{case}: {disk}");
        let terminal = disk["terminal_identities"].as_array().unwrap();
        assert_eq!(terminal.len(), 1, "{case}");
        assert_eq!(terminal[0]["primary"], before[0].message_id.get(), "{case}");
        let after = fixture.queue().await;
        assert_eq!(
            format!("{after:?}"),
            format!("{:?}", &before[1..]),
            "{case}"
        );
        std::fs::remove_file(fixture.pane.path("pasted")).unwrap();
        fixture.kick().await;
        let buffer = std::fs::read_to_string(fixture.pane.path("buffer")).unwrap();
        assert_eq!(
            buffer,
            format!("[User: {AUTHOR_NAME} (ID: 200)] distinct tail B"),
            "{case}: the settled head is never typed again"
        );
        assert!(fixture.queue().await.is_empty(), "{case}");
        fixture.assert_holder_kept().await;
    }
}

async fn reached(signal: &tokio::sync::Notify) {
    tokio::time::timeout(std::time::Duration::from_secs(10), signal.notified())
        .await
        .expect("the actual queued owner must reach its durable phase");
}

async fn finish_without_an_unnecessary_may_write(
    mut caller: tokio::task::JoinHandle<()>,
    monitor: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
) {
    if let Some((at_may, resume_may)) = monitor {
        let entered_may = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::select! {
                result = &mut caller => { result.unwrap(); false },
                () = at_may.notified() => {
                    resume_may.notify_one();
                    caller.await.unwrap();
                    true
                },
            }
        })
        .await
        .expect("the changed target must release its reservation");
        assert!(
            !entered_may,
            "a target change already known at RESERVED cannot enter MAY_HAVE_SENT"
        );
    } else {
        caller.await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_cancelled_kickoff_caller_leaves_the_reserved_owner_to_finish_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_813_406, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let head = fixture.queue().await.remove(0);
    let (at_reserved, resume) = inject_hook::park_before_reserve(fixture.channel.get());
    let caller = fixture.spawn_kick();
    reached(&at_reserved).await;
    assert_eq!(fixture.disk()["head_injection"]["phase"], "reserved");
    assert_eq!(fixture.pane.tmux_calls(), 0);
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    resume.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while fixture.disk()["terminal_identities"]
            .as_array()
            .is_none_or(|identities| identities.is_empty())
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the detached owner must settle after its caller is cancelled");
    assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);
    assert!(fixture.pane.transcript_recorded_the_paste());
    let after = fixture.queue().await;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].text, "distinct tail B");
    assert_eq!(
        fixture.disk()["terminal_identities"][0]["primary"],
        head.message_id.get()
    );
    assert_eq!(
        fixture.disk()["terminal_identities"][0]["queued_generation"],
        head.queued_generation
    );
    fixture.assert_holder_kept().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_reserved_and_may_have_sent_owners_recheck_the_row_before_pane_io_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, phase, idle) in [
        (6_813_407, "reserved", false),
        (6_813_408, "may_have_sent", false),
        (6_813_427, "reserved", true),
        (6_813_428, "may_have_sent", true),
    ] {
        let fixture = Fixture::new(channel, "all").await;
        let _gate = router::busy_inject_support::open_gate(channel);
        let before = payloads(items(fixture.disk()));
        let before_payload_bytes = serde_json::to_vec(&before).unwrap();
        let before_queue = format!("{:?}", fixture.queue().await);
        let (park, resume) = if phase == "reserved" {
            inject_hook::park_before_reserve(channel)
        } else {
            inject_hook::park_queued(channel, "may_have_sent")
        };
        let forbidden_may =
            (phase == "reserved").then(|| inject_hook::park_queued(channel, "may_have_sent"));
        let caller = fixture.spawn_kick();
        reached(&park).await;
        assert_eq!(fixture.disk()["head_injection"]["phase"], phase);
        assert_eq!(fixture.pane.tmux_calls(), 0);
        if idle {
            let transcript_path = fixture.pane.path(InjectPane::TRANSCRIPT);
            assert!(
                crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&transcript_path)
                    .is_busy()
            );
            let transcript = std::fs::read_to_string(&transcript_path).unwrap();
            fixture.pane.set(
                InjectPane::TRANSCRIPT,
                &format!("{transcript}{{\"type\":\"result\"}}\n"),
            );
            fixture.pane.idle();
            assert_eq!(
                crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&transcript_path),
                crate::services::tui_turn_state::TuiTurnState::Idle
            );
        } else {
            fixture
                .pane
                .reseat_row(TurnSource::Managed, fixture.holder_message.get() + 1);
        }
        resume.notify_one();
        finish_without_an_unnecessary_may_write(caller, forbidden_may).await;
        assert_eq!(
            fixture.pane.tmux_calls(),
            0,
            "{phase}/idle={idle}: the changed target must veto before pane I/O"
        );
        assert_eq!(format!("{:?}", fixture.queue().await), before_queue);
        assert_eq!(payloads(items(fixture.disk())), before);
        assert_eq!(
            serde_json::to_vec(&payloads(items(fixture.disk()))).unwrap(),
            before_payload_bytes
        );
        let disk = fixture.disk();
        assert!(disk.is_array() || disk["head_injection"].is_null());
        let rows = ATTEMPT_ROWS.lock().unwrap().clone();
        let row = rows.iter().rev().find(|row| row.channel_id == channel);
        assert_eq!(
            row.map(|row| (row.outcome, row.head_state)),
            Some(("not_sent", "queued")),
            "{phase}/idle={idle}: the actor committed the head back to the queue"
        );
        let snapshot = fixture.shared.mailbox(fixture.channel).snapshot().await;
        assert!(
            snapshot
                .cancel_token
                .as_ref()
                .is_some_and(|token| Arc::ptr_eq(token, &fixture.holder))
        );
        assert_eq!(
            snapshot.active_user_message_id,
            Some(fixture.holder_message)
        );
        let row = crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            fixture.channel.get(),
        )
        .unwrap();
        assert_eq!(
            row.user_msg_id,
            fixture.holder_message.get() + u64::from(!idle)
        );
        if idle {
            fixture.assert_holder_kept().await;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_reserved_owner_panic_keeps_unknown_and_never_retries_the_executor_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_813_409, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let before = payloads(items(fixture.disk()));
    inject_hook::crash_owner(fixture.channel.get());
    fixture.kick().await;
    assert_eq!(fixture.pane.tmux_calls(), 0);
    fixture.assert_unknown_barrier(before).await;
    fixture.assert_holder_kept().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_may_have_sent_save_failure_keeps_unknown_before_any_executor_call_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_813_410, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let before = payloads(items(fixture.disk()));
    let (at_reserved, resume) = inject_hook::park_before_reserve(fixture.channel.get());
    let caller = fixture.spawn_kick();
    reached(&at_reserved).await;
    assert_eq!(fixture.disk()["head_injection"]["phase"], "reserved");
    crate::services::turn_orchestrator::test_support::fail_queue_saves(fixture.channel, 1);
    assert_eq!(
        crate::services::turn_orchestrator::test_support::queue_save_faults(fixture.channel, false),
        1
    );
    resume.notify_one();
    caller.await.unwrap();
    assert_eq!(
        crate::services::turn_orchestrator::test_support::queue_save_faults(fixture.channel, true),
        0
    );
    assert_eq!(
        fixture.pane.tmux_calls(),
        0,
        "a failed MAY write cannot reach the executor"
    );
    fixture.assert_unknown_barrier(before).await;
    fixture.assert_holder_kept().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_acknowledged_input_with_a_failed_consume_commit_keeps_unknown_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_813_411, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let before = payloads(items(fixture.disk()));
    let (after_effect, resume) = inject_hook::park_queued(fixture.channel.get(), "after_effect");
    let caller = fixture.spawn_kick();
    reached(&after_effect).await;
    assert_eq!(fixture.disk()["head_injection"]["phase"], "may_have_sent");
    assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);
    assert!(fixture.pane.transcript_recorded_the_paste());
    crate::services::turn_orchestrator::test_support::fail_queue_saves(fixture.channel, 1);
    resume.notify_one();
    caller.await.unwrap();
    assert_eq!(
        crate::services::turn_orchestrator::test_support::queue_save_faults(fixture.channel, true),
        0
    );
    fixture.assert_unknown_barrier(before).await;
    fixture.assert_holder_kept().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_an_acknowledged_paste_can_settle_after_the_transcript_becomes_idle_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_813_412, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let head = fixture.queue().await.remove(0);
    let (after_effect, resume) = inject_hook::park_queued(fixture.channel.get(), "after_effect");
    let caller = fixture.spawn_kick();
    reached(&after_effect).await;
    assert_eq!(fixture.disk()["head_injection"]["phase"], "may_have_sent");
    assert!(fixture.pane.transcript_recorded_the_paste());
    fixture.pane.idle();
    let transcript = std::fs::read_to_string(fixture.pane.path(InjectPane::TRANSCRIPT)).unwrap();
    fixture.pane.set(
        InjectPane::TRANSCRIPT,
        &format!("{transcript}{{\"type\":\"result\"}}\n"),
    );
    assert_eq!(
        crate::services::tui_turn_state::observe_claude_jsonl_turn_state(
            &fixture.pane.path(InjectPane::TRANSCRIPT)
        ),
        crate::services::tui_turn_state::TuiTurnState::Idle
    );
    resume.notify_one();
    caller.await.unwrap();
    assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);
    assert_eq!(fixture.queue().await.len(), 1);
    assert_eq!(fixture.queue().await[0].text, "distinct tail B");
    assert_eq!(
        fixture.disk()["terminal_identities"][0]["primary"],
        head.message_id.get()
    );
    assert!(fixture.disk()["head_injection"].is_null());
    fixture.assert_holder_kept().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_reserved_and_may_owners_detect_same_path_binding_replacement_before_pane_io_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, phase, field) in [
        (6_813_414, "reserved", "session_id"),
        (6_813_415, "reserved", "input_fifo_path"),
        (6_813_416, "may_have_sent", "session_id"),
        (6_813_417, "may_have_sent", "input_fifo_path"),
    ] {
        let fixture = Fixture::new(channel, "all").await;
        let _gate = router::busy_inject_support::open_gate(channel);
        let before = payloads(items(fixture.disk()));
        let before_queue = format!("{:?}", fixture.queue().await);
        let (park, resume) = if phase == "reserved" {
            inject_hook::park_before_reserve(channel)
        } else {
            inject_hook::park_queued(channel, "may_have_sent")
        };
        let forbidden_may =
            (phase == "reserved").then(|| inject_hook::park_queued(channel, "may_have_sent"));
        let caller = fixture.spawn_kick();
        reached(&park).await;
        assert_eq!(fixture.disk()["head_injection"]["phase"], phase);
        replace_same_path_binding(&fixture, field);
        resume.notify_one();
        finish_without_an_unnecessary_may_write(caller, forbidden_may).await;
        assert_eq!(fixture.pane.tmux_calls(), 0, "{phase}/{field}");
        assert_eq!(format!("{:?}", fixture.queue().await), before_queue);
        assert_eq!(payloads(items(fixture.disk())), before);
        let disk = fixture.disk();
        assert!(disk.is_array() || disk["head_injection"].is_null());
        fixture.assert_holder_kept().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_acknowledged_owner_keeps_unknown_when_same_path_binding_changes_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, field) in [(6_813_418, "session_id"), (6_813_419, "input_fifo_path")] {
        let fixture = Fixture::new(channel, "all").await;
        let _gate = router::busy_inject_support::open_gate(channel);
        let before = payloads(items(fixture.disk()));
        let (after_effect, resume) = inject_hook::park_queued(channel, "after_effect");
        let caller = fixture.spawn_kick();
        reached(&after_effect).await;
        assert_eq!(fixture.disk()["head_injection"]["phase"], "may_have_sent");
        assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);
        assert!(fixture.pane.transcript_recorded_the_paste());
        replace_same_path_binding(&fixture, field);
        resume.notify_one();
        caller.await.unwrap();
        fixture.assert_unknown_barrier(before).await;
        fixture.assert_holder_kept().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_actor_rechecks_both_gates_and_the_admitted_channel_before_reserving_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, mode, list_open, wrong_channel) in [
        (6_813_420, "off", true, false),
        (6_813_422, "all", false, false),
        (6_813_424, "all", true, true),
    ] {
        let fixture = Fixture::new(channel, mode).await;
        let _gate = list_open.then(|| router::busy_inject_support::open_gate(channel));
        let other = wrong_channel.then(|| InjectPane::new(channel + 1, "all"));
        let _other_gate =
            wrong_channel.then(|| router::busy_inject_support::open_gate(channel + 1));
        let admitted_channel = ChannelId::new(channel + u64::from(wrong_channel));
        let gate = router::queued_head_inject_covers;
        assert_eq!(gate(admitted_channel, None), list_open);
        let before = fixture.bytes();
        let queue = format!("{:?}", fixture.queue().await);
        let head = fixture.queue().await.remove(0);
        let persistence = crate::services::discord::queue_persistence_context(
            &fixture.shared,
            &ProviderKind::Claude,
            fixture.channel,
        );
        let claimed = fixture
            .shared
            .mailbox(fixture.channel)
            .claim_queued_head_if_enabled(
                QueueIdentity::of(&head),
                persistence,
                admitted_channel,
                None,
                gate,
            )
            .await;
        assert!(
            matches!(claimed, QueuedHeadClaim::NotEligible),
            "actor admission must reject mode={mode}, list={list_open}, wrong_channel={wrong_channel}"
        );
        assert_eq!(fixture.bytes(), before);
        assert_eq!(format!("{:?}", fixture.queue().await), queue);
        assert_eq!(fixture.pane.tmux_calls(), 0);
        if let Some(other) = other {
            assert_eq!(other.tmux_calls(), 0);
        }
        fixture.assert_holder_kept().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s2b_a_definite_not_sent_releases_the_head_even_if_the_row_changes_after_the_effect_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_813_426, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    fixture.pane.menu();
    let before = payloads(items(fixture.disk()));
    let before_queue = format!("{:?}", fixture.queue().await);
    let (after_effect, resume) = inject_hook::park_queued(fixture.channel.get(), "after_effect");
    let caller = fixture.spawn_kick();
    reached(&after_effect).await;
    assert_eq!(fixture.disk()["head_injection"]["phase"], "may_have_sent");
    assert!(fixture.pane.tmux_calls() > 0);
    assert!(fixture.pane.keys().is_empty());
    fixture
        .pane
        .reseat_row(TurnSource::Managed, fixture.holder_message.get() + 1);
    resume.notify_one();
    caller.await.unwrap();
    assert!(fixture.pane.keys().is_empty());
    assert_eq!(format!("{:?}", fixture.queue().await), before_queue);
    assert_eq!(payloads(items(fixture.disk())), before);
    let disk = fixture.disk();
    assert!(
        disk.is_array() || disk["head_injection"].is_null(),
        "a proven NotSent must release H rather than quarantine it as UNKNOWN"
    );
    let snapshot = fixture.shared.mailbox(fixture.channel).snapshot().await;
    assert!(
        snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, &fixture.holder))
    );
    assert_eq!(
        snapshot.active_user_message_id,
        Some(fixture.holder_message)
    );
    let row = crate::services::discord::inflight::load_inflight_state_read_only(
        &ProviderKind::Claude,
        fixture.channel.get(),
    )
    .unwrap();
    assert_eq!(row.user_msg_id, fixture.holder_message.get() + 1);
}

/// Through the real deliver route, a busy pane takes a new input past a backlog that is all
/// parked, whether the live actor holds it or only the disk does; the parked inputs stay queued.
#[tokio::test(flavor = "current_thread")]
async fn a_deliver_passes_a_parked_only_backlog_and_leaves_it_queued_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    // The live actor holds the backlog in memory and on disk; one fixture runs at a time.
    {
        let fixture = Fixture::new(6_897_401, "all").await;
        let queued: Vec<MessageId> = fixture.queue().await.iter().map(|i| i.message_id).collect();
        fixture.park(&queued[..1]);
        let behind_ready = fixture.deliver("waits behind a ready input").await;
        assert_eq!(behind_ready["delivery"], "queued", "{behind_ready}");
        let queued: Vec<MessageId> = fixture.queue().await.iter().map(|i| i.message_id).collect();
        fixture.park(&queued);
        let disk = fixture.bytes();
        let body = fixture.deliver("C after parked").await;
        assert_eq!(body["delivery"], "injected", "{body}");
        assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);
        let buffer = std::fs::read_to_string(fixture.pane.path("buffer")).unwrap();
        assert_eq!(buffer, "[User: imessage:200 (ID: 200)] C after parked");
        let after: Vec<MessageId> = fixture.queue().await.iter().map(|i| i.message_id).collect();
        assert_eq!(after, queued);
        assert_eq!(
            payloads(items(fixture.disk())),
            payloads(items(serde_json::from_slice(&disk).unwrap()))
        );
        let retries = crate::services::discord::busy_followup_retry_store::is_capped;
        assert!(queued.iter().all(|id| retries(
            &ProviderKind::Claude,
            fixture.channel.get(),
            id.get()
        )));
    }
    // A fresh actor that has not loaded the disk queue.
    let fresh = Fixture::new(6_897_402, "all").await;
    let on_disk: Vec<MessageId> = fresh.queue().await.iter().map(|i| i.message_id).collect();
    fresh.park(&on_disk);
    fresh
        .shared
        .mailboxes
        .remove_fixture_for_test(fresh.channel);
    let actor = fresh.shared.mailbox(fresh.channel);
    let holder = (fresh.holder.clone(), UserId::new(100), fresh.holder_message);
    assert!(actor.try_start_turn(holder.0, holder.1, holder.2).await);
    assert!(
        fresh.queue().await.is_empty(),
        "the fresh actor has not loaded the disk queue"
    );
    let body = fresh.deliver("D past a disk-only backlog").await;
    assert_eq!(body["delivery"], "injected", "{body}");
    assert_eq!(fresh.pane.keys(), ["paste-buffer", "send-keys"]);
}

/// A parked head is neither offered to the injector by the queued-head kickoff nor reserved by the
/// actor, and a head already reserved holds later input back even when every input is parked.
#[tokio::test(flavor = "current_thread")]
async fn a_parked_head_is_never_typed_and_a_reserved_head_still_holds_the_order_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_897_411, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let queue = fixture.queue().await;
    let head = queue[0].clone();
    let ids: Vec<MessageId> = queue.iter().map(|i| i.message_id).collect();
    // Every input is parked: a ready one behind the parked head would be typed in its place.
    fixture.park(&ids);
    let before = fixture.bytes();
    fixture.kick().await;
    assert!(!OFFERED_HEADS.lock().unwrap().contains(&head.message_id));
    assert_eq!(fixture.pane.tmux_calls(), 0);
    assert_eq!(fixture.bytes(), before);
    let persistence = crate::services::discord::queue_persistence_context(
        &fixture.shared,
        &ProviderKind::Claude,
        fixture.channel,
    );
    let actor = fixture.shared.mailbox(fixture.channel);
    let identity = QueueIdentity::of(&head);
    let claimed = actor
        .claim_queued_head(identity.clone(), persistence.clone())
        .await;
    assert!(matches!(claimed, QueuedHeadClaim::NotEligible));
    assert_eq!(fixture.bytes(), before);
    fixture.assert_holder_kept().await;

    // Reserve the head while it is ready, then park every queued input behind the reservation.
    let record = runtime_store::discord_busy_followup_retries_root()
        .unwrap()
        .join(format!(
            "claude/{}/{}.json",
            fixture.channel, head.message_id
        ));
    std::fs::remove_file(record).unwrap();
    let QueuedHeadClaim::Reserved(_ticket) = actor.claim_queued_head(identity, persistence).await
    else {
        panic!("a ready head is reserved");
    };
    fixture.park(&ids);
    let body = fixture.deliver("C behind a reserved head").await;
    assert_eq!(body["delivery"], "queued", "{body}");
    assert_eq!(fixture.pane.tmux_calls(), 0);
}

/// A menu over a busy pane keeps the queued head and types nothing, however often it is kicked;
/// the person is told once.
#[tokio::test(flavor = "current_thread")]
async fn menu_keeps_the_queued_head_without_any_key_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(7_314_201, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let original = std::fs::read_to_string(fixture.pane.path("cap.before")).unwrap();
    fixture.pane.set(
        "cap.before",
        &format!("Run this tool? allow / deny\n{original}"),
    );
    let before = fixture.queue().await;
    let deps = router::IntakeDeps {
        http: &fixture.http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &fixture.shared,
        token: "s2b-test-token",
    };
    for _ in 0..2 {
        assert!(
            super::try_busy(
                &deps,
                &ProviderKind::Claude,
                fixture.channel,
                None,
                Some(|_, _| true),
                "menu-held"
            )
            .await
        );
        assert!(fixture.pane.keys().is_empty());
        assert_eq!(fixture.queue().await[0].message_id, before[0].message_id);
        assert_eq!(fixture.queue().await.len(), before.len());
    }
    assert_eq!(fixture.preparation_notices().await, 1);
    assert!(!crate::services::claude_tui::composer_lock::draft_guarded(
        &fixture.pane.session()
    ));
}

/// On a transcript-turn channel an idle pane whose composer cannot be read still takes the
/// queued head: paste and Enter, the head leaves the queue, and nobody is told to wait.
#[tokio::test(flavor = "current_thread")]
async fn unread_idle_composer_consumes_the_queued_head_without_notice_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(7_314_202, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let _turns = crate::services::tui_o::turn_mode::TestConfirmation::new(fixture.channel.get());
    fixture.pane.set("cap.before", "unrecognized composer");
    fixture.pane.set(
        InjectPane::TRANSCRIPT,
        "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
    );
    let before = fixture.queue().await;
    let deps = router::IntakeDeps {
        http: &fixture.http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &fixture.shared,
        token: "s2b-test-token",
    };
    assert!(
        super::try_busy(
            &deps,
            &ProviderKind::Claude,
            fixture.channel,
            None,
            Some(|_, _| true),
            "idle-unread"
        )
        .await
    );
    assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);
    let after = fixture.queue().await;
    assert_eq!(format!("{after:?}"), format!("{:?}", &before[1..]));
    assert_eq!(fixture.preparation_notices().await, 0);
    assert!(!crate::services::claude_tui::composer_lock::draft_guarded(
        &fixture.pane.session()
    ));
}

#[cfg(unix)]
#[path = "queued_head_owner_entry_tests.rs"]
mod owner_entry;
