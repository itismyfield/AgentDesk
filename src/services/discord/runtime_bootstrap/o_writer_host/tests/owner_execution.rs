use super::*;
use crate::db::{auto_queue::test_support::TestPostgresDb, intake_outbox as outbox};
use crate::services::cluster::intake_router_hook::owner_record::{input_start, output};
use crate::services::tui_o::writer::{DiscordPort, PostOutcome};
use crate::services::tui_o::{channel_policy::BootChannels, cutover, ownership::execution};
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Discord {
    http: Arc<serenity::Http>,
    posts: Arc<Mutex<Vec<Value>>>,
    server: tokio::task::AbortHandle,
}
impl Discord {
    async fn new() -> Self {
        let posts = Arc::new(Mutex::new(Vec::new()));
        let recorded = posts.clone();
        let app = axum::Router::new().fallback(axum::routing::any(move |method:axum::http::Method,uri:axum::http::Uri,body:axum::body::Bytes| {
            let recorded=recorded.clone();async move {
                if uri.path().ends_with("/users/@me") { return axum::Json(json!({"id":"1","username":"fixture","discriminator":"0001","avatar":null,"bot":true})); }
                if method==axum::http::Method::GET { return axum::Json(json!([])); }
                assert_eq!(method,axum::http::Method::POST);
                let payload:Value=serde_json::from_slice(&body).unwrap();recorded.lock().unwrap().push(payload.clone());
                let channel=uri.path().split('/').nth(4).unwrap();
                axum::Json(json!({"id":"9001","channel_id":channel,"content":payload["content"],
                    "author":{"id":"1","username":"fixture","discriminator":"0001","avatar":null,"bot":true},
                    "timestamp":"2026-10-28T00:00:00+00:00","edited_timestamp":null,"tts":false,
                    "mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy(format!("http://{}", listener.local_addr().unwrap()))
                .default_allowed_mentions(
                    serenity::CreateAllowedMentions::new()
                        .everyone(true)
                        .all_roles(true),
                )
                .ratelimiter_disabled(true)
                .build(),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            http,
            posts,
            server: server.abort_handle(),
        }
    }
}
impl Drop for Discord {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn select(
    provider: &ProviderKind,
    channel: u64,
    local: &str,
    preferred: &str,
) -> cutover::test_override::ChannelsGuard {
    let config=serde_json::from_value(json!({"server":{},"cluster":{"enabled":true,"instance_id":local,"gateway_preferred_instance_id":preferred},
        "agents":[{"id":"custody-agent","name":"fixture","channels":{(provider.as_str()):{"id":channel.to_string(),"runtime":"tui"}}}],
        "tui_o":{"writer":{"channels":[channel]}}})).unwrap();
    cutover::test_override::force_boot(BootChannels::validate(&config).unwrap())
}
async fn runtime(
    pool: &sqlx::PgPool,
    provider: ProviderKind,
    channel: u64,
    role: &str,
) -> (Arc<health::HealthRegistry>, Arc<SharedData>) {
    let registry = Arc::new(health::HealthRegistry::new());
    let mut shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let data = Arc::get_mut(&mut shared).unwrap();
    data.provider = provider.clone();
    data.health_registry = Arc::downgrade(&registry);
    shared.settings.write().await.allowed_channel_ids = vec![channel];
    shared
        .http
        .cached_bot_token
        .set("test-token".into())
        .unwrap();
    match role {
        "gateway" => {
            registry
                .register(provider.as_str().into(), shared.clone())
                .await
        }
        "standby" => {
            registry
                .register_standby(provider.as_str().into(), shared.clone())
                .await
        }
        _ => {
            registry
                .register_worker(provider.as_str().into(), shared.clone())
                .await
        }
    };
    (registry, shared)
}
async fn origin(
    pool: &sqlx::PgPool,
    provider: &str,
    channel: u64,
    local: &str,
    delivery: i64,
) -> output::Origin {
    let row = outbox::claim_pending_for_target_except(pool, local, provider, "owner-http", &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.id, delivery);
    assert!(
        outbox::mark_accepted(pool, delivery, "owner-http")
            .await
            .unwrap()
    );
    assert!(
        outbox::mark_spawned(pool, delivery, "owner-http")
            .await
            .unwrap()
    );
    use crate::services::tui_prompt_dedupe::binding_context::{
        BindingContext, PreparedIncarnation,
    };
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let owner = format!("owner-http-native-{channel}");
    let root = crate::services::tmux_common::current_tmux_owner_marker();
    let _context = PreparedIncarnation::create(BindingContext {
        schema: 1,
        provider: provider.into(),
        created_at: chrono::Utc::now(),
        execution_nonce: nonce.clone(),
        tmux_session: owner.clone(),
        channel_id: Some(channel),
        owner_runtime_root: root.clone(),
        host: Some(local.into()),
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: None,
        first_prompt_digest: None,
        source_policy: None,
    })
    .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(&owner, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, &nonce).unwrap();
    let binding =
        json!({"host":"tmux","endpoint":local,"owner":owner,"root":root,"execution_nonce":nonce});
    let permit = input_start::prepare(
        pool,
        input_start::StartRequest {
            delivery_id: delivery,
            executor: local,
            claim_owner: "owner-http",
            incarnation: "owner-http-runtime",
            frame: "frame",
            native_binding: &binding,
        },
    )
    .await
    .unwrap();
    let origin = permit.output_origin();
    let submitted = Arc::new(AtomicUsize::new(0));
    let count = submitted.clone();
    permit
        .submit("frame", &binding, move |_| async move {
            count.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .unwrap();
    assert_eq!(submitted.load(Ordering::SeqCst), 1);
    origin
}

#[tokio::test]
async fn owner_http_symmetric_real_rest_uses_execution_scope_without_gateway_context_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_http_symmetric_real_rest_uses_execution_scope_without_gateway_context_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let discord = Discord::new().await;
    let mut n = 0;
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        for (local, peer) in [("mac-a", "mac-b"), ("mac-b", "mac-a")] {
            for role in ["gateway", "standby", "worker"] {
                n += 1;
                let channel = 73369400 + n;
                let _selected = select(&provider, channel, local, peer);
                let delivery =
                    execution::tests::admission(&pool, provider.as_str(), channel, local).await;
                let permit =
                    execution::read(&pool, provider.as_str(), channel, local, || Ok(false))
                        .await
                        .unwrap()
                        .unwrap();
                let snapshot = cutover::boot_snapshot().unwrap();
                snapshot.install_execution(channel, Some(Ok(permit)));
                let (_registry, shared) = runtime(&pool, provider.clone(), channel, role).await;
                assert!(shared.http.cached_serenity_ctx.get().is_none());
                let parts = parts_with_rest(&shared, &provider, None, Some(discord.http.clone()));
                let port = tokio::time::timeout(Duration::from_secs(3), parts.io.port_for(channel))
                    .await
                    .unwrap()
                    .unwrap();
                let original = origin(&pool, provider.as_str(), channel, local, delivery).await;
                let body = format!("{local} {role} {}", provider.as_str());
                assert!(
                    matches!(
                        port.post_origin(channel, body.clone(), Some(original.seal(&body)))
                            .await,
                        PostOutcome::Created(_)
                    ),
                    "{local}/{role} owner must POST through its real REST port"
                );
                let before = discord.posts.lock().unwrap().len();
                output::tests::revoke(&pool, &original).await;
                assert!(matches!(
                    port.post_origin(channel, body.clone(), Some(original.seal(&body)))
                        .await,
                    PostOutcome::Withheld(_)
                ));
                assert_eq!(discord.posts.lock().unwrap().len(), before);
            }
        }
    }
    assert_eq!(discord.posts.lock().unwrap().len(), 12);
    pool.close().await;
}

#[tokio::test]
async fn owner_host_fresh_claude_starts_without_gateway_and_restarts_same_pg_generation_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_host_fresh_claude_starts_without_gateway_and_restarts_same_pg_generation_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 73369450;
    let _selected = select(&ProviderKind::Claude, channel, "mac-b", "mac-a");
    let delivery = execution::tests::admission(&pool, "claude", channel, "mac-b").await;
    let snapshot = cutover::boot_snapshot().unwrap();
    let permit = execution::read(&pool, "claude", channel, "mac-b", || Ok(false))
        .await
        .unwrap()
        .unwrap();
    let local_gate = permit.gate();
    snapshot.install_execution(channel, Some(Ok(permit)));
    let discord = Discord::new().await;
    let ready = Arc::new(host::Readiness::default());
    let global = crate::services::tui_o::ownership::gate("claude");
    global.acquired();
    let mut original = None;
    for restart in [false, true] {
        let (_registry, shared) = runtime(
            &pool,
            ProviderKind::Claude,
            channel,
            if restart { "standby" } else { "gateway" },
        )
        .await;
        let mut prepared = parts_with_rest(
            &shared,
            &ProviderKind::Claude,
            None,
            Some(discord.http.clone()),
        );
        prepared.readiness = ready.clone();
        let io = prepared.io.clone();
        let mut handles = host::start_managed(
            ShadowProvider::Claude,
            !restart,
            cutover::boot_ownership(),
            || prepared,
        );
        assert_eq!(
            handles.len(),
            1,
            "a PG channel owner must host independently of the gateway lease"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready.accepts(channel) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(local_gate.current().epoch().is_some());
        if original.is_none() {
            original = Some(origin(&pool, "claude", channel, "mac-b", delivery).await);
        }
        let original = original.as_ref().unwrap();
        let port = io.port_for(channel).await.unwrap();
        let body = if restart {
            "after node-local restart"
        } else {
            "before gateway loss"
        };
        assert!(matches!(
            port.post_origin(channel, body.into(), Some(original.seal(body)))
                .await,
            PostOutcome::Created(_)
        ));
        if !restart {
            super::super::owner_execution::gateway_transport_lost(&shared);
            crate::services::tui_o::ownership::release_gateway(&global, async {}).await;
            assert_eq!(
                global.current(),
                crate::services::tui_o::ownership::GatewayOwnership::Lost
            );
            assert!(ready.accepts(channel));
            let body = "after gateway loss";
            assert!(matches!(
                port.post_origin(channel, body.into(), Some(original.seal(body)))
                    .await,
                PostOutcome::Created(_)
            ));
            let rebound = execution::read(&pool, "claude", channel, "mac-b", || {
                panic!("the original local binding must restore")
            })
            .await
            .unwrap()
            .unwrap();
            snapshot.install_execution(channel, Some(Ok(rebound)));
            assert!(
                Arc::ptr_eq(
                    &local_gate,
                    &snapshot.execution(channel).unwrap().permit.unwrap().gate()
                ),
                "first native binding must not close the running turn's gate"
            );
            assert!(ready.accepts(channel));
        }
        handles.pop().unwrap().stop_and_join().await.unwrap();
        assert!(!ready.is_hosted(channel));
    }
    let owner:i64=sqlx::query_scalar("SELECT generation FROM intake_session_owners WHERE provider='claude' AND raw_channel_id=$1 AND status='active'").bind(channel.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(owner, 0);
    assert_eq!(discord.posts.lock().unwrap().len(), 3);
    pool.close().await;
}

#[tokio::test]
async fn owner_http_unregistered_runtime_is_not_advertised_as_ready_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_http_unregistered_runtime_is_not_advertised_as_ready_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 73369460;
    let _selected = select(&ProviderKind::Claude, channel, "mac-a", "mac-b");
    execution::tests::admission(&pool, "claude", channel, "mac-a").await;
    let snapshot = cutover::boot_snapshot().unwrap();
    snapshot.install_execution(
        channel,
        Some(Ok(execution::read(
            &pool,
            "claude",
            channel,
            "mac-a",
            || Ok(false),
        )
        .await
        .unwrap()
        .unwrap())),
    );
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let discord = Discord::new().await;
    let parts = parts_with_rest(
        &shared,
        &ProviderKind::Claude,
        None,
        Some(discord.http.clone()),
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(6), parts.io.port_for(channel))
            .await
            .unwrap()
            .is_err()
    );
    assert!(discord.posts.lock().unwrap().is_empty());
    pool.close().await;
}

#[test]
fn owner_host_boot_wires_every_runtime_role_without_native_end_on_transport_loss() {
    let boot = include_str!("../../../runtime_bootstrap.rs");
    let spawn = "o_writer_host::spawn_execution(&shared, &provider, boot_config.tui_o.as_ref());";
    assert!(
        boot[boot.find("if !modules.gateway {").unwrap()..boot.find("let voice_receiver").unwrap()]
            .contains(spawn)
    );
    assert!(
        boot[boot.find("GatewayLeaseOutcome::Standby =>").unwrap()
            ..boot.find("GatewayLeaseOutcome::Failed =>").unwrap()]
            .contains(spawn)
    );
    let transport = include_str!("../../gateway_lease.rs");
    let fence = &transport[transport.find("async fn self_fence_gateway(").unwrap()
        ..transport
            .find("pub(super) fn run_bot_spawn_gateway_lease_keepalive(")
            .unwrap()];
    assert!(fence.contains("super::owner_execution::gateway_transport_lost(shared);"));
    assert!(fence.contains("mailbox_restart_drain_all(shared, provider)"));
    assert!(!fence.contains("transfer_owner"));
    assert!(!fence.contains("normal_end"));
}

#[tokio::test]
async fn owner_http_wire_mentions_override_unsafe_client_defaults() {
    let discord = Discord::new().await;
    let port = GatewayPort::new(discord.http.clone(), 1);
    let outcome = port.post(73369470, "@everyone <@&42> <@43>".into()).await;
    assert!(matches!(outcome, PostOutcome::Created(_)));
    let requests = discord.posts.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["allowed_mentions"]["parse"], json!(["users"]));
}

#[tokio::test]
async fn owner_host_fence_serializes_current_generation_with_one_connection_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_host_fence_serializes_current_generation_with_one_connection_pg"
    )) {
        return;
    }
    use crate::services::cluster::intake_router_hook::owner_record::{OwnerIdentity, custody};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let setup = db.connect_and_migrate().await;
    let channel = 73369471;
    let _selected = select(&ProviderKind::Claude, channel, "mac-b", "mac-a");
    execution::tests::admission(&setup, "claude", channel, "mac-b").await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(2))
        .connect(&db.database_url)
        .await
        .unwrap();
    let snapshot = cutover::boot_snapshot().unwrap();
    snapshot.install_execution(
        channel,
        Some(Ok(execution::read(
            &pool,
            "claude",
            channel,
            "mac-b",
            || Ok(false),
        )
        .await
        .unwrap()
        .unwrap())),
    );
    let (_registry, shared) = runtime(&pool, ProviderKind::Claude, channel, "standby").await;
    let io = parts_with_rest(&shared, &ProviderKind::Claude, None, None).io;
    let fence = tokio::time::timeout(
        Duration::from_secs(3),
        io.intake_fence(channel, ShadowProvider::Claude),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(fence.facts.open_intake, 0);
    let competing = setup.clone();
    let mut changing = tokio::spawn(async move {
        let mut tx = competing.begin().await.unwrap();
        custody::lock(&mut tx, &OwnerIdentity::new("claude", &channel.to_string()))
            .await
            .unwrap();
        sqlx::query("UPDATE intake_session_owners SET generation=generation+1 WHERE provider='claude' AND raw_channel_id=$1")
            .bind(channel.to_string()).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(75), &mut changing)
            .await
            .is_err()
    );
    fence.hold.release().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), changing)
        .await
        .unwrap()
        .unwrap();
    assert!(
        io.intake_fence(channel, ShadowProvider::Claude)
            .await
            .is_err(),
        "a pinned old generation must not initialize after ownership changes"
    );
    pool.close().await;
    assert!(
        io.intake_fence(channel, ShadowProvider::Claude)
            .await
            .is_err(),
        "PG errors must not restore old authority"
    );
    setup.close().await;
}

#[tokio::test]
async fn owner_cleanup_legacy_port_open_before_management_keeps_scope_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_cleanup_legacy_port_open_before_management_keeps_scope_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 73369120;
    let discord = Discord::new().await;
    let _selection = select(&ProviderKind::Claude, channel, "mac-a", "mac-a");
    let (_registry, shared) = runtime(&pool, ProviderKind::Claude, channel, "gateway").await;
    let host = parts_with_rest(
        &shared,
        &ProviderKind::Claude,
        None,
        Some(discord.http.clone()),
    );
    let port = host.io.port().await;
    assert!(matches!(
        port.post_origin(channel, "before".into(), None).await,
        PostOutcome::Created(_)
    ));
    execution::tests::admission(&pool, "claude", channel, "mac-a").await;
    assert!(matches!(
        port.post_origin(channel, "after".into(), None).await,
        PostOutcome::Withheld(_)
    ));
    assert_eq!(discord.posts.lock().unwrap().len(), 1);
    pool.close().await;
}
