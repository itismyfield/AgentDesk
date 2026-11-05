//! A consumed busy paste crosses a daemon restart; one idle health pass frees its original queue.
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::inflight::{self, InflightTurnState};
use crate::services::tui_o::writer::actor::{Owing, Undelivered};
use crate::services::tui_o::writer::host::OwedView;
use std::io::Write;

const B: &str = "iMessage reply after carried A";
const C: &str = "Discord input after carried A";
const END: &str = "{\"type\":\"system\",\"subtype\":\"turn_duration\"}";

thread_local! {
    static DAEMON_INTAKE_READY: std::cell::RefCell<Option<
        crate::services::tui_o::cutover::intake_route::test_probe::ProbeGuard
    >> = const { std::cell::RefCell::new(None) };
}

fn intake_ready(
    channel: ChannelId,
) -> crate::services::tui_o::cutover::intake_route::test_probe::ProbeGuard {
    let guard =
        crate::services::tui_o::cutover::intake_route::test_probe::answer_with(move |observed| {
            observed == channel.get()
        });
    assert_eq!(
        crate::services::tui_o::cutover::intake_route::route("claude", channel.get()),
        crate::services::tui_o::cutover::intake_route::IntakeRoute::Gateway,
        "the fixture's running O writer accepts intake on its gateway"
    );
    guard
}

struct DaemonRuntime(Option<tokio::runtime::Runtime>);

impl DaemonRuntime {
    async fn shutdown(mut self) {
        let runtime = self.0.take().unwrap();
        tokio::task::spawn_blocking(move || runtime.shutdown_timeout(Duration::ZERO))
            .await
            .unwrap();
    }
}

impl Drop for DaemonRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

fn assert_canonical(row: &Value) {
    let facts = crate::services::tui_o::shadow::identity::classify(
        crate::services::tui_o::shadow::ShadowProvider::Claude,
        row,
    );
    assert!(
        !facts.is_empty()
            && !facts.iter().any(|fact| matches!(
                fact,
                crate::services::tui_o::shadow::identity::RecordFact::Blocked(_)
            )),
        "fixture native record must have canonical identity: {row}; {facts:?}"
    );
}

fn assistant(id: &str, text: &str) -> Value {
    json!({"type":"assistant","uuid":format!("{id}-row"),"apiBlockIndex":0,
        "message":{"id":id,"role":"assistant","content":[{"type":"text","text":text}]}})
}

fn append(path: &Path, rows: &[Value]) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    for row in rows {
        assert_canonical(row);
        writeln!(file, "{row}").unwrap();
    }
}

async fn until(rig: &IntakeRig, label: &str, mut ready: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{label}: inflight={:?}, native_tail={:?}, renderer={:?}; {}",
            row(rig.channel).map(|row| (row.user_msg_id, row.restart_mode)),
            fs::read_to_string(&rig.transcript)
                .unwrap()
                .lines()
                .rev()
                .take(8)
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .map(|record| json!({"type":record["type"],"uuid":record["uuid"],
                    "subtype":record["subtype"],"message_id":record["message"]["id"]}))
                .collect::<Vec<_>>(),
            events(&rig.renderer),
            rig.trace.text()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_activity(
    rig: &IntakeRig,
    shared: &Arc<SharedData>,
    wanted: discord::turn_presence::activity::Activity,
    closed: bool,
    label: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let reading = discord::turn_presence::activity::reading_now(
            shared,
            &ProviderKind::Claude,
            rig.channel,
        )
        .await;
        if reading.observed.activity == wanted && (!closed || reading.closed_boundary().is_ok()) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{label}: wanted {wanted:?}, actual={:?}, boundary={:?}; {}",
            reading.observed,
            reading.closed_boundary(),
            rig.trace.text()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn row(channel: ChannelId) -> Option<InflightTurnState> {
    inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get())
}

fn native_prompts(rig: &IntakeRig, wanted: &str) -> usize {
    fs::read_to_string(&rig.transcript)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| {
            assert_canonical(row);
            row["type"] == "user"
                && row["message"]["content"]
                    .as_str()
                    .is_some_and(|text| text.contains(wanted))
        })
        .count()
}

