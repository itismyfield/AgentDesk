//! External HTTP admission over the existing bound Herdr execution and real O writer.
use super::*;
use crate::services::discord::health::HealthRegistry;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;

#[path = "provider_dispatch_herdr_internal_start_tests.rs"]
mod internal_start;
#[path = "provider_dispatch_herdr_native_work_tests.rs"]
mod native_work;
#[path = "provider_dispatch_herdr_http_start_tests/thread_ingress_tests.rs"]
mod thread_ingress;

async fn app(fx: &Fixture, intake: &Intake) -> Router {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agents WHERE id='busy-entry')")
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    if !exists {
        crate::db::agents::insert_agent_channels_for_tests(
            &fx.pool,
            "busy-entry",
            Some(&CHANNEL.to_string()),
            None,
        )
        .await;
    }
    if !intake.listener_started.swap(true, Ordering::SeqCst) {
        use discord::queue_io::cancel_backstop_test_support as observed;
        let before = observed::listener_completed_reconciles(ChannelId::new(CHANNEL));
        discord::queue_io::spawn_turn_completion_idle_queue_listener(
            intake.shared.clone(),
            ProviderKind::Claude,
        );
        tokio::time::timeout(LIMIT, async {
            while observed::listener_completed_reconciles(ChannelId::new(CHANNEL)) <= before {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    intake.shared.settings.write().await.owner_user_id = Some(7);
    let registry = Arc::new(HealthRegistry::new());
    registry
        .register("claude".into(), intake.shared.clone())
        .await;
    let generation = discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "1").unwrap();
    let config = crate::config::Config::default();
    let broadcast = crate::eventbus::new_broadcast();
    crate::server::routes::api_router_with_pg(
        crate::engine::PolicyEngine::new(&config).unwrap(),
        config,
        broadcast.clone(),
        crate::eventbus::spawn_batch_flusher(broadcast),
        Some(registry),
        Some(fx.pool.clone()),
    )
}

async fn call(app: &Router, endpoint: &str, origin: &str) -> (StatusCode, Value) {
    call_source(app, endpoint, origin, "imessage").await
}

async fn call_source(
    app: &Router,
    endpoint: &str,
    origin: &str,
    source: &str,
) -> (StatusCode, Value) {
    call_target(app, endpoint, origin, source, None).await
}

async fn call_target(
    app: &Router,
    endpoint: &str,
    origin: &str,
    source: &str,
    channel: Option<u64>,
) -> (StatusCode, Value) {
    let mut body = match endpoint {
        "start" => json!({"prompt":INPUT,"source":source,"origin_id":origin}),
        "deliver" => json!({"text":INPUT,"source":source,"origin_id":origin,
            "author_discord_user_id":"7"}),
        _ => panic!("unknown endpoint"),
    };
    if let Some(channel) = channel {
        body["channel_id"] = json!(channel.to_string());
    }
    let request = Request::builder()
        .method("POST")
        .uri(format!("/agents/busy-entry/turn/{endpoint}"))
        .header("content-type", "application/json")
        .extension(crate::server::routes::auth::loopback_peer())
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = discord::OWNER_ADOPTS_FOR_TEST
        .scope(true, app.clone().oneshot(request))
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn finish_answer(path: &Path, session: &str, n: usize) {
    answer_with_body(path, session, &format!("HTTP session answered {n}"));
}

fn answer_with_body(path: &Path, session: &str, body: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    append(
        path,
        &[
            json!({"type":"assistant","uuid":uuid(),"sessionId":session,"timestamp":now,
            "apiBlockIndex":0,"message":{"id":uuid(),"role":"assistant",
            "content":[{"type":"text","text":body}],"stop_reason":"end_turn"}}),
            json!({"type":"system","subtype":"turn_duration","sessionId":session,"timestamp":now}),
        ],
    );
}

fn answer_input(fx: &Fixture, intake: &Intake, started: &Started) -> Arc<Mutex<Vec<Value>>> {
    answer_on_enter(fx, intake, started, false)
}

fn answer_on_enter(
    fx: &Fixture,
    intake: &Intake,
    started: &Started,
    hold_first: bool,
) -> Arc<Mutex<Vec<Value>>> {
    let (session, path) = (started.session.clone(), started.path.clone());
    let heads = Arc::new(Mutex::new(Vec::new()));
    let (seen, mailbox, runtime) = (
        heads.clone(),
        intake.mailbox.clone(),
        intake.runtime.handle().clone(),
    );
    let body = Mutex::new(String::new());
    fx.rig.on_send(move |params| {
        let snapshot = runtime
            .block_on(mailbox.v2_request(|slot| slot.as_ref().unwrap().json()))
            .unwrap();
        seen.lock()
            .unwrap()
            .push(snapshot["head_injection"].clone());
        if let Some(text) = params["text"].as_str() {
            *body.lock().unwrap() = text
                .trim_start_matches("\u{1b}[200~")
                .trim_end_matches("\u{1b}[201~")
                .into();
        }
        if params["keys"] == json!(["enter"]) {
            let n = seen.lock().unwrap().len() / 2;
            let now = chrono::Utc::now().to_rfc3339();
            append(
                &path,
                &[
                    json!({"type":"user","uuid":uuid(),"sessionId":session,"timestamp":now,
                "message":{"role":"user","content":body.lock().unwrap().clone()}}),
                ],
            );
            if !hold_first || n != 1 {
                finish_answer(&path, &session, n);
            }
        }
    });
    heads
}

async fn completed(intake: &Intake, answers: usize) {
    let outcome = tokio::time::timeout(LIMIT, async {
        loop {
            let posts = intake.posts.to(CHANNEL);
            if intake.mailbox.has_active_turn().await == Ok(false)
                && inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL).is_none()
                && posts
                    .iter()
                    .filter(|text| text.contains("HTTP session answered"))
                    .count()
                    == answers
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    if let Err(error) = outcome {
        let state = intake
            .mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        let activity = discord::turn_presence::activity::activity_now(
            &intake.shared,
            &ProviderKind::Claude,
            ChannelId::new(CHANNEL),
        )
        .await;
        let history =
            crate::services::tui_input::human_frame::history::v2::Registry::runtime().unwrap();
        let native: Vec<_> = history
            .native_classification_registrations()
            .unwrap()
            .into_iter()
            .filter(|r| r.ticket.channel_id == CHANNEL)
            .map(|r| history.native_settlement(&r.namespace, &r.ticket))
            .collect();
        panic!(
            "{error}: row={:?}, O={:?}, bridge={:?}, state={state}, activity={activity:?}, native={native:?}, drives={:?}, {}",
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL),
            intake.posts.to(CHANNEL),
            intake.mock.written_texts(),
            super::super::super::OwnerTurn::drive_results(CHANNEL),
            intake.writer_state()
        );
    }
    assert!(
        !intake
            .mock
            .written_texts()
            .iter()
            .any(|text| text.contains("HTTP session answered")),
        "the bridge does not publish O's body"
    );
    assert!(intake.posts.to(CHANNEL + 1).is_empty());
}

struct FallbackCount;
impl FallbackCount {
    fn new() -> Self {
        *COUNTED_DISPATCHES.lock().unwrap() = Some((CHANNEL, 0));
        Self
    }
    fn assert_none(&self) {
        assert_eq!(*COUNTED_DISPATCHES.lock().unwrap(), Some((CHANNEL, 0)));
    }
}
impl Drop for FallbackCount {
    fn drop(&mut self) {
        *COUNTED_DISPATCHES.lock().unwrap() = None;
    }
}

fn idle_endpoint(endpoint: &str) {
    let mut fx = Fixture::new(&format!("http-{endpoint}"), None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let registry_guard = fx.rig.registry_on_this_thread();
    let admission_guard = open_admission();
    let intake = Intake::new(&fx, &started);
    let heads = answer_input(&fx, &intake, &started);
    let fallback = FallbackCount::new();
    let original = intake.runtime.block_on(async {
        let app = app(&fx, &intake).await;
        let first = call(&app, endpoint, "external-http-bound-origin").await;
        assert_eq!(first.0, StatusCode::OK, "{endpoint}: {}", first.1);
        tokio::time::timeout(LIMIT, async {
            loop {
                let state = intake
                    .mailbox
                    .v2_request(|slot| slot.as_ref().unwrap().json())
                    .await
                    .unwrap();
                if fx.rig.sends().len() == before + 2
                    && state["items"] == json!([])
                    && state["head_injection"].is_null()
                    && state["retained_inputs"] == json!([])
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{endpoint}: {error}; results={:?}",
                super::super::super::OwnerTurn::drive_results(CHANNEL)
            )
        });
        completed(&intake, 1).await;
        assert!(
            !intake.mock.written_texts().is_empty(),
            "the delivery presentation actually reaches Discord's message boundary"
        );
        {
            let heads = heads.lock().unwrap();
            assert_eq!(heads.len(), 2);
            assert_eq!(heads[0]["original"]["channel_id"], CHANNEL);
            let attempt = heads[0]["attempt"]["nonce"].as_str().unwrap();
            assert_eq!(
                crate::services::claude_tui::host_input::busy::observed(attempt),
                0,
                "Idle executes its own turn"
            );
        }
        let closed_writer = (endpoint == "deliver")
            .then(|| crate::services::herdr_launch::force_writer_accepts(Some(false)));
        let replay = call(&app, endpoint, "external-http-bound-origin").await;
        assert_eq!(replay, first, "replay precedes changed readiness");
        drop(closed_writer);
        let sends = fx.rig.sends();
        assert_eq!(sends.len(), before + 2, "{sends:?}");
        assert!(sends[before]["text"].as_str().unwrap().contains(INPUT));
        assert_eq!(sends[before + 1]["keys"], json!(["enter"]));
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
        fallback.assert_none();
        let next = call(&app, endpoint, "external-http-next-origin").await;
        assert_eq!(next.0, StatusCode::OK, "{endpoint}: {}", next.1);
        completed(&intake, 2).await;
        assert_eq!(fx.rig.sends().len(), before + 4);
        assert_eq!(heads.lock().unwrap().len(), 4);
        assert_eq!(
            intake
                .posts
                .to(CHANNEL)
                .iter()
                .filter(|text| text.contains("HTTP session answered 1"))
                .count(),
            1
        );
        assert_eq!(
            intake
                .posts
                .to(CHANNEL)
                .iter()
                .filter(|text| text.contains("HTTP session answered 2"))
                .count(),
            1
        );
        fallback.assert_none();
        first
    });
    // Connections opened by the first intake cannot outlive its I/O runtime.
    intake.runtime.block_on(fx.pool.close());
    drop(intake);
    fx.pool = fx
        .rt
        .block_on(fx.db.as_ref().unwrap().connect_and_migrate());
    let restarted = Intake::new(&fx, &started);
    restarted.runtime.block_on(async {
        let app = app(&fx, &restarted).await;
        let replay = call(&app, endpoint, "external-http-bound-origin").await;
        assert_eq!(
            replay, original,
            "a rebuilt runtime replays the persisted GUID"
        );
        assert_eq!(fx.rig.sends().len(), before + 4);
        assert!(
            restarted.posts.to(CHANNEL).is_empty(),
            "adopted output is not reposted"
        );
        assert_eq!(restarted.mailbox.has_active_turn().await, Ok(false));
        assert!(inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL).is_none());
        fallback.assert_none();
    });
    drop(restarted);
    drop(admission_guard);
    drop(registry_guard);
}

#[test]
fn external_herdr_http_start_executes_once_pg() {
    idle_endpoint("start");
}

#[test]
fn external_herdr_http_idle_deliver_executes_once_pg() {
    idle_endpoint("deliver");
}

#[test]
fn external_herdr_http_refusals_do_not_claim_the_guid_pg() {
    let fx = Fixture::new("http-refusals", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let registry_guard = fx.rig.registry_on_this_thread();
    let admission_guard = open_admission();
    let intake = Intake::new(&fx, &started);
    let saved = match fx.record() {
        HostedRecord::Known(record) => serde_json::to_value(record).unwrap(),
        other => panic!("{other:?}"),
    };
    intake.runtime.block_on(async {
        let app = app(&fx, &intake).await;
        for (reason, expected) in [
            ("switch", "executor_not_wired"),
            ("writer", "o_writer_not_ready"),
            ("host", "hosted_row_unreadable"),
        ] {
            let switch = (reason == "switch").then(|| force_switch_for_test(Some(false)));
            let writer = (reason == "writer")
                .then(|| crate::services::herdr_launch::force_writer_accepts(Some(false)));
            if reason == "host" {
                sqlx::query("UPDATE sessions SET hosted_execution=$1 WHERE channel_id=$2")
                    .bind(json!({"schema":99}))
                    .bind(CHANNEL.to_string())
                    .execute(&fx.pool)
                    .await
                    .unwrap();
            }
            for endpoint in ["start", "deliver"] {
                let response = call(&app, endpoint, &format!("refused-{reason}-{endpoint}")).await;
                assert_eq!(
                    response.0,
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "{reason}/{endpoint}: {}",
                    response.1
                );
                assert!(
                    response.1.to_string().contains(expected),
                    "{reason}/{endpoint}: {}",
                    response.1
                );
                assert_eq!(fx.rig.sends().len(), before);
                let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM imsg_reply_obligations")
                    .fetch_one(&fx.pool)
                    .await
                    .unwrap();
                assert_eq!(rows, 0, "{reason}/{endpoint} claimed the origin");
                let state = intake
                    .mailbox
                    .v2_request(|slot| slot.as_ref().unwrap().json())
                    .await
                    .unwrap();
                assert_eq!(state["items"], json!([]));
                assert_eq!(
                    state["admission_receipts"].as_array().map_or(0, Vec::len),
                    0
                );
                assert!(state["head_injection"].is_null());
            }
            if reason == "host" {
                sqlx::query("UPDATE sessions SET hosted_execution=$1 WHERE channel_id=$2")
                    .bind(saved.clone())
                    .bind(CHANNEL.to_string())
                    .execute(&fx.pool)
                    .await
                    .unwrap();
            }
            drop(writer);
            drop(switch);
        }
    });
    drop(intake);
    drop(admission_guard);
    drop(registry_guard);
}

fn native_busy(started: &Started) {
    append(
        &started.path,
        &[
            json!({"type":"user","uuid":uuid(),"sessionId":started.session,
            "message":{"role":"user","content":"already running"}}),
            json!({"type":"assistant","uuid":uuid(),"apiBlockIndex":0,"sessionId":started.session,
            "message":{"id":uuid(),"role":"assistant","content":[{"type":"text","text":"working"}]}}),
        ],
    );
    assert!(
        crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&started.path).is_busy()
    );
}

#[test]
fn external_herdr_http_machine_waits_for_its_own_turn_pg() {
    let fx = Fixture::new("http-machine-busy", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let registry_guard = fx.rig.registry_on_this_thread();
    let admission_guard = open_admission();
    let intake = Intake::new(&fx, &started);
    let heads = answer_on_enter(&fx, &intake, &started, true);
    let fallback = FallbackCount::new();
    intake.runtime.block_on(async {
        let app = app(&fx, &intake).await;
        let first = call(&app, "start", "machine-holder-origin").await;
        assert_eq!(first.0, StatusCode::OK);
        tokio::time::timeout(LIMIT, async {
            while heads.lock().unwrap().len() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(true));
        let holder = intake.mailbox.cancel_token().await.unwrap().unwrap();
        assert_eq!(
            call(&app, "start", "machine-holder-origin").await,
            first,
            "retry of the active GUID does not admit another original"
        );
        assert_eq!(fx.rig.sends().len(), before + 2);
        let response = call_source(&app, "start", "machine-busy-origin", "api").await;
        assert_eq!(response.0, StatusCode::OK, "{}", response.1);
        let active = intake.mailbox.cancel_token().await.unwrap().unwrap();
        assert!(
            Arc::ptr_eq(&holder, &active),
            "machine admission keeps the running actor"
        );
        assert_eq!(fx.rig.sends().len(), before + 2, "machine never joins Busy");
        let state = intake
            .mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        assert!(
            !state["items"].as_array().unwrap().is_empty(),
            "the waiting original is retained: {state}"
        );
        finish_answer(&started.path, &started.session, 1);
        completed(&intake, 2).await;
        assert_eq!(fx.rig.sends().len(), before + 4);
        for (response, expected) in [
            (&first.1, "HTTP session answered 1"),
            (&response.1, "HTTP session answered 2"),
        ] {
            let turn = response["turn_id"].as_str().unwrap();
            let answers: Vec<String> = sqlx::query_scalar(
                "SELECT assistant_message FROM session_transcripts WHERE turn_id = $1",
            )
            .bind(turn)
            .fetch_all(&fx.pool)
            .await
            .unwrap();
            assert_eq!(
                answers
                    .iter()
                    .map(|answer| answer.trim())
                    .collect::<Vec<_>>(),
                [expected]
            );
        }
        let heads = heads.lock().unwrap();
        assert_eq!(heads.len(), 4);
        let nonce = heads[2]["attempt"]["nonce"].as_str().unwrap();
        assert_eq!(
            crate::services::claude_tui::host_input::busy::observed(nonce),
            0
        );
        fallback.assert_none();
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    });
    drop(intake);
    drop(admission_guard);
    drop(registry_guard);
}

#[test]
fn external_herdr_http_person_joins_busy_at_early_and_late_boundaries_pg() {
    for late in [false, true] {
        for endpoint in ["start", "deliver"] {
            let fx = Fixture::new(&format!("http-person-{endpoint}-{late}"), None);
            let launcher = Arc::new(Launcher::default());
            let (_, started) = reconnect::launch(&fx, &launcher);
            let before = fx.rig.sends().len();
            let registry_guard = fx.rig.registry_on_this_thread();
            let admission_guard = open_admission();
            let intake = Intake::new(&fx, &started);
            let heads = answer_input(&fx, &intake, &started);
            let fallback = FallbackCount::new();
            intake.runtime.block_on(async {
                let app = app(&fx, &intake).await;
                let barrier = if late {
                    Some(super::super::super::OwnerTurn::pause_late(CHANNEL))
                } else {
                    native_busy(&started);
                    intake.install_holder(&fx, &started).await;
                    None
                };
                let response = call(&app, endpoint, "person-busy-origin").await;
                assert_eq!(
                    response.0,
                    StatusCode::OK,
                    "{endpoint}/{late}: {}",
                    response.1
                );
                if let Some((entered, release)) = barrier {
                    entered
                        .recv_timeout(LIMIT)
                        .expect("the actual headless owner reaches its late observer");
                    assert_eq!(
                        fx.rig.sends().len(),
                        before,
                        "Idle delegation itself writes no key"
                    );
                    native_busy(&started);
                    release.send(()).unwrap();
                }
                tokio::time::timeout(LIMIT, async {
                    while fx.rig.sends().len() != before + 2 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                if late {
                    idle_turn_unwound(
                        &intake,
                        response.1["turn_id"]
                            .as_str()
                            .unwrap()
                            .rsplit(':')
                            .next()
                            .unwrap()
                            .parse()
                            .unwrap(),
                    )
                    .await;
                }
                tokio::time::timeout(LIMIT, async {
                    while heads.lock().unwrap().len() != 2 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                let heads = heads.lock().unwrap();
                assert_eq!(heads.len(), 2);
                let nonce = heads[0]["attempt"]["nonce"].as_str().unwrap();
                assert_eq!(
                    crate::services::claude_tui::host_input::busy::observed(nonce),
                    1,
                    "the person joins the native Busy turn once"
                );
                assert_eq!(heads[0]["original"]["channel_id"], CHANNEL);
                fallback.assert_none();
            });
            drop(intake);
            drop(admission_guard);
            drop(registry_guard);
        }
    }
}

#[test]
fn external_herdr_http_late_switch_change_retains_the_admitted_original_pg() {
    let fx = Fixture::new("http-late-switch", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let registry_guard = fx.rig.registry_on_this_thread();
    let admission_guard = open_admission();
    let intake = Intake::new(&fx, &started);
    intake.runtime.block_on(async {
        let app = app(&fx, &intake).await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let probe = crate::server::routes::DeliveryProbe {
            entered: Some(entered.clone()), release: Some(release.clone()), ..Default::default()
        };
        let delivery = crate::server::routes::DELIVERY_PROBE_FOR_TEST.scope(
            probe, call(&app, "deliver", "late-admitted-origin"),
        );
        let close = async {
            tokio::time::timeout(LIMIT, entered.notified()).await.unwrap();
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM imsg_reply_obligations WHERE origin_id='late-admitted-origin'")
                .fetch_one(&fx.pool).await.unwrap();
            assert_eq!(count, 1, "the barrier is after origin admission");
            assert_eq!(fx.rig.sends().len(), before);
            let mut changed = crate::config_live_reload::current().unwrap().as_ref().clone();
            changed.runtime.herdr_turn_enabled = Some(false);
            crate::config_live_reload::install(changed);
            release.notify_one();
        };
        let (response, ()) = tokio::join!(delivery, close);
        assert_eq!(response.0, StatusCode::OK, "{}", response.1);
        assert_eq!(response.1["delivery"], "queued");
        let state = tokio::time::timeout(LIMIT, async {
            loop {
                let state = intake.mailbox.v2_request(|slot| slot.as_ref().unwrap().json()).await.unwrap();
                if state["retained_inputs"].as_array().is_some_and(|inputs| inputs.len() == 1) {
                    break state;
                }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        assert_eq!(state["retained_inputs"][0]["input"]["original"]["text"], INPUT);
        assert!(state["retained_inputs"][0]["reason"].to_string().contains("executor_not_wired"));
        discord::queue_io::input_owner::notice::drain_synchronously_for_tests(
            &fx.pool, &intake.mailbox, "claude", CHANNEL,
        ).await.unwrap();
        let notices: Vec<String> = sqlx::query_scalar("SELECT content FROM message_outbox WHERE source='retained_input_notice' AND target=$1")
            .bind(format!("channel:{CHANNEL}")).fetch_all(&fx.pool).await.unwrap();
        assert_eq!(notices.len(), 1, "the retained original has one visible notice");
        assert_eq!(fx.rig.sends().len(), before);
        let replay = call(&app, "deliver", "late-admitted-origin").await;
        assert_eq!(replay, response);
        assert_eq!(fx.rig.sends().len(), before);
    });
    drop(intake);
    drop(admission_guard);
    drop(registry_guard);
}

#[test]
fn external_herdr_http_driver_rechecks_idle_after_its_new_reservation_pg() {
    let fx = Fixture::new("http-driver-recheck", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let registry_guard = fx.rig.registry_on_this_thread();
    let admission_guard = open_admission();
    let intake = Intake::new(&fx, &started);
    let heads = answer_input(&fx, &intake, &started);
    let armed = Arc::new(AtomicBool::new(false));
    let (seen, mailbox, runtime) = (
        armed.clone(),
        intake.mailbox.clone(),
        intake.runtime.handle().clone(),
    );
    let path = started.path.clone();
    let native = started.session.clone();
    let (reserved, reservation) = std::sync::mpsc::channel();
    fx.rig.on_reply(move |call| {
        if !matches!(call.request, crate::services::session_host::herdr_socket_rig_tests::HerdrRequest::PaneRead { .. })
            || seen.load(Ordering::SeqCst) {
            return;
        }
        let owned = tokio::task::block_in_place(|| runtime.block_on(mailbox.v2_request(|slot| {
            let state = slot.as_ref().unwrap().json()?;
            Ok(!state["head_injection"].is_null())
        }))).unwrap();
        if !owned || seen.swap(true, Ordering::SeqCst) { return; }
        let (path, native, reserved) = (path.clone(), native.clone(), reserved.clone());
        crate::services::claude_tui::readiness::tests::clear_tests::after_next_reserve(move || {
            append(&path, &[
                json!({"type":"user","uuid":uuid(),"sessionId":native,"message":{"role":"user","content":"native turn wins"}}),
                json!({"type":"assistant","uuid":uuid(),"sessionId":native,"message":{"id":uuid(),"role":"assistant","content":[{"type":"text","text":"working"}]}}),
            ]);
            reserved.send(()).unwrap();
        });
    });
    intake.runtime.block_on(async {
        let app = app(&fx, &intake).await;
        let count = super::super::super::OwnerTurn::drive_results(CHANNEL).len();
        let response = call(&app, "deliver", "driver-recheck-origin").await;
        assert_eq!(response.0, StatusCode::OK, "{}", response.1);
        reservation.recv_timeout(LIMIT).expect(
            "the driver acquired a fresh reservation after Idle delegation returned its read proof",
        );
        tokio::time::timeout(LIMIT, async {
            while super::super::super::OwnerTurn::drive_results(CHANNEL).len() == count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(armed.load(Ordering::SeqCst));
        assert!(
            crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&started.path)
                .is_busy()
        );
        assert_eq!(
            fx.rig.sends().len(),
            before,
            "an Idle driver never starts while the same incarnation is Busy"
        );
        assert!(heads.lock().unwrap().is_empty());
        let state = intake
            .mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        let queued = state["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["text"] == INPUT);
        let retained = state["retained_inputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["input"]["original"]["text"] == INPUT);
        assert!(
            queued || retained,
            "the untouched original remains visible in owner custody: {state}"
        );
    });
    drop(intake);
    drop(admission_guard);
    drop(registry_guard);
}

fn internal_snapshot_start(flavor: &str) {
    use discord::queue_io::input_owner::origin::InputOrigin;
    let fx = Fixture::new(&format!("http-internal-{flavor}"), None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let registry_guard = fx.rig.registry_on_this_thread();
    let admission_guard = open_admission();
    let intake = Intake::new(&fx, &started);
    // The snapshot pane is absent; the existing native probe must reach its refusal driver.
    std::fs::write(
        fx.cwd.path().join("tmux"),
        "#!/bin/sh\necho \"can't find session: absent snapshot\" >&2\nexit 1\n",
    )
    .unwrap();
    intake.runtime.block_on(async {
        let router = app(&fx, &intake).await;
        let reservation = discord::router::reserve_headless_turn();
        let turn = reservation.turn_id(ChannelId::new(CHANNEL));
        let outbox_id = i64::try_from(reservation.user_msg_id().get() & 0x7fff_ffff_ffff_ffff).unwrap();
        // Per-test databases share a process deduper keyed by the outbox row ID.
        sqlx::query("SELECT setval(pg_get_serial_sequence('message_outbox','id')::regclass,$1,false)")
            .bind(outbox_id).execute(&fx.pool).await.unwrap();
        let label = format!("scheduled:snapshot-{flavor}");
        let mut metadata = json!({"agent_id":"busy-entry","scheduled_message_id":"smsg-8125",
            "scheduled_snapshot_session_label":label});
        let mut context = json!({"origin":"scheduled","provider":"claude","turn_id":turn,
            "metadata":metadata,"synthetic_label":label,"source":"scheduled_message",
            "channel_name_hint":fx.logical().strip_prefix("AgentDesk-claude-"),"is_dm":false});
        if flavor == "restored" {
            context = metadata.clone();
            context["origin"] = "scheduled".into();
            context["provider"] = "claude".into();
            context["turn_id"] = turn.clone().into();
        } else if flavor == "routine" {
            metadata = json!({"routine_id":"routine-8125","execution_strategy":"persistent"});
            context["metadata"] = metadata;
            context["origin"] = "routine".into();
        }
        assert_eq!(InputOrigin::from_context(Some(&context)), InputOrigin::Routine);
        let pane = discord::router::message_handler::headless_turn::owner_start_pane(
            &intake.shared, ChannelId::new(CHANNEL), &context).await.unwrap().unwrap();
        assert!(!pane.1 && pane.0 != fx.logical(), "snapshot B differs from the Bound channel A");
        let hosted: Value = sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id=$1")
            .bind(CHANNEL.to_string()).fetch_one(&fx.pool).await.unwrap();
        let execution = ChannelMailboxHandle::owner_input_execution("claude", CHANNEL, false);
        let (original, namespace) = discord::OWNER_ADOPTS_FOR_TEST.scope(true,
            intake.mailbox.owned_start(execution, (&turn, CHANNEL, "frozen snapshot original B")))
            .await.unwrap().unwrap();
        let input = ChannelMailboxHandle::new_owner_input(&original, &namespace, "claude", context.clone());
        assert!(input.confirm_admission(&intake.mailbox).await.unwrap());
        let registry = Arc::new(HealthRegistry::new());
        registry.register("claude".into(), intake.shared.clone()).await;
        registry.register_http("claude".into(), intake.shared.serenity_http_or_token_fallback().unwrap()).await;
        ChannelMailboxHandle::owner_input_wake(&registry, "claude", CHANNEL).await;
        let rejected = tokio::time::timeout(LIMIT, async {
            loop {
                if let Some(reason) = input.start_rejection(&intake.mailbox).await.unwrap() {
                    break reason;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        let snapshot = intake.mailbox.v2_request(|slot| slot.as_ref().unwrap().json()).await.unwrap();
        let screen = discord::router::message_handler::provider_dispatch::OwnerTurn::start_wait_screen_for_test(
            &intake.shared, ChannelId::new(CHANNEL), &context).await;
        assert!(rejected.is_ok(), "the actual admitted snapshot must reach start-rejected instead of Watch: screen={screen:?}, owner={snapshot}");
        let rejected = rejected.unwrap();
        assert!(rejected.starts_with("start_rejected: ") && rejected.contains("executor_not_wired"), "{rejected}");
        assert_eq!(snapshot["retained_inputs"][0]["input"]["original"]["text"], original.text);
        assert_eq!(snapshot["retained_inputs"][0]["disposition"], "known_not_sent");
        assert_eq!(fx.rig.sends().len(), before, "the rejected snapshot writes nothing to A");
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(false));
        assert!(inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL).is_none());
        let after: Value = sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id=$1")
            .bind(CHANNEL.to_string()).fetch_one(&fx.pool).await.unwrap();
        assert_eq!(after, hosted, "internal refusal preserves the existing Bound execution");
        discord::queue_io::input_owner::notice::drain_synchronously_for_tests(
            &fx.pool, &intake.mailbox, "claude", CHANNEL).await.unwrap();
        let notices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE source='retained_input_notice' AND target=$1")
            .bind(format!("channel:{CHANNEL}")).fetch_one(&fx.pool).await.unwrap();
        assert_eq!(notices, 1, "one retained original owes one visible notice");
        let pool = Arc::new(fx.pool.clone());
        assert_eq!(crate::server::outbox_worker::drain_message_outbox_once(&pool, &registry, "snapshot-refusal").await, 1);
        assert_eq!(intake.mock.delivered_texts().iter().filter(|text| text.contains("원문을 보관")).count(), 1);
        assert_eq!(crate::server::outbox_worker::drain_message_outbox_once(&pool, &registry, "snapshot-refusal-repeat").await, 0);
        let heads = answer_input(&fx, &intake, &started);
        let next = call_source(&router, "start", &format!("after-internal-{flavor}"), "api").await;
        assert_eq!(next.0, StatusCode::OK, "{}", next.1);
        completed(&intake, 1).await;
        assert_eq!(heads.lock().unwrap().len(), 2);
        assert_eq!(fx.rig.sends().len(), before + 2, "only the later external original runs");
    });
    drop(intake);
    drop(admission_guard);
    drop(registry_guard);
}

#[test]
fn internal_herdr_snapshot_owner_reaches_its_legacy_refusal_pg() {
    internal_snapshot_start("scheduled");
}

#[test]
fn internal_herdr_restored_snapshot_owner_reaches_its_legacy_refusal_pg() {
    internal_snapshot_start("restored");
}

#[test]
fn internal_herdr_routine_owner_reaches_its_legacy_refusal_pg() {
    internal_snapshot_start("routine");
}

/// `foreign` hands the channel's hosted record to another execution before the raw closer lands.
fn execution_gone(terminal: Option<bool>, foreign: bool) {
    let tag = match (terminal, foreign) {
        (None, _) => "missing",
        (Some(false), false) => "raw",
        (Some(false), true) => "foreign",
        (Some(true), _) => "flushed",
    };
    let fx = Fixture::new(&format!("http-execution-gone-{tag}"), None);
    let hooks = terminal
        .is_none()
        .then(crate::services::claude_tui::hook_server::hook_receiver_router);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let registry_guard = fx.rig.registry_on_this_thread();
    let admission_guard = open_admission();
    let hook_endpoint = terminal.is_none().then(|| {
        crate::services::claude_tui::hook_server::publish_hook_endpoint("http://127.0.0.1:1".into())
    });
    assert_eq!(
        started.hook(
            &fx,
            "Stop",
            &json!({"session_id":started.session,"transcript_path":started.path})
        ),
        202
    );
    let intake = Intake::with_launch(
        &fx,
        &started,
        ExpectedPin::Verified,
        Arc::default(),
        (terminal.is_none(), None),
        None,
    );
    let (path, session) = (started.path.clone(), started.session.clone());
    let pasted = Mutex::new(String::new());
    fx.rig.on_send(move |params| {
        if let Some(text) = params["text"].as_str().filter(|text| !text.is_empty()) {
            *pasted.lock().unwrap() = text
                .trim_start_matches("\u{1b}[200~")
                .trim_end_matches("\u{1b}[201~")
                .into();
        }
        // The native Enter writes a user row; the later shell launch command does not.
        if params["keys"] == json!(["enter"]) && params["text"].as_str().is_none_or(str::is_empty) {
            append(
                &path,
                &[json!({"type":"user","uuid":uuid(),"sessionId":session,
                "timestamp":chrono::Utc::now().to_rfc3339(),
                "message":{"role":"user","content":pasted.lock().unwrap().clone()}})],
            );
        }
    });
    let fallback = FallbackCount::new();
    intake.runtime.block_on(async {
        let app = app(&fx, &intake).await;
        let response = call(&app, "start", "external-execution-gone-origin").await;
        assert_eq!(response.0, StatusCode::OK, "{}", response.1);
        let actor = tokio::time::timeout(LIMIT, async {
            loop {
                if let Some(actor) = intake.mailbox.cancel_token().await.unwrap()
                    && actor.herdr_interrupt_state().is_some_and(|state| state.turn_start.get().is_some())
                    && fx.rig.sends().len() == before + 2 {
                    break actor;
                }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        let row = inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL).unwrap();
        assert_eq!(row.runtime_kind, Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui));
        assert_eq!(row.tmux_session_name.as_deref(), Some(fx.logical()));
        assert!(!discord::is_synthetic_headless_message_id_raw(row.current_msg_id),
            "the response anchor is absent or a real Discord message");
        let flush = terminal.map(|newline| {
            let path = started.path.clone();
            let session = started.session.clone();
            let pool = fx.pool.clone();
            let once = Arc::new(std::sync::atomic::AtomicBool::new(false));
            discord::recovery_engine::herdr_row_end::tests::on_while("live_after_end", move || {
                let first = !once.swap(true, Ordering::SeqCst);
                let (path, session, pool) = (path.clone(), session.clone(), pool.clone());
                Box::pin(async move {
                    if !first { return; }
                    if foreign {
                        sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(jsonb_set(jsonb_set(hosted_execution,
                            '{execution_nonce}', '\"successor\"'), '{source_ref,execution_nonce}', '\"successor\"'),
                            '{expected,binding_nonce}', '\"successor\"') WHERE channel_id=$1")
                            .bind(CHANNEL.to_string()).execute(&pool).await.unwrap();
                    }
                    let now = chrono::Utc::now().to_rfc3339();
                    let answer = json!({"type":"assistant","uuid":uuid(),"sessionId":session,
                        "timestamp":now,"apiBlockIndex":0,"message":{"id":uuid(),"role":"assistant",
                        "content":[{"type":"text","text":"HTTP session answered after E"}],"stop_reason":"end_turn"}});
                    let record = json!({"type":"system","subtype":"turn_duration","sessionId":session,"timestamp":now});
                    // One write, so the poll that reads the answer also holds the closer.
                    let end = if newline { "\n" } else { "" };
                    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
                    use std::io::Write;
                    file.write_all(format!("{answer}\n{record}{end}").as_bytes()).unwrap();
                })
            })
        });
        let notice = super::super::live_end::LiveEnd::notice();
        let mut next_message = None;
        if terminal.is_none() {
            let next = call_source(&app, "start", "after-execution-gone-machine", "api").await;
            assert_eq!(next.0, StatusCode::OK, "{}", next.1);
            next_message = Some(next.1["turn_id"].as_str().unwrap().rsplit(':').next().unwrap().parse::<u64>().unwrap());
            assert_eq!(fx.rig.sends().len(), before + 2, "the machine waits for its own turn");
            intake.mock.refuse_message_writes(true);
        }
        super::super::live_end::kill(&fx);
        discord::recovery_engine::herdr_row_end::row_end_pass(&intake.shared, &ProviderKind::Claude).await;
        if terminal.is_none() {
            tokio::time::timeout(LIMIT, async {
                loop {
                    if intake.mock.refused_texts().iter().any(|text| text == &notice) { break; }
                    discord::recovery_engine::herdr_row_end::row_end_pass(&intake.shared, &ProviderKind::Claude).await;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.expect("the actual end pass reaches the refused notice transport");
            let after_refusal = intake.shared.session_transition_locks.lock(ChannelId::new(CHANNEL)).lock_owned().await;
            let held = inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL)
                .expect("a refused notice preserves the captured inflight row");
            assert_eq!(held.turn_nonce, row.turn_nonce, "a refused notice preserves the same inflight episode");
            assert!(intake.mailbox.cancel_token().await.unwrap().is_some_and(|token| Arc::ptr_eq(&token, &actor)),
                "a refused notice preserves the exact mailbox token");
            assert!(!actor.is_completion_cleanup() && !actor.cancelled.load(Ordering::SeqCst));
            assert_eq!(fx.rig.sends().len(), before + 2);
            assert!(!intake.mock.delivered_texts().contains(&notice));
            drop(after_refusal);
            intake.mock.refuse_message_writes(false);
            // The shell from the ended execution has gone; the queued owner launches its successor.
            std::fs::write(fx.cwd.path().join("tmux"),
                "#!/bin/sh\necho \"can't find session: ended legacy pane\" >&2\nexit 1\n").unwrap();
            fx.rig.show_panes(&[]);
            fx.rig.answer("workspace.create", json!({"type":"workspace_created","root_pane":{
                "pane_id":PANE,"workspace_id":"w1","tab_id":"w1:1","revision":0,"cwd":fx.cwd.path()}}));
            discord::recovery_engine::herdr_row_end::row_end_pass(&intake.shared, &ProviderKind::Claude).await;
            let pending = tokio::time::timeout(LIMIT, async {
                loop {
                    discord::recovery_engine::herdr_row_end::row_end_pass(&intake.shared, &ProviderKind::Claude).await;
                    let raw: Option<Value> = sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id=$1")
                        .bind(CHANNEL.to_string()).fetch_one(&fx.pool).await.unwrap();
                    if let HostedRecord::Known(record) = HostedRecord::decode(raw.as_ref())
                        && record.state == HostedState::Pending && record.location.is_some() {
                        break record;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.expect("completion wakes the already queued machine into its actual next launch");
            let nonce = &pending.execution_nonce;
            assert_ne!(nonce, actor.herdr_interrupt_state().unwrap().submission.lock().unwrap().execution.as_ref().unwrap());
            let context = crate::services::tui_prompt_dedupe::binding_context::execution_context("claude", nonce).unwrap();
            let session = context.expected_native_session_id.unwrap();
            assert_eq!(session, started.session, "the successor resumes the channel's native source");
            let context_path = crate::config::runtime_root().unwrap().join(format!("runtime/binding_contexts/claude/{nonce}.json"));
            let launch = &fx.rig.sends()[before + 2];
            assert_eq!(launch["keys"], json!(["enter"]));
            assert!(launch["text"].as_str().unwrap().starts_with("bash "), "one shell command launches the successor");
            assert_eq!(fx.rig.sends().len(), before + 3, "no original input before SessionStart");
            fx.rig.run_provider(&context_path, false);
            fx.rig.foreground(&[crate::services::session_host::herdr_socket_rig_tests::PROVIDER]);
            fx.rig.show_panes(&[PANE]);
            let envelope = HookBindingEnvelope::capture_from_env("claude", |name| {
                (name == "AGENTDESK_BINDING_CONTEXT").then(|| context_path.clone().into_os_string())
            }).encode().unwrap();
            let successor = Started { session, path: started.path.clone(), envelope };
            append(&successor.path, &[json!({"type":"system","subtype":"turn_duration",
                "sessionId":successor.session,"timestamp":chrono::Utc::now().to_rfc3339()})]);
            let heads = answer_input(&fx, &intake, &successor);
            use crate::services::claude_tui::hook_server::relay_receipts::{
                RELAY_REQUEST_ID_HEADER, RELAY_PUBLISHED_AT_HEADER, RELAY_DEADLINE_HEADER, RELAY_RESPOND_BY_HEADER,
            };
            let now = chrono::Utc::now();
            let hook = Request::builder().method("POST")
                .uri(format!("/hooks/claude/SessionStart?session_id={}", successor.session))
                .header("content-type", "application/json")
                .header(crate::services::tui_prompt_dedupe::binding_context::BINDING_HEADER, successor.envelope.as_str())
                .header(RELAY_REQUEST_ID_HEADER, uuid())
                .header(RELAY_PUBLISHED_AT_HEADER, now.to_rfc3339())
                .header(RELAY_DEADLINE_HEADER, (now + chrono::Duration::minutes(5)).to_rfc3339())
                .header(RELAY_RESPOND_BY_HEADER, (now + chrono::Duration::minutes(4)).to_rfc3339())
                .body(Body::from(json!({"session_id":successor.session,"source":"resume",
                    "transcript_path":successor.path}).to_string())).unwrap();
            // The receiver and executor must share the live hook bus.
            let hooked = hooks.as_ref().unwrap().clone().oneshot(hook).await.unwrap();
            assert_eq!(hooked.status(), StatusCode::ACCEPTED);
            completed(&intake, 1).await;
            {
                let heads = heads.lock().unwrap();
                assert_eq!(heads.len(), 2, "the next machine original has one paste and Enter");
                assert_eq!(heads[0]["original"]["message_id"].as_u64(), next_message);
                assert_eq!(heads[0]["original"]["author_is_bot"], true);
                assert_eq!(heads[0]["attempt"]["target"]["execution_nonce"], *nonce);
                assert_eq!(crate::services::claude_tui::host_input::busy::observed(heads[0]["attempt"]["nonce"].as_str().unwrap()), 0);
            }
            assert!(actor.is_completion_cleanup() && actor.cancelled.load(Ordering::SeqCst));
            assert_eq!(fx.rig.sends().len(), before + 5, "one launch command, then one paste and Enter per original");
            // The native writer posts one answer and its existing turn-end tool summary.
            tokio::time::timeout(LIMIT, async {
                while !intake.posts.to(CHANNEL).iter().any(|text| text.starts_with("-# ⏱️ ")) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.expect("the next own-turn publishes its tool summary");
            let posts = intake.posts.to(CHANNEL);
            assert_eq!(posts.iter().filter(|text| text.contains("HTTP session answered")).count(), 1);
            assert_eq!(posts.iter().filter(|text| text.starts_with("-# ⏱️ ")).count(), 1);
            assert_eq!(posts.len(), 2);
            assert_eq!(intake.mock.delivered_texts().iter().filter(|text| *text == &notice).count(), 1);
            let bound: Value = sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id=$1")
                .bind(CHANNEL.to_string()).fetch_one(&fx.pool).await.unwrap();
            let HostedRecord::Known(bound) = HostedRecord::decode(Some(&bound)) else { panic!("the next own-turn has a Bound execution") };
            assert_eq!(bound.state, HostedState::Bound);
            assert_eq!(&bound.execution_nonce, nonce);
            assert_eq!(fx.rig.requests().iter().filter(|request| request["method"] == "workspace.create").count(), 1);
            fallback.assert_none();
            return;
        }
        if foreign {
            // The poll that posts the answer already offered the closer; another execution's end
            // proves nothing for this one, so only the completed record settles it.
            tokio::time::timeout(LIMIT, async {
                while !intake.posts.to(CHANNEL).iter().any(|text| text.contains("answered after E")) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.expect("the answer is read with its closer");
            assert!(native_results().is_empty(), "no raw closer result from a foreign end");
            let mut file = std::fs::OpenOptions::new().append(true).open(&started.path).unwrap();
            std::io::Write::write_all(&mut file, b"\n").unwrap();
        }
        completed(&intake, usize::from(terminal.is_some())).await;
        if foreign {
            let results = native_results();
            assert!(results.len() == 1 && results[0]["native_proof"]["raw"].is_null(), "{results:?}");
        }
        for trial in 0..2 {
            discord::recovery_engine::herdr_row_end::row_end_pass(&intake.shared, &ProviderKind::Claude).await;
            tokio::time::sleep(Duration::from_millis(10 + trial)).await;
        }
        let notices = intake.mock.written_texts().into_iter().filter(|text| text == &notice).count();
        assert_eq!(notices, usize::from(terminal.is_none()), "a native terminal is left to its reader");
        assert_eq!(fx.rig.sends().len(), before + 2);
        fallback.assert_none();
        drop(flush);
    });
    drop(intake);
    drop(hook_endpoint);
    drop(admission_guard);
    drop(registry_guard);
}

#[test]
fn external_herdr_http_execution_gone_without_terminal_settles_once_pg() {
    execution_gone(None, false);
}

#[test]
fn external_herdr_http_execution_gone_yields_a_flushed_terminal_pg() {
    for newline in [false, true] {
        execution_gone(Some(newline), false);
    }
}

#[test]
fn external_herdr_http_raw_closer_needs_its_own_execution_end_pg() {
    execution_gone(Some(false), true);
}

/// Native results saved for this channel's inputs.
fn native_results() -> Vec<serde_json::Map<String, Value>> {
    let history =
        crate::services::tui_input::human_frame::history::v2::Registry::runtime().unwrap();
    let registrations = history.native_classification_registrations().unwrap();
    registrations
        .iter()
        .filter(|r| r.ticket.channel_id == CHANNEL)
        .filter_map(|r| {
            history
                .native_settlement(&r.namespace, &r.ticket)
                .unwrap()
                .outcome
                .map(|o| o.extra)
        })
        .collect()
}

#[test]
fn external_herdr_http_turn_mode_aborted_terminal_settles_without_synthetic_reply_pg() {
    let fx = Fixture::new("http-mode-aborted", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let registry_guard = fx.rig.registry_on_this_thread();
    let admission_guard = open_admission();
    let confirmed = crate::services::tui_o::turn_mode::TestConfirmation::new(CHANNEL);
    let intake = Intake::new(&fx, &started);
    let (path, session) = (started.path.clone(), started.session.clone());
    let pasted = Mutex::new(String::new());
    fx.rig.on_send(move |params| {
        if let Some(text) = params["text"].as_str().filter(|text| !text.is_empty()) {
            *pasted.lock().unwrap() = text.trim_start_matches("\u{1b}[200~")
                .trim_end_matches("\u{1b}[201~").into();
        }
        if params["keys"] == json!(["enter"]) {
            append(&path, &[
                json!({"type":"user","uuid":uuid(),"sessionId":session,
                    "timestamp":chrono::Utc::now().to_rfc3339(),
                    "message":{"role":"user","content":pasted.lock().unwrap().clone()}}),
                json!({"type":"user","uuid":uuid(),"sessionId":session,
                    "message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}),
            ]);
        }
    });
    let fallback = FallbackCount::new();
    intake.runtime.block_on(async {
        let app = app(&fx, &intake).await;
        let response = call(&app, "start", "external-aborted-origin").await;
        assert_eq!(response.0, StatusCode::OK, "{}", response.1);
        tokio::time::timeout(LIMIT, async {
            while fx.rig.sends().len() < before + 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        completed(&intake, 0).await;
        let targets = intake.mock.reply_targets();
        assert!(
            !targets
                .iter()
                .copied()
                .any(discord::is_synthetic_headless_message_id_raw),
            "a lifecycle notice cannot reference an input that has no Discord message: {targets:?}"
        );
        assert_eq!(fx.rig.sends().len(), before + 2);
        fallback.assert_none();
    });
    drop(intake);
    drop(confirmed);
    drop(admission_guard);
    drop(registry_guard);
}

#[test]
fn external_herdr_http_machine_unknown_keeps_custody_and_wakes_on_idle_pg() {
    use crate::services::discord::queue_io::input_owner::held_watch::{self, Verdict};
    let fx = Fixture::new("http-machine-unknown", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch_pinless(&fx, &launcher);
    let before = fx.rig.sends().len();
    let registry = fx.rig.registry_on_this_thread();
    let admission = open_admission();
    let intake = Intake::with_expected_pin(&fx, &started, ExpectedPin::Registered);
    native_busy(&started);
    let heads = answer_input(&fx, &intake, &started);
    let fallback = FallbackCount::new();
    intake.runtime.block_on(async {
        let app = app(&fx, &intake).await;
        let drives = super::super::super::OwnerTurn::drive_results(CHANNEL).len();
        let response = call_source(&app, "start", "machine-unknown-origin", "api").await;
        assert_eq!(response.0, StatusCode::OK, "{}", response.1);
        let message = tokio::time::timeout(LIMIT, async {
            loop {
                let held = intake
                    .mailbox
                    .v2_request(|slot| {
                        let owner = slot.as_ref().unwrap();
                        Ok(owner.front().map(|front| front.1.message_id))
                    })
                    .await
                    .unwrap();
                if let Some(message) = held
                    && super::super::super::OwnerTurn::drive_results(CHANNEL).len() > drives
                {
                    break message;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (_, observed) = held_watch::watch_observed(source_retry_drive(&intake), message);
        let look = tokio::time::timeout(LIMIT, observed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(look.verdict, Verdict::Keep, "Unknown is not Idle");
        assert_eq!(fx.rig.sends().len(), before);
        assert!(heads.lock().unwrap().is_empty());
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(false));
        let state = intake
            .mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        assert_eq!(state["items"].as_array().unwrap().len(), 1);
        assert_eq!(state["items"][0]["author_is_bot"], true);
        let front = intake
            .mailbox
            .v2_request(|slot| Ok(slot.as_ref().unwrap().front().unwrap()))
            .await
            .unwrap();
        let context = intake
            .mailbox
            .v2_admission_context(front.1, front.0.unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            context["source"], "api",
            "the owner keeps the external input source"
        );
        assert_eq!(
            super::super::super::OwnerTurn::start_wait_screen_for_test(
                &intake.shared,
                ChannelId::new(CHANNEL),
                &context,
            )
            .await,
            Some(crate::services::claude_tui::busy_inject::Screen::Unknown),
            "an unverified Herdr execution stays Unknown at start observation",
        );
        assert!(state["head_injection"].is_null());
        assert!(state["retained_inputs"].as_array().unwrap().is_empty());
        append(
            &started.path,
            &[json!({"type":"system","subtype":"turn_duration",
            "sessionId":started.session,"timestamp":chrono::Utc::now().to_rfc3339()})],
        );
        // No new API admission or manual owner wake follows the native terminal.
        completed(&intake, 1).await;
        assert_eq!(fx.rig.sends().len(), before + 2);
        let heads = heads.lock().unwrap();
        assert_eq!(heads.len(), 2);
        assert_eq!(
            crate::services::claude_tui::host_input::busy::observed(
                heads[0]["attempt"]["nonce"].as_str().unwrap()
            ),
            0
        );
        fallback.assert_none();
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    });
    drop(intake);
    drop(admission);
    drop(registry);
}

#[test]
fn external_herdr_http_late_writer_hold_retains_the_admitted_original_pg() {
    let fx = Fixture::new("http-late-writer", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let registry_guard = fx.rig.registry_on_this_thread();
    let admission_guard = open_admission();
    let mut intake = Intake::new(&fx, &started);
    let hosts = std::mem::take(&mut intake.hosts);
    intake.runtime.block_on(async {
        let app = app(&fx, &intake).await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let probe = crate::server::routes::DeliveryProbe {
            entered: Some(entered.clone()), release: Some(release.clone()), ..Default::default()
        };
        let delivery = crate::server::routes::DELIVERY_PROBE_FOR_TEST.scope(
            probe, call(&app, "deliver", "late-admitted-origin"),
        );
        let close = async {
            tokio::time::timeout(LIMIT, entered.notified()).await.unwrap();
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM imsg_reply_obligations WHERE origin_id='late-admitted-origin'")
                .fetch_one(&fx.pool).await.unwrap();
            assert_eq!(count, 1, "the barrier is after origin admission");
            assert_eq!(fx.rig.sends().len(), before);
            for host in hosts {
                tokio::time::timeout(LIMIT, host.stop_and_join()).await.unwrap().unwrap();
            }
            assert!(!host::channel_accepts(CHANNEL));
            release.notify_one();
        };
        let (response, ()) = tokio::join!(delivery, close);
        assert_eq!(response.0, StatusCode::OK, "{}", response.1);
        assert_eq!(response.1["delivery"], "queued");
        let state = intake.mailbox.v2_request(|slot| slot.as_ref().unwrap().json()).await.unwrap();
        let queued = state["items"].as_array().unwrap().iter().any(|item| item["text"] == INPUT);
        let retained = state["retained_inputs"].as_array().unwrap().iter()
            .any(|item| item["input"]["original"]["text"] == INPUT);
        assert!(queued || retained, "the writer hold keeps the admitted original: {state}");
        assert!(state.to_string().contains("late-admitted-origin"));
        let registry = HealthRegistry::new();
        registry.register("claude".into(), intake.shared.clone()).await;
        if queued {
            let visible = discord::health::snapshot_pending_queue_state(
                &registry, "claude", ChannelId::new(CHANNEL),
            ).await.unwrap();
            assert_eq!(visible.queue_depth, 1, "the public queue snapshot includes the held original");
        } else {
            discord::queue_io::input_owner::notice::drain_synchronously_for_tests(
                &fx.pool, &intake.mailbox, "claude", CHANNEL,
            ).await.unwrap();
            let notices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE source='retained_input_notice' AND target=$1")
                .bind(format!("channel:{CHANNEL}")).fetch_one(&fx.pool).await.unwrap();
            assert_eq!(notices, 1);
        }
        assert_eq!(fx.rig.sends().len(), before);
        let replay = call(&app, "deliver", "late-admitted-origin").await;
        assert_eq!(replay, response);
        assert_eq!(fx.rig.sends().len(), before);
    });
    drop(intake);
    drop(admission_guard);
    drop(registry_guard);
}

#[derive(Default)]
pub(super) struct OutputGate {
    held: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl OutputGate {
    fn open(&self) {
        self.held.store(false, Ordering::SeqCst);
        self.release.notify_one();
    }
}

struct OutputGateRelease(Arc<OutputGate>);
impl Drop for OutputGateRelease {
    fn drop(&mut self) {
        self.0.open();
    }
}

struct GatedPosts {
    base: Arc<Posts>,
    gate: Arc<OutputGate>,
}

impl crate::services::tui_o::writer::DiscordPort for GatedPosts {
    fn bot_id(&self) -> u64 {
        self.base.bot_id()
    }
    fn post(
        &self,
        channel: u64,
        content: String,
    ) -> impl std::future::Future<Output = crate::services::tui_o::writer::PostOutcome> + Send + 'static
    {
        let (base, gate) = (self.base.clone(), self.gate.clone());
        async move {
            while gate.held.load(Ordering::SeqCst) && content.contains("HTTP session answered") {
                let release = gate.release.notified();
                gate.entered.notify_one();
                if gate.held.load(Ordering::SeqCst) {
                    release.await;
                }
            }
            base.post(channel, content).await
        }
    }
    fn history_after(
        &self,
        channel: u64,
        after: u64,
    ) -> impl std::future::Future<
        Output = Result<Vec<crate::services::tui_o::writer::SeenMessage>, String>,
    > + Send {
        self.base.history_after(channel, after)
    }
    fn message(
        &self,
        channel: u64,
        id: u64,
    ) -> impl std::future::Future<
        Output = Result<Option<crate::services::tui_o::writer::SeenMessage>, String>,
    > + Send {
        self.base.message(channel, id)
    }
}

struct OutputHost {
    base: Arc<SourceHost>,
    port: Arc<GatedPosts>,
}

impl host::HostIo for OutputHost {
    type Port = GatedPosts;
    type Lease = <SourceHost as host::HostIo>::Lease;
    type Alarms = <SourceHost as host::HostIo>::Alarms;
    type Bindings = <SourceHost as host::HostIo>::Bindings;
    fn port(&self) -> impl std::future::Future<Output = Arc<GatedPosts>> + Send {
        std::future::ready(self.port.clone())
    }
    fn lease(&self) -> Self::Lease {
        self.base.lease()
    }
    fn alarms(&self) -> Self::Alarms {
        self.base.alarms()
    }
    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<Self::Bindings> {
        self.base.bindings(channel, provider)
    }
    fn native_inputs(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> Option<crate::services::tui_o::writer::actor::native_input::NativePort> {
        self.base.native_inputs(channel, provider)
    }
    fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl std::future::Future<
        Output = Result<crate::services::tui_o::writer::activation::ActivationFacts, String>,
    > + Send {
        self.base.activation_facts(channel, provider)
    }
    fn intake_fence(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl std::future::Future<Output = Result<host::FencedFacts, String>> + Send {
        self.base.intake_fence(channel, provider)
    }
    fn local_custody(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> Result<crate::services::tui_o::writer::host::Custody, String> {
        self.base.local_custody(channel, provider)
    }
    fn legacy(&self) -> Arc<dyn LegacyView> {
        self.base.legacy()
    }
    fn legacy_busy(&self, channel: u64) -> impl std::future::Future<Output = bool> + Send {
        self.base.legacy_busy(channel)
    }
    fn relaying(&self, channel: u64) -> bool {
        self.base.relaying(channel)
    }
}

pub(super) fn start_writer(
    parts: HostParts<SourceHost>,
    output_gate: Arc<OutputGate>,
) -> Vec<ManagedWriterHandle> {
    let port = Arc::new(GatedPosts {
        base: parts.io.io.posts.clone(),
        gate: output_gate,
    });
    let parts = HostParts {
        io: Arc::new(OutputHost {
            base: parts.io,
            port,
        }),
        runtime_root: parts.runtime_root,
        gate: parts.gate,
        readiness: parts.readiness,
    };
    host::start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership()
            .into_iter()
            .filter(|(channel, _, _)| *channel == CHANNEL)
            .collect(),
        || parts,
    )
}

#[test]
fn external_herdr_http_reply_before_and_after_terminal_pg() {
    for after in [false, true] {
        let fx = Fixture::new(
            if after {
                "http-body-after"
            } else {
                "http-body-before"
            },
            None,
        );
        let launcher = Arc::new(Launcher::default());
        let (_, started) = reconnect::launch(&fx, &launcher);
        assert_eq!(
            started.hook(
                &fx,
                "Stop",
                &json!({
                    "session_id":started.session,"transcript_path":started.path,
                })
            ),
            202
        );
        let before = fx.rig.sends().len();
        let registry = fx.rig.registry_on_this_thread();
        let admission = open_admission();
        let gate = Arc::new(OutputGate::default());
        gate.held.store(after, Ordering::SeqCst);
        let intake = Intake::with_output_gate(&fx, &started, ExpectedPin::Verified, gate.clone());
        let gate_release = OutputGateRelease(gate.clone());
        let heads = answer_on_enter(&fx, &intake, &started, true);
        let fallback = FallbackCount::new();
        intake.runtime.block_on(async {
            let app = app(&fx, &intake).await;
            let response = call(&app, "start", "ordered-body-origin").await;
            assert_eq!(response.0, StatusCode::OK, "{}", response.1);
            tokio::time::timeout(LIMIT, async {
                while heads.lock().unwrap().len() != 2
                    || !crate::services::tui_turn_state::observe_claude_jsonl_turn_state(
                        &started.path,
                    )
                    .is_busy()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if after {
                finish_answer(&started.path, &started.session, 1);
                let outcome = tokio::time::timeout(LIMIT, async {
                    gate.entered.notified().await;
                    loop {
                        let row =
                            inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL);
                        if row
                            .as_ref()
                            .is_none_or(|row| row.tui_terminal_kind.is_some())
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                    assert!(
                        intake.posts.to(CHANNEL).is_empty(),
                        "the O body post is still held"
                    );
                })
                .await;
                gate.open();
                outcome.unwrap();
            } else {
                let (entered, paused) = std::sync::mpsc::channel();
                let (release, wait) = std::sync::mpsc::channel();
                let hook = discord::gateway::HeadlessExactGateway::before_next_presentation(
                    Box::new(move || {
                        entered.send(()).unwrap_or(());
                        wait.recv_timeout(LIMIT).unwrap_or(());
                    }),
                );
                finish_answer(&started.path, &started.session, 1);
                paused.recv_timeout(LIMIT).unwrap();
                let outcome = tokio::time::timeout(LIMIT, async {
                    while !intake
                        .posts
                        .to(CHANNEL)
                        .iter()
                        .any(|text| text.contains("HTTP session answered"))
                    {
                        tokio::task::yield_now().await;
                    }
                    if let Some(row) = inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL) {
                        assert!(row.tui_terminal_kind.is_none(), "O publishes before terminal admission");
                    } else {
                        let history = crate::services::tui_input::human_frame::history::v2::Registry::runtime().unwrap();
                        let registration = history.native_classification_registrations().unwrap().into_iter()
                            .find(|r| r.ticket.channel_id == CHANNEL && r.ticket.owning_episode.is_some()).unwrap();
                        let saved = history.native_settlement(&registration.namespace, &registration.ticket).unwrap();
                        assert!(saved.cleanup_complete, "native cleanup must account for the missing row: {saved:?}");
                        assert_eq!(saved.outcome.unwrap().disposition,
                            crate::services::tui_input::human_frame::history::v2::native::Disposition::Executed);
                    }
                })
                .await;
                release.send(()).unwrap_or(());
                drop(hook);
                outcome.unwrap();
            }
            completed(&intake, 1).await;
            assert_eq!(fx.rig.sends().len(), before + 2);
            fallback.assert_none();
        });
        drop(gate_release);
        drop(intake);
        drop(admission);
        drop(registry);
    }
}
