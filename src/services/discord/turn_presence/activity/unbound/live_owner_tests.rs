//! A session-less turn-mode channel on a host that also runs unrelated tmux sessions.
use super::review_tests::Rig;
use super::*;
use crate::services::discord::health::{
    HealthRegistry, HumanInputDelivery, HumanInputRequest, deliver_human_input,
    external_turn_hold_for_start, start_without_gateway,
};
use crate::services::platform::tmux::session_inventory::{EmptyReason, Inventory};
use crate::services::tui_o::turn_mode::TestConfirmation;
use crate::services::turn_orchestrator::{Intervention, InterventionMode, QueuePersistenceContext};
use poise::serenity_prelude::{MessageId, UserId};
use std::path::PathBuf;

const MANUAL: &str = "claude";
const ROUTINE: &str = "AgentDesk-claude-routine-brief---counsel";
const STRAY: &str = "AgentDesk-claude-stray";
const THREAD: u64 = 69_183_950;

/// Real tmux sessions on the rig's private socket, which is never the host's own server.
struct HostSessions {
    socket: PathBuf,
    _namespace: crate::services::platform::tmux::session_inventory::tests::ChannelNamespace,
}
impl HostSessions {
    async fn new(rig: &Rig) -> Self {
        let namespace =
            crate::services::platform::tmux::session_inventory::tests::ChannelNamespace::new(
                rig.channel.get(),
                rig.root.path().join("host.sock"),
            );
        let watch = Arc::new(Mutex::new(Watch::default()));
        let (_, stamp) = read(&rig.shared, &ProviderKind::Claude, rig.channel, watch).await;
        let socket = match &stamp.namespace.as_ref().unwrap().inventory {
            Inventory::Empty(EmptyReason::MissingSocket(path)) => path.clone(),
            other => panic!("fresh private namespace expected: {other:?}"),
        };
        assert!(socket.starts_with(rig.root.path()), "{socket:?}");
        let dir = socket.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            socket,
            _namespace: namespace,
        }
    }
    fn tmux(&self, args: &[&str]) {
        let output = crate::services::platform::binary_resolver::runtime_command("tmux")
            .unwrap()
            .arg("-S")
            .arg(&self.socket)
            .args(["-u", "-f", "/dev/null"])
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
    }
    fn start(&self, name: &str) {
        self.tmux(&["new-session", "-d", "-s", name, "sleep 600"]);
    }
}
impl Drop for HostSessions {
    fn drop(&mut self) {
        let _ = crate::services::platform::binary_resolver::runtime_command("tmux")
            .unwrap()
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}

/// A finished fresh routine run that recorded `name` and ran its turn in `channel`.
async fn routine_ran(rig: &Rig, name: &str, channel: u64) {
    let pool = rig.shared.pg_pool.as_ref().unwrap();
    let key = adk_session::build_namespaced_session_key(
        &rig.shared.token_hash,
        &ProviderKind::Claude,
        name,
    );
    sqlx::query("INSERT INTO routines (id, script_ref, name) VALUES ('brief', 'b.js', 'brief')")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO routine_runs (id, routine_id, status, turn_id, result_json,
                                   owned_tmux_session, finished_at)
         VALUES ('run', 'brief', 'succeeded', 'turn', $1, $2, NOW())",
    )
    .bind(serde_json::json!({"channel_id": channel.to_string()}))
    .bind(key)
    .execute(pool)
    .await
    .unwrap();
}

async fn unbound(rig: &Rig) -> (Observed, Stamp, Arc<Mutex<Watch>>) {
    assert!(
        rig.shared
            .tmux_watchers
            .channel_binding(&rig.channel)
            .is_none()
    );
    let watch = Arc::new(Mutex::new(Watch::default()));
    let (answer, stamp) = read(
        &rig.shared,
        &ProviderKind::Claude,
        rig.channel,
        watch.clone(),
    )
    .await;
    let live = match &stamp.namespace.as_ref().unwrap().inventory {
        Inventory::Sessions(names) => names.clone(),
        other => panic!("live host inventory expected: {other:?}"),
    };
    assert!(live.iter().any(|name| name == MANUAL), "{live:?}");
    (answer, stamp, watch)
}