/// Preflight sees idle; the first pane probe after intake creates its real row opens a native
/// turn, so execute_owned must take its busy observer after A was committed.
fn busy_after_row_create(rig: &IntakeRig) {
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let shim = rig._tmux.root.path().join("tmux");
    let script = fs::read_to_string(&shim).unwrap();
    let inflight = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        rig.channel.get(),
    );
    let tail = format!(
        "exec {} -L {} -f /dev/null \"$@\"",
        quote(&rig._tmux.real),
        rig._tmux.socket
    );
    let busy = [
        json!({"type":"user","uuid":"before-a-user","sessionId":rig.transcript.file_stem().unwrap().to_str().unwrap(),"message":{"role":"user","content":"native turn before A"}}),
        assistant("before-a-working", "working"),
    ];
    for record in &busy {
        assert_canonical(record);
    }
    let busy = busy
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let busy = format!("'{}'", busy.replace('\'', "'\\''"));
    let enter = format!(
        "if [ -f {row} ] && mkdir {marker} 2>/dev/null; then printf '%s\\n' {busy} >> {transcript}; fi\n\
         case \"$*\" in *send-keys*Enter*) n=$(awk '$0 == \"Enter\" {{ n++ }} END {{ print n + 1 }}' {events}); printf 'carried-native-%s' \"$n\" > {uuid} ;; esac\n",
        row = quote(&inflight),
        marker = quote(&rig.renderer.join("busy-after-row")),
        transcript = quote(&rig.transcript),
        events = quote(&rig.renderer.join("events")),
        uuid = quote(&rig.renderer.join("native-uuid")),
    );
    assert!(
        script.contains(&tail),
        "the fixture owns the exact tmux executable"
    );
    fs::write(shim, script.replace(&tail, &(enter + &tail))).unwrap();
}

