//! External HTTP admission over the existing bound Herdr execution and real O writer.
use super::*;
use crate::services::discord::health::HealthRegistry;
use crate::services::tui_o::writer::DiscordPort;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;

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
    let body = match endpoint {
        "start" => json!({"prompt":INPUT,"source":source,"origin_id":origin}),
        "deliver" => json!({"text":INPUT,"source":source,"origin_id":origin,
            "author_discord_user_id":"7"}),
        _ => panic!("unknown endpoint"),
    };
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
    let now = chrono::Utc::now().to_rfc3339();
    append(
        path,
        &[
            json!({"type":"assistant","uuid":uuid(),"sessionId":session,"timestamp":now,
            "apiBlockIndex":0,"message":{"id":uuid(),"role":"assistant",
            "content":[{"type":"text","text":format!("HTTP session answered {n}")}],"stop_reason":"end_turn"}}),
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
        panic!(
            "{error}: row={:?}, O={:?}, bridge={:?}, state={state}, activity={activity:?}, drives={:?}, {}",
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
    let fx = Fixture::new(&format!("http-{endpoint}"), None);
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
    drop(intake);
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
                    idle_turn_unwound(&intake).await;
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

fn execution_gone(terminal: Option<bool>) {
    let tag = match terminal {
        None => "missing",
        Some(false) => "raw",
        Some(true) => "flushed",
    };
    let fx = Fixture::new(&format!("http-execution-gone-{tag}"), None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let registry_guard = fx.rig.registry_on_this_thread();
    let admission_guard = open_admission();
    let intake = Intake::new(&fx, &started);
    let (path, session) = (started.path.clone(), started.session.clone());
    fx.rig.on_send(move |params| {
        if params["keys"] == json!(["enter"]) {
            append(
                &path,
                &[json!({"type":"user","uuid":uuid(),"sessionId":session,
                "timestamp":chrono::Utc::now().to_rfc3339(),
                "message":{"role":"user","content":INPUT}})],
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
            let once = Arc::new(std::sync::atomic::AtomicBool::new(false));
            discord::recovery_engine::herdr_row_end::tests::on_while("live_after_end", move || {
                if !once.swap(true, Ordering::SeqCst) {
                    let now = chrono::Utc::now().to_rfc3339();
                    append(&path, &[json!({"type":"assistant","uuid":uuid(),"sessionId":session,
                        "timestamp":now,"apiBlockIndex":0,"message":{"id":uuid(),"role":"assistant",
                        "content":[{"type":"text","text":"HTTP session answered after E"}],"stop_reason":"end_turn"}})]);
                    let record = json!({"type":"system","subtype":"turn_duration","sessionId":session,"timestamp":now}).to_string();
                    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
                    use std::io::Write;
                    file.write_all(record.as_bytes()).unwrap();
                    if newline { file.write_all(b"\n").unwrap(); }
                }
                Box::pin(async {})
            })
        });
        super::super::live_end::kill(&fx);
        discord::recovery_engine::herdr_row_end::row_end_pass(&intake.shared, &ProviderKind::Claude).await;
        completed(&intake, usize::from(terminal.is_some())).await;
        for trial in 0..2 {
            discord::recovery_engine::herdr_row_end::row_end_pass(&intake.shared, &ProviderKind::Claude).await;
            tokio::time::sleep(Duration::from_millis(10 + trial)).await;
        }
        let notice = super::super::live_end::LiveEnd::notice();
        let notices = intake.mock.written_texts().into_iter().filter(|text| text == &notice).count();
        assert_eq!(notices, usize::from(terminal.is_none()), "a native terminal is left to its reader");
        if terminal.is_none() {
            assert!(actor.is_completion_cleanup());
            assert!(actor.cancelled.load(Ordering::SeqCst), "the settled reader is woken");
        }
        assert_eq!(fx.rig.sends().len(), before + 2);
        fallback.assert_none();
        drop(flush);
    });
    if terminal.is_none() {
        assert_eq!(fx.row(), Some(HostedState::Retired));
    }
    drop(intake);
    drop(admission_guard);
    drop(registry_guard);
}

#[test]
fn external_herdr_http_execution_gone_without_terminal_settles_once_pg() {
    execution_gone(None);
}

#[test]
fn external_herdr_http_execution_gone_yields_a_flushed_terminal_pg() {
    for newline in [false, true] {
        execution_gone(Some(newline));
    }
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
    fx.rig.on_send(move |params| {
        if params["keys"] == json!(["enter"]) {
            append(&path, &[
                json!({"type":"user","uuid":uuid(),"sessionId":session,
                    "timestamp":chrono::Utc::now().to_rfc3339(),
                    "message":{"role":"user","content":INPUT}}),
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
    let (_, started) = reconnect::launch(&fx, &launcher);
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
                if let Some(message) = held {
                    if super::super::super::OwnerTurn::drive_results(CHANNEL).len() > drives {
                        break message;
                    }
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
        self.release.notify_waiters();
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
    base: Arc<TestHost>,
    port: Arc<GatedPosts>,
}

impl host::HostIo for OutputHost {
    type Port = GatedPosts;
    type Lease = <TestHost as host::HostIo>::Lease;
    type Alarms = <TestHost as host::HostIo>::Alarms;
    type Bindings = <TestHost as host::HostIo>::Bindings;
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
    parts: HostParts<TestHost>,
    output_gate: Arc<OutputGate>,
) -> Vec<ManagedWriterHandle> {
    let port = Arc::new(GatedPosts {
        base: parts.io.posts.clone(),
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
        cutover::boot_ownership(),
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
                let logical = fx.logical().to_owned();
                let (entered, locked) = std::sync::mpsc::channel();
                let (release, wait) = std::sync::mpsc::channel();
                let lock = std::thread::spawn(move || {
                    crate::services::tmux_common::with_tmux_source_authority(&logical, |_| {
                        entered.send(()).unwrap();
                        wait.recv_timeout(LIMIT).unwrap_or(());
                    });
                });
                locked.recv_timeout(LIMIT).unwrap();
                finish_answer(&started.path, &started.session, 1);
                let outcome = tokio::time::timeout(LIMIT, async {
                    while !intake
                        .posts
                        .to(CHANNEL)
                        .iter()
                        .any(|text| text.contains("HTTP session answered"))
                    {
                        tokio::task::yield_now().await;
                    }
                    let row =
                        inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL)
                            .unwrap();
                    assert!(
                        row.tui_terminal_kind.is_none(),
                        "O publishes before terminal admission"
                    );
                })
                .await;
                release.send(()).unwrap_or(());
                lock.join().unwrap();
                outcome.unwrap();
            }
            completed(&intake, 1).await;
            assert_eq!(fx.rig.sends().len(), before + 2);
            fallback.assert_none();
        });
        gate.open();
        drop(intake);
        drop(admission);
        drop(registry);
    }
}