/// A manual session and a routine session that ran elsewhere leave the channel idle; an unowned
/// AgentDesk session, a failed routine read or a routine moved here keeps it unknown.
#[tokio::test(flavor = "current_thread")]
async fn unrelated_host_sessions_leave_a_session_less_channel_idle_pg() {
    let rig = Rig::new(69_183_101, false, false).await;
    let host = HostSessions::new(&rig).await;
    host.start(MANUAL);
    host.start(ROUTINE);
    let unknown = observed(Activity::Unknown, "session_identity_unresolved");
    assert_eq!(unbound(&rig).await.0, unknown, "routine without evidence");
    routine_ran(&rig, ROUTINE, THREAD).await;
    let (answer, stamp, watch) = unbound(&rig).await;
    assert_eq!(answer, observed(Activity::Idle, "no_session"));
    assert!(stamp.publish(&watch, || ()).await.is_some());

    let pool = rig.shared.pg_pool.as_ref().unwrap();
    let moved = format!(r#"{{"channel_id": "{}"}}"#, rig.channel);
    let moved_here = sqlx::query("UPDATE routine_runs SET result_json = $1::jsonb").bind(&moved);
    moved_here.execute(pool).await.unwrap();
    assert!(
        stamp.publish(&watch, || ()).await.is_none(),
        "an ownership change after the read published the old absence"
    );
    assert_eq!(
        unbound(&rig).await.0,
        observed(Activity::Unknown, "watcher_unbound")
    );

    let back = format!(r#"{{"channel_id": "{THREAD}"}}"#);
    let back = sqlx::query("UPDATE routine_runs SET result_json = $1::jsonb").bind(&back);
    back.execute(pool).await.unwrap();
    let hidden = "ALTER TABLE routine_runs RENAME TO routine_runs_hidden";
    sqlx::query(hidden).execute(pool).await.unwrap();
    assert_eq!(unbound(&rig).await.0, unknown, "a failed routine read");
    let shown = "ALTER TABLE routine_runs_hidden RENAME TO routine_runs";
    sqlx::query(shown).execute(pool).await.unwrap();

    host.start(STRAY);
    assert_eq!(
        unbound(&rig).await.0,
        unknown,
        "an unowned AgentDesk session"
    );
}

fn queued(id: u64, text: &str) -> Intervention {
    Intervention {
        author_id: UserId::new(200),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: text.into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

/// Input queued while the channel read unknown stays queued, then drains in order, once each, by
/// the automatic dequeue once only unrelated host sessions remain.
#[tokio::test(flavor = "current_thread")]
async fn queued_input_drains_in_order_once_unrelated_sessions_remain_pg() {
    let rig = Rig::new(69_183_102, false, false).await;
    let _turn_mode = TestConfirmation::new(rig.channel.get());
    let host = HostSessions::new(&rig).await;
    host.start(MANUAL);
    host.start(ROUTINE);
    let unknown = observed(Activity::Unknown, "session_identity_unresolved");
    assert_eq!(unbound(&rig).await.0, unknown);
    let mailbox = rig.shared.mailbox(rig.channel);
    for (id, text) in [(701, "A"), (702, "B")] {
        let token = &rig.shared.token_hash;
        let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, token, None);
        let enqueued = mailbox.enqueue(queued(id, text), persistence).await;
        assert!(enqueued.enqueued);
    }
    let take = crate::services::discord::mailbox_take_next_automatic_intervention;
    let held = take(&rig.shared, &ProviderKind::Claude, rig.channel).await;
    assert!(held.intervention.is_none(), "an unknown channel dequeued");
    let queue = mailbox.snapshot().await.intervention_queue;
    let texts: Vec<_> = queue.iter().map(|item| item.text.as_str()).collect();
    assert_eq!(texts, ["A", "B"]);

    routine_ran(&rig, ROUTINE, THREAD).await;
    assert_eq!(
        unbound(&rig).await.0,
        observed(Activity::Idle, "no_session")
    );
    let mut taken = Vec::new();
    for _ in 0..3 {
        let outcome = take(&rig.shared, &ProviderKind::Claude, rig.channel).await;
        let Some(intervention) = outcome.intervention else {
            break;
        };
        taken.push(intervention.text.clone());
        crate::services::discord::mailbox_abandon_unclaimed_dispatch_after_success(
            &rig.shared,
            &ProviderKind::Claude,
            rig.channel,
            intervention.message_id,
            outcome.dispatch_lease.expect("automatic dequeue lease"),
        )
        .await;
    }
    assert_eq!(taken, ["A", "B"]);
    assert!(mailbox.snapshot().await.intervention_queue.is_empty());
}

/// The start gate and the human-input deliver both pass such a channel, and the deliver starts
/// exactly one turn.
#[tokio::test(flavor = "current_thread")]
async fn start_gate_and_deliver_start_once_beside_unrelated_sessions_pg() {
    let rig = Rig::new(69_183_103, false, false).await;
    let _turn_mode = TestConfirmation::new(rig.channel.get());
    let host = HostSessions::new(&rig).await;
    host.start(MANUAL);
    host.start(ROUTINE);
    routine_ran(&rig, ROUTINE, THREAD).await;
    let channel = rig.channel.get();
    {
        let mut settings = rig.shared.settings.write().await;
        settings.owner_user_id = Some(100);
        settings.allowed_user_ids = vec![200];
        settings.allowed_channel_ids = vec![channel];
    }
    let registry = HealthRegistry::new();
    registry.register("claude".into(), rig.shared.clone()).await;
    let begun = start_without_gateway(channel);
    let provider = ProviderKind::Claude;
    let hold = external_turn_hold_for_start(Some(&registry), &provider, channel).await;
    assert_eq!(hold, None);
    let request = || HumanInputRequest {
        channel_id: rig.channel,
        provider: ProviderKind::Claude,
        text: "status?".into(),
        author_id: 200,
        source: "imessage".into(),
        metadata: None,
        channel_name_hint: None,
    };
    let first = deliver_human_input(&registry, request()).await.unwrap();
    let HumanInputDelivery::Started { turn_id } = &first else {
        panic!("{first:?}");
    };
    assert!(
        turn_id.starts_with(&format!("discord:{channel}:")),
        "{turn_id}"
    );
    let started = tokio::time::timeout(std::time::Duration::from_secs(1), begun.notified());
    assert!(started.await.is_ok(), "the deliver never began its start");
    let second = deliver_human_input(&registry, request()).await.unwrap();
    assert!(
        matches!(&second, HumanInputDelivery::Queued { reason, .. } if reason == "turn_active"),
        "a second start ran beside the first: {second:?}"
    );
}

async fn post_start(
    app: &axum::Router,
    agent: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
    use tower::ServiceExt;
    let request = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri(format!("/agents/{agent}/turn/start"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"prompt": "status?"}"#))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// The turn-start route answers such a channel with a started turn whose provider runs once.
#[tokio::test(flavor = "current_thread")]
async fn turn_start_route_runs_one_provider_turn_beside_unrelated_sessions_pg() {
    use crate::services::discord::router::message_handler::{
        CLAUDE_LEGACY_PROBE, INPUT_EFFECT_PROBE, InputEffectProbe,
    };
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    use crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let rig = Rig::new(69_183_104, false, false).await;
    let _turn_mode = TestConfirmation::new(rig.channel.get());
    let host = HostSessions::new(&rig).await;
    host.start(MANUAL);
    host.start(ROUTINE);
    routine_ran(&rig, ROUTINE, THREAD).await;
    let channel = rig.channel.get();
    let pool = rig.shared.pg_pool.clone().unwrap();
    let seed = crate::db::agents::insert_agent_channels_for_tests;
    seed(&pool, "e1-route", Some(&channel.to_string()), None).await;
    let workspace = rig.root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut core = rig.shared.core.lock().await;
    core.sessions.get_mut(&rig.channel).unwrap().current_path =
        Some(workspace.display().to_string());
    drop(core);
    let mock = discord_mock::DiscordMockState::new();
    mock.allow_channel(channel);
    let (proxy, gateway, server) = discord_mock::start(mock).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    assert!(rig.shared.http.cached_serenity_ctx.set(ctx).is_ok());
    assert!(
        rig.shared
            .http
            .cached_bot_token
            .set("test-token".into())
            .is_ok()
    );
    let registry = Arc::new(HealthRegistry::new());
    registry.register("claude".into(), rig.shared.clone()).await;
    let (entered, ran) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let probe = InputEffectProbe {
        channel,
        entered,
        release: wait,
        terminal_before_release: false,
    };
    assert!(INPUT_EFFECT_PROBE.lock().unwrap().replace(probe).is_none());
    let extra_starts = Arc::new(AtomicUsize::new(0));
    let extra = extra_starts.clone();
    assert!(
        CLAUDE_LEGACY_PROBE
            .lock()
            .unwrap()
            .replace((
                channel,
                Box::new(move || {
                    extra.fetch_add(1, Ordering::SeqCst);
                    Err("duplicate provider start".into())
                })
            ))
            .is_none()
    );
    let (completed, completion) = tokio::sync::oneshot::channel();
    assert!(
        BRIDGE_COMPLETION_PROBE
            .lock()
            .unwrap()
            .replace((rig.channel, completed))
            .is_none()
    );
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
        Some(pool),
    );

    let (status, body) = post_start(&app, "e1-route").await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(
        (&body["ok"], &body["status"]),
        (&serde_json::json!(true), &serde_json::json!("started"))
    );
    let turn_id = body["turn_id"].as_str().unwrap_or_default();
    assert!(
        turn_id
            .strip_prefix(&format!("discord:{channel}:"))
            .and_then(|id| id.parse::<u64>().ok())
            .is_some_and(|id| id != 0),
        "{body}"
    );
    let ran = tokio::time::timeout(std::time::Duration::from_secs(10), ran).await;
    assert!(
        matches!(ran, Ok(Ok(_))),
        "the started turn never reached its provider"
    );
    assert_ne!(ran.unwrap().unwrap(), std::thread::current().id());
    let inflight = inflight::load_inflight_state(&ProviderKind::Claude, channel).unwrap();
    assert_eq!(
        turn_id,
        format!("discord:{channel}:{}", inflight.user_msg_id)
    );
    let (again, refused) = post_start(&app, "e1-route").await;
    assert_eq!(again, axum::http::StatusCode::CONFLICT, "{refused}");
    assert!(INPUT_EFFECT_PROBE.lock().unwrap().is_none());
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), completion)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(extra_starts.load(Ordering::SeqCst), 0, "duplicate starter");
    CLAUDE_LEGACY_PROBE.lock().unwrap().take();
    eprintln!("E1_ROUTE_START channel={channel} starter=1 extra=0 skip=0");
    server.abort();
}