/// Waits for the actual busy producer; it never manufactures evidence in the row.
async fn observe_busy_producer_pin(channel: ChannelId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if row(channel).is_some_and(|row| row.native_stop_user_pin.is_some()) {
            assert!(inflight::historical_input_verified(&row(channel).unwrap()));
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            eprintln!("BUSY_PRODUCER_PIN channel={channel} persisted=false");
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn host_notices(channel: ChannelId, path: &Path) -> tokio::task::JoinHandle<()> {
    host_debt(channel, path, 20)
}

fn host_debt(channel: ChannelId, path: &Path, notices: usize) -> tokio::task::JoinHandle<()> {
    let owing = Owing::default();
    crate::services::tui_o::writer::host::process_readiness().track_owed_for_test(
        channel.get(),
        OwedView {
            published: owing.published.subscribe(),
            demand: owing.demand.clone(),
        },
    );
    let path = path.to_path_buf();
    tokio::spawn(async move {
        loop {
            if owing.demand.wanted() {
                let len = fs::metadata(&path).unwrap().len();
                owing.published.send_replace(Some(Undelivered {
                    owed: notices,
                    notice_owed: notices,
                    asked: owing.demand.asked(),
                    captured: vec![(path.clone(), len)],
                    ..Undelivered::default()
                }));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
}

async fn state(shared: &Arc<SharedData>, channel: ChannelId) -> Value {
    shared
        .mailbox(channel)
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap()
}

async fn fresh_runtime(rig: &IntakeRig, pool: &sqlx::PgPool, generation: u64) -> Arc<SharedData> {
    let mut shared = discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let target = Arc::get_mut(&mut shared).unwrap();
    target.restart.current_generation = generation;
    target.api_port = rig.shared.api_port;
    *shared.settings.write().await = rig.shared.settings.read().await.clone();
    shared
        .http
        .cached_serenity_ctx
        .set(rig.shared.http.cached_serenity_ctx.get().unwrap().clone())
        .unwrap();
    shared
        .http
        .cached_bot_token
        .set("test-token".into())
        .unwrap();
    let label = format!("a1-rollback-{}", rig.channel);
    discord::host_defer_gate::tests::map_channel(&shared, rig.channel, &label).await;
    let previous = rig
        .shared
        .core
        .lock()
        .await
        .sessions
        .get(&rig.channel)
        .unwrap()
        .clone();
    shared
        .core
        .lock()
        .await
        .sessions
        .insert(rig.channel, previous);
    let persistence =
        discord::queue_persistence_context(&shared, &ProviderKind::Claude, rig.channel);
    shared
        .mailbox(rig.channel)
        .v2_ensure_channel(&persistence, rig.channel)
        .await
        .unwrap();
    shared
}

/// A recovered daemon marks its remaining row through the real shutdown barrier, then exits.
async fn restore_then_restart(rig: &IntakeRig, pool: &sqlx::PgPool) -> InflightTurnState {
    discord::runtime_store::set_process_generation_for_tests(Some(5));
    let shared = fresh_runtime(rig, pool, 5).await;
    let http = rig
        .shared
        .http
        .cached_serenity_ctx
        .get()
        .unwrap()
        .http
        .clone();
    let channel = rig.channel;
    let trace = rig.trace.clone();
    tokio::task::spawn_blocking(move || {
        let mut daemon = DaemonRuntime(Some(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap(),
        ));
        let restored = daemon.0.as_ref().unwrap().block_on(async move {
            discord::recovery_engine::restore_inflight_turns(&http, &shared, &ProviderKind::Claude)
                .await;
            assert!(
                shared
                    .mailbox(channel)
                    .snapshot()
                    .await
                    .cancel_token
                    .is_none()
            );
            let restored = row(channel).expect("the first boot retains A for its episode gate");
            assert!(!restored.readopted_from_inflight);
            assert_eq!(
                trace
                    .text()
                    .matches("restart marker consumed: readoption refused")
                    .count(),
                1,
                "the first boot consumes A's marker once: {}",
                trace.text()
            );
            assert!(
                discord::runtime_bootstrap::carried_restart_marked(&ProviderKind::Claude).await
            );
            let marked = row(channel).unwrap();
            assert_eq!(marked.user_msg_id, restored.user_msg_id);
            assert_eq!(marked.turn_nonce, restored.turn_nonce);
            assert_eq!(
                marked.restart_mode,
                Some(discord::InflightRestartMode::DrainRestart)
            );
            assert_eq!(marked.restart_generation, Some(5));
            restored
        });
        daemon.0.take().unwrap().shutdown_timeout(Duration::ZERO);
        restored
    })
    .await
    .unwrap()
}

async fn post_b(pool: &sqlx::PgPool, registry: Arc<HealthRegistry>, rig: &IntakeRig) -> Value {
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode},
        routing::post,
    };
    use tower::ServiceExt;
    let channel = rig.channel;
    crate::db::agents::insert_agent_channels_for_tests(
        pool,
        "cg-d1-0",
        Some(&channel.to_string()),
        None,
    )
    .await;
    let config = crate::config::Config::default();
    let broadcast_tx = crate::eventbus::new_broadcast();
    let app_state = crate::app_state::AppState {
        pg_pool: Some(pool.clone()),
        engine: crate::engine::PolicyEngine::new(&config).unwrap(),
        config: Arc::new(config),
        batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: Some(registry),
        cluster_instance_id: None,
    };
    let app = Router::new()
        .route(
            "/api/agents/{id}/turn/start",
            post(crate::server::routes::agents::start_agent_turn),
        )
        .with_state(app_state);
    let response = discord::queue_io::input_ingress::ADOPTS
        .scope(
            true,
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/agents/cg-d1-0/turn/start")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"prompt":B,"source":"imessage"}).to_string(),
                    ))
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
    let status = response.status();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "B POST after known-busy binding: {body}; {}",
        rig.trace.text()
    );
    assert_eq!(body["status"], "accepted", "{body}");
    body
}

/// The input enters through real owner intake. Only the old daemon runtime is stopped; the pane
/// and its disk queue remain, as with a planned restart.
async fn consumed_a(rig: &mut IntakeRig, restart: bool) -> Option<DaemonRuntime> {
    Arc::get_mut(&mut rig.shared)
        .unwrap()
        .restart
        .current_generation = 4;
    let sid = rig.transcript.file_stem().unwrap().to_str().unwrap();
    fs::write(rig.renderer.join("native-sid"), sid).unwrap();
    fs::write(rig.renderer.join("native-uuid"), "carried-native-1").unwrap();
    fs::write(rig.renderer.join("after"), view("", false)).unwrap();
    fs::create_dir_all(rig.transcript.parent().unwrap()).unwrap();
    fs::write(rig.renderer.join("transcript"), IDLE).unwrap();
    fs::hard_link(rig.renderer.join("transcript"), &rig.transcript).unwrap();
    busy_after_row_create(rig);
    let (drive, mut settled) = rig.drive();
    let trace = rig.trace.clone();
    let channel = rig.channel;
    let mailbox = rig.mailbox.clone();
    let daemon = tokio::task::spawn_blocking(move || {
        let mut daemon = DaemonRuntime(Some(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .on_thread_start(move || {
                    DAEMON_INTAKE_READY
                        .with(|slot| *slot.borrow_mut() = Some(intake_ready(channel)));
                })
                .on_thread_stop(|| {
                    DAEMON_INTAKE_READY.with(|slot| slot.borrow_mut().take());
                })
                .build()
                .unwrap(),
        ));
        daemon.0.as_ref().unwrap().block_on(async move {
            crate::services::discord::queue_io::input_owner::drive::wake(drive);
            let result = tokio::time::timeout(Duration::from_secs(30), settled.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result, Ok(()), "{}", trace.text());
            let owner = mailbox
                .v2_request(|slot| slot.as_ref().unwrap().json())
                .await
                .unwrap();
            let message = 1_479_671_302_387_076_000u64 + channel.get();
            assert!(
                owner["terminal_identities"].as_array().unwrap().iter().any(
                    |item| item["identity"]["primary"] == message || item["primary"] == message
                ),
                "A settled before the daemon continuation: {owner}"
            );
            let live = row(channel).unwrap_or_else(|| {
                panic!(
                    "real intake created A's row before its busy race: {}",
                    trace.text()
                )
            });
            assert!(live.terminal_receipt.is_none());
            observe_busy_producer_pin(channel).await;
            if restart {
                assert!(
                    discord::runtime_bootstrap::carried_restart_marked(&ProviderKind::Claude).await
                );
            }
        });
        if restart {
            daemon.0.take().unwrap().shutdown_timeout(Duration::ZERO);
            None
        } else {
            Some(daemon)
        }
    })
    .await
    .unwrap();
    let a = 1_479_671_302_387_076_000u64 + rig.channel.get();
    let owner = rig.state().await;
    assert!(
        owner["terminal_identities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["identity"]["primary"] == a || item["primary"] == a),
        "{owner}"
    );
    assert_eq!(
        native_prompts(rig, PROMPT),
        1,
        "A really reached the native transcript"
    );
    assert_eq!(events(&rig.renderer), ["paste", "Enter"]);
    daemon
}

#[tokio::test(flavor = "current_thread")]
async fn consumed_busy_input_restart_closes_on_the_first_idle_health_pass_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::carried_restart::",
        "consumed_busy_input_restart_closes_on_the_first_idle_health_pass_pg"
    )) {
        return;
    }
    restart_case(7807301, 20, false, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn consumed_busy_input_with_zero_debt_closes_on_first_idle_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::carried_restart::",
        "consumed_busy_input_with_zero_debt_closes_on_first_idle_pg"
    )) {
        return;
    }
    restart_case(7807303, 0, false, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn consumed_busy_input_followed_by_interrupted_native_turn_completes_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::carried_restart::",
        "consumed_busy_input_followed_by_interrupted_native_turn_completes_pg"
    )) {
        return;
    }
    restart_case(7807304, 20, true, false).await;
}

async fn restart_case(channel: u64, notices: usize, n_aborted: bool, own_cancelled: bool) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    discord::runtime_store::set_process_generation_for_tests(Some(4));
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut rig =
        IntakeRig::with_storage(channel, false, ProviderKind::Claude, Some(pool.clone())).await;
    let _owned = crate::services::tui_o::cutover::test_override::force_channels(&[(
        rig.channel.get(),
        RuntimeHandoffKind::ClaudeTui,
    )]);
    let channels =
        serde_json::to_string(&[(rig.channel.get(), RuntimeHandoffKind::ClaudeTui)]).unwrap();
    let _owned_process = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
        std::ffi::OsStr::new(&channels),
    );
    let _intake_ready = intake_ready(rig.channel);
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(rig.channel.get());
    assert!(consumed_a(&mut rig, true).await.is_none());
    let a = row(rig.channel).unwrap();
    assert_eq!(
        a.restart_mode,
        Some(discord::InflightRestartMode::DrainRestart)
    );
    assert!(!a.readopted_from_inflight);
    let first_restored = restore_then_restart(&rig, &pool).await;
    assert_eq!(first_restored.user_msg_id, a.user_msg_id);
    assert_eq!(native_prompts(&rig, PROMPT), 1);
    discord::runtime_store::set_process_generation_for_tests(Some(6));
    let shared = fresh_runtime(&rig, &pool, 6).await;
    let http = &rig.shared.http.cached_serenity_ctx.get().unwrap().http;
    discord::recovery_engine::restore_inflight_turns(http, &shared, &ProviderKind::Claude).await;
    assert!(
        shared
            .mailbox(rig.channel)
            .snapshot()
            .await
            .cancel_token
            .is_none(),
        "A's terminal source refuses readoption"
    );
    let restored = row(rig.channel).unwrap();
    assert!(!restored.readopted_from_inflight);
    assert_eq!(
        rig.trace
            .text()
            .matches("restart marker consumed: readoption refused")
            .count(),
        2,
        "each of the two actual boots consumes A's marker once: {}",
        rig.trace.text()
    );

    let binding_root = rig._tmux.root.path().join("health-binding");
    fs::create_dir_all(&binding_root).unwrap();
    let _binding = discord::health::BindingRoot::enter(&binding_root);
    let session =
        ProviderKind::Claude.build_tmux_session_name(&format!("a1-rollback-{}", rig.channel));
    let _ready = discord::health::ReadyPane::mark(&session);
    drop(_turn);
    let _confirmed = discord::health::bind_turn_mode_transcript(
        &shared,
        &ProviderKind::Claude,
        rig.channel.get(),
        &session,
        &rig.transcript,
    );
    wait_activity(
        &rig,
        &shared,
        discord::turn_presence::activity::Activity::Busy,
        false,
        "the source reader observes A before its historical end",
    )
    .await;
    append(
        &rig.transcript,
        &[
            serde_json::from_str(END).unwrap(),
            json!({"type":"user","uuid":"native-n","sessionId":a.session_id,"message":{"role":"user","content":"native N"}}),
            assistant("native-n-working", "N working"),
        ],
    );
    wait_activity(
        &rig,
        &shared,
        discord::turn_presence::activity::Activity::Busy,
        false,
        "N is busy before HTTP B",
    )
    .await;
    eprintln!(
        "CARRIED_RESTART_STAGE channel={} phase=n_busy_bound",
        rig.channel
    );
    let registry = Arc::new(HealthRegistry::new());
    registry.register("claude".into(), shared.clone()).await;
    let o = host_debt(rig.channel, &rig.transcript, notices);
    let foreign_before = rig
        .trace
        .text()
        .matches("another turn's durable inflight row holds the channel")
        .count();
    let reply = post_b(&pool, registry.clone(), &rig).await;
    let b = reply["turn_id"]
        .as_str()
        .unwrap()
        .rsplit(':')
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    until(
        &rig,
        "B is held by busy native input or the carried row",
        || {
            let trace = rig.trace.text();
            trace
                .matches("another turn's durable inflight row holds the channel")
                .count()
                > foreign_before
                || trace.contains("synthetic_waits_idle")
        },
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let snapshot = shared.mailbox(rig.channel).snapshot().await;
        if snapshot.cancel_token.is_none() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "B foreign-row refusal did not finish unwinding its claim: {}",
            rig.trace.text()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let c: PendingQueueItem = serde_json::from_value(json!({"author_id":7,"message_id":a.user_msg_id+100,"channel_id":rig.channel.get(),"queued_generation":6,"text":C})).unwrap();
    assert!(matches!(
        admit(&shared.mailbox(rig.channel), &c, AdmissionTicket::new()).await,
        Admission::Accepted(_)
    ));
    assert_eq!(native_prompts(&rig, B), 0);
    assert_eq!(native_prompts(&rig, C), 0);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let pending = state(&shared, rig.channel).await;
        let queued = |message| {
            pending["items"].as_array().unwrap().iter().any(|item| {
                item["message_id"] == message || item["original"]["message_id"] == message
            })
        };
        if queued(b) && queued(c.message_id) && pending["head_injection"].is_null() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "B/C retained after the foreign refusal settles: {pending}; {}",
            rig.trace.text()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(row(rig.channel).unwrap().user_msg_id, a.user_msg_id);
    eprintln!(
        "CARRIED_RESTART_STAGE channel={} phase=busy_or_foreign_held_b_and_c_preserved",
        rig.channel
    );
    let n_end = match n_aborted {
        true => {
            json!({"type":"user","uuid":"native-n-interrupted","sessionId":a.session_id,"message":{"role":"user","content":"[Request interrupted by user]"}})
        }
        false => serde_json::from_str(END).unwrap(),
    };
    append(&rig.transcript, &[n_end]);
    wait_activity(
        &rig,
        &shared,
        discord::turn_presence::activity::Activity::Idle,
        true,
        "N is idle with a proved native boundary before the health pass",
    )
    .await;
    let archives = inflight::inflight_runtime_root().unwrap().join("archive");
    eprintln!(
        "CARRIED_RESTART_STAGE channel={} phase=one_idle_health_pass",
        rig.channel
    );
    let expected_outcome = if own_cancelled {
        "cancelled"
    } else {
        "completed"
    };
    if own_cancelled {
        let live = row(rig.channel).unwrap();
        assert_eq!(
            inflight::stamp_terminal_receipt_if_matches_identity(
                &ProviderKind::Claude,
                rig.channel.get(),
                &inflight::InflightTurnIdentity::from_state(&live),
                live.turn_nonce.as_deref(),
                inflight::TerminalReceiptStamp {
                    outcome: crate::db::turns::TerminalOutcome::Cancelled,
                    confirmed_at: chrono::Utc::now(),
                },
            ),
            inflight::GuardedSaveOutcome::Saved
        );
    }
    let committed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed_commit = committed.clone();
    let options = pool.connect_options().as_ref().clone();
    let turn = format!("discord:{}:{}", rig.channel, a.user_msg_id);
    discord::health::before_carried_archive_for_tests(rig.channel.get(), move || {
        let outcome = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let pool = sqlx::postgres::PgPoolOptions::new()
                        .max_connections(1)
                        .connect_with(options)
                        .await
                        .unwrap();
                    let result: Option<String> =
                        sqlx::query_scalar("SELECT terminal_outcome FROM turns WHERE turn_id=$1")
                            .bind(turn)
                            .fetch_optional(&pool)
                            .await
                            .unwrap();
                    pool.close().await;
                    result
                })
        })
        .join()
        .unwrap();
        assert_eq!(
            outcome.as_deref(),
            Some(expected_outcome),
            "A committed its own outcome before archive even if later N aborted"
        );
        observed_commit.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    discord::health::run_stall_watchdog_pass(&registry, &ProviderKind::Claude).await;
    if row(rig.channel).is_some_and(|now| now.user_msg_id == a.user_msg_id) {
        assert!(
            rig.trace.text().contains("o_owes:owed=20")
                || rig.trace.text().contains("unverified_carried_input"),
            "the baseline must reach the unrelated-notice gate, not fail a fixture precondition: {}",
            rig.trace.text()
        );
    }
    assert!(
        row(rig.channel).is_none_or(|now| now.user_msg_id != a.user_msg_id),
        "the first idle pass must close A with {notices} notices: {}",
        rig.trace.text()
    );
    assert!(committed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        a.native_stop_user_pin.is_some(),
        "the busy producer stored A's pin before shutdown"
    );
    until(&rig, "B starts after the one idle pass", || {
        native_prompts(&rig, B) == 1
    })
    .await;
    append(
        &rig.transcript,
        &[
            assistant("b-answer", "B answered once"),
            serde_json::from_str(END).unwrap(),
        ],
    );
    until(&rig, "C starts once", || native_prompts(&rig, C) == 1).await;
    append(
        &rig.transcript,
        &[
            assistant("c-answer", "C answered once"),
            serde_json::from_str(END).unwrap(),
        ],
    );
    until(
        &rig,
        "B completion frees its row after C's native turn ends",
        || row(rig.channel).is_none_or(|now| now.user_msg_id != b),
    )
    .await;
    assert_eq!(native_prompts(&rig, PROMPT), 1, "A was never replayed");
    assert_eq!(native_prompts(&rig, B), 1);
    assert_eq!(native_prompts(&rig, C), 1);
    let uuid_for = |prompt: &str| {
        fs::read_to_string(&rig.transcript)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|record| {
                record["type"] == "user"
                    && record["message"]["content"]
                        .as_str()
                        .is_some_and(|body| body.contains(prompt))
            })
            .unwrap()["uuid"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        [uuid_for(PROMPT), uuid_for(B), uuid_for(C)],
        ["carried-native-1", "carried-native-2", "carried-native-3"]
    );
    let archived = fs::read_dir(archives)
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{}.json.turn-mode-carried-turn", rig.channel))
        })
        .count();
    assert_eq!(archived, 1, "A archived exactly once");
    let receipt: String = sqlx::query_scalar("SELECT terminal_outcome FROM turns WHERE turn_id=$1")
        .bind(format!("discord:{}:{}", rig.channel, a.user_msg_id))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(receipt, expected_outcome);
    assert!(
        first_restored.restart_mode.is_none(),
        "the first boot consumes A's original marker before the next shutdown restamps it"
    );
    assert!(
        restored.restart_mode.is_none(),
        "the second boot consumes the marker stamped by the intervening actual shutdown"
    );
    assert_eq!(
        rig.mock
            .written_texts()
            .iter()
            .filter(|text| text.contains("B answered once") || text.contains("C answered once"))
            .count(),
        0,
        "O owns B/C bodies, so the bridges cannot post duplicate legacy answers"
    );
    o.abort();
    drop(rig);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn consumed_busy_input_without_restart_finishes_through_its_original_bridge_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::carried_restart::",
        "consumed_busy_input_without_restart_finishes_through_its_original_bridge_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    discord::runtime_store::set_process_generation_for_tests(Some(4));
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut rig =
        IntakeRig::with_storage(7_807_302, false, ProviderKind::Claude, Some(pool.clone())).await;
    let _owned = crate::services::tui_o::cutover::test_override::force_channels(&[(
        rig.channel.get(),
        RuntimeHandoffKind::ClaudeTui,
    )]);
    let channels =
        serde_json::to_string(&[(rig.channel.get(), RuntimeHandoffKind::ClaudeTui)]).unwrap();
    let _owned_process = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
        std::ffi::OsStr::new(&channels),
    );
    let _intake_ready = intake_ready(rig.channel);
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(rig.channel.get());
    let daemon = consumed_a(&mut rig, false)
        .await
        .expect("the daemon remains alive without restart");
    let a = row(rig.channel).unwrap();
    let o = host_notices(rig.channel, &rig.transcript);
    append(
        &rig.transcript,
        &[
            assistant("a-answer", "A answered without restart"),
            serde_json::from_str(END).unwrap(),
        ],
    );
    until(&rig, "the original bridge closes A", || {
        row(rig.channel).is_none()
    })
    .await;
    assert_eq!(native_prompts(&rig, PROMPT), 1);
    assert_eq!(events(&rig.renderer), ["paste", "Enter"]);
    let answers = rig
        .mock
        .written_texts()
        .into_iter()
        .filter(|text| text.contains("A answered without restart"))
        .count();
    assert_eq!(
        answers, 0,
        "O owns the body, so the original bridge cannot post another answer"
    );
    let receipt: String = sqlx::query_scalar("SELECT terminal_outcome FROM turns WHERE turn_id=$1")
        .bind(format!("discord:{}:{}", rig.channel, a.user_msg_id))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(receipt, "completed");
    assert!(!rig.trace.text().contains("carried-over turn closed"));
    o.abort();
    daemon.shutdown().await;
    drop(rig);
    pool.close().await;
    db.drop().await;
}

#[derive(Clone, Copy, Debug)]
enum RestoreOwnership {
    NonO,
    Unknown,
    OWithoutTurnConfirmation,
}

async fn restore_ownership_case(channel: u64, ownership: RestoreOwnership) {
    use crate::services::tui_o::channel_policy::Adoption;
    use crate::services::tui_o::cutover::{boot_ownership, test_override};

    let _root = crate::config::TestRuntimeRootGuard::new();
    discord::runtime_store::set_process_generation_for_tests(Some(4));
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut rig =
        IntakeRig::with_storage(channel, false, ProviderKind::Claude, Some(pool.clone())).await;
    let channels = [(rig.channel.get(), RuntimeHandoffKind::ClaudeTui)];
    let initial_owned = test_override::force_channels(&channels);
    let owned_json = serde_json::to_string(&channels).unwrap();
    let initial_process = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        test_override::CHANNELS_ENV,
        std::ffi::OsStr::new(&owned_json),
    );
    let _intake_ready = intake_ready(rig.channel);
    let initial_turn = crate::services::tui_o::turn_mode::TestConfirmation::new(channel);
    assert!(consumed_a(&mut rig, true).await.is_none());
    let marked = row(rig.channel).expect("the actual old daemon left its marked A row");
    assert_eq!(
        marked.restart_mode,
        Some(discord::InflightRestartMode::DrainRestart)
    );
    assert_eq!(marked.restart_generation, Some(4));
    assert!(!marked.readopted_from_inflight);
    drop(initial_turn);
    drop(initial_owned);
    drop(initial_process);

    // The new process has its own boot ownership, while A and its native pane stay intact.
    let _boot = match ownership {
        RestoreOwnership::NonO => test_override::force_channels(&[]),
        RestoreOwnership::Unknown => test_override::force_candidates(&channels),
        RestoreOwnership::OWithoutTurnConfirmation => test_override::force_channels(&channels),
    };
    let process_channels = match ownership {
        RestoreOwnership::OWithoutTurnConfirmation => owned_json.as_str(),
        RestoreOwnership::NonO | RestoreOwnership::Unknown => "[]",
    };
    let _boot_process = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        test_override::CHANNELS_ENV,
        std::ffi::OsStr::new(process_channels),
    );
    assert!(!crate::services::tui_o::turn_mode::transcript_turns(
        channel
    ));
    let candidate = boot_ownership()
        .into_iter()
        .find(|(observed, _, _)| *observed == channel)
        .and_then(|(_, _, candidate)| candidate);
    match ownership {
        RestoreOwnership::NonO => assert!(candidate.is_none()),
        RestoreOwnership::Unknown => {
            let candidate = candidate
                .as_ref()
                .expect("unknown boot has a pending candidate");
            assert_eq!(candidate.peek(), Adoption::Pending);
            assert!(
                !candidate.peek().owned(),
                "unknown is not evidence of O ownership"
            );
        }
        RestoreOwnership::OWithoutTurnConfirmation => {
            assert!(candidate.as_ref().unwrap().peek().owned());
        }
    }
    discord::runtime_store::set_process_generation_for_tests(Some(5));
    let shared = fresh_runtime(&rig, &pool, 5).await;
    let http = &rig.shared.http.cached_serenity_ctx.get().unwrap().http;
    discord::recovery_engine::restore_inflight_turns(http, &shared, &ProviderKind::Claude).await;
    assert!(
        shared
            .mailbox(rig.channel)
            .snapshot()
            .await
            .cancel_token
            .is_none(),
        "A's terminal owner identity refuses readoption: {ownership:?}; {}",
        rig.trace.text()
    );
    let restored = row(rig.channel).expect("pane-alive restore retains the same A row");
    assert_eq!(restored.user_msg_id, marked.user_msg_id);
    assert_eq!(restored.turn_nonce, marked.turn_nonce);
    assert!(!restored.readopted_from_inflight);
    assert!(restored.native_stop_user_pin.is_some());
    assert!(!crate::services::tui_o::turn_mode::transcript_turns(
        channel
    ));
    let consumed = rig
        .trace
        .text()
        .matches("restart marker consumed: readoption refused")
        .count();
    match ownership {
        RestoreOwnership::OWithoutTurnConfirmation => {
            assert!(candidate.as_ref().unwrap().peek().owned());
            assert!(restored.restart_mode.is_none());
            assert!(restored.restart_generation.is_none());
            assert_eq!(consumed, 1, "O ownership alone permits marker consumption");
        }
        RestoreOwnership::NonO | RestoreOwnership::Unknown => {
            assert!(
                candidate
                    .as_ref()
                    .is_none_or(|candidate| !candidate.peek().owned())
            );
            assert_eq!(restored.restart_mode, marked.restart_mode);
            assert_eq!(restored.restart_generation, marked.restart_generation);
            assert_eq!(
                consumed, 0,
                "an unowned boot cannot consume watcher authority"
            );
            assert_eq!(
                restored.effective_relay_owner_kind(),
                inflight::RelayOwnerKind::None
            );
            assert!(!restored.terminal_delivery_committed);
            let session = restored.tmux_session_name.as_deref().unwrap();
            let start = restored.turn_start_offset.unwrap_or(restored.last_offset);
            let end = fs::metadata(&rig.transcript).unwrap().len();
            assert!(
                start < end,
                "the actual watcher range intersects A's saved start"
            );
            assert!(!discord::tmux::restored_watcher_should_yield(
                &restored, session, 0, end,
            ));
            let mut without_marker = restored.clone();
            without_marker.restart_mode = None;
            without_marker.restart_generation = None;
            assert!(
                discord::tmux::restored_watcher_should_yield(&without_marker, session, 0, end),
                "the preserved marker is the existing planned-restart hatch's decisive input"
            );
        }
    }
    assert_eq!(native_prompts(&rig, PROMPT), 1, "A is never replayed");
    assert_eq!(events(&rig.renderer), ["paste", "Enter"]);
    drop(rig);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn consumed_busy_input_non_o_boot_preserves_the_planned_watcher_hatch_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::carried_restart::",
        "consumed_busy_input_non_o_boot_preserves_the_planned_watcher_hatch_pg"
    )) {
        return;
    }
    restore_ownership_case(7_807_303, RestoreOwnership::NonO).await;
}

#[tokio::test(flavor = "current_thread")]
async fn consumed_busy_input_unknown_boot_preserves_the_planned_watcher_hatch_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::carried_restart::",
        "consumed_busy_input_unknown_boot_preserves_the_planned_watcher_hatch_pg"
    )) {
        return;
    }
    restore_ownership_case(7_807_304, RestoreOwnership::Unknown).await;
}

#[tokio::test(flavor = "current_thread")]
async fn consumed_busy_input_o_boot_consumes_marker_without_turn_confirmation_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::carried_restart::",
        "consumed_busy_input_o_boot_consumes_marker_without_turn_confirmation_pg"
    )) {
        return;
    }
    restore_ownership_case(7_807_305, RestoreOwnership::OWithoutTurnConfirmation).await;
}

#[tokio::test(flavor = "current_thread")]
async fn consumed_busy_input_own_cancel_stamp_survives_orphan_close_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::carried_restart::",
        "consumed_busy_input_own_cancel_stamp_survives_orphan_close_pg"
    )) {
        return;
    }
    restart_case(7807308, 20, false, true).await;
}
