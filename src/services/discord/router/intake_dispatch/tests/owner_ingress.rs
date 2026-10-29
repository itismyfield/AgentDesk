//! The live intake entry under an installed input owner: arrivals join the next unit at once, a
//! hold queues them with one notice each, and Pending stays its own typed case.
use super::*;
use crate::services::discord::queue_io::input_ingress::{self, Hold, Intake, OwnerPending};
use crate::services::discord::queue_io::input_admission::{self, TurnBind};
use crate::services::discord::router::input_payload::member_body;
use crate::services::discord::shared_state::test_rest;
use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
use crate::services::turn_orchestrator::{ChannelMailboxHandle, PendingQueueItem};
use serde_json::{Value, json};
use std::sync::atomic::Ordering::Relaxed;


/// A booted process has a settled queue generation; a fresh test root has none.
fn seed_generation() {
    let path = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "7").unwrap();
}

type Bodies = Arc<std::sync::Mutex<Vec<Value>>>;

/// A REST stand-in on this thread that keeps every created message body; tests read the queue
/// notices among them.
async fn notices(channel: u64) -> (Bodies, test_rest::Guard) {
    use axum::{Json, Router, body::Bytes};
    let bodies = Bodies::default();
    let kept = bodies.clone();
    let app = Router::new().fallback(move |body: Bytes| {
        let kept = kept.clone();
        async move {
            kept.lock().unwrap().push(serde_json::from_slice(&body).unwrap_or(Value::Null));
            Json(json!({
                "id": "1", "channel_id": channel.to_string(), "content": "",
                "author": {"id":"1", "username":"bot", "discriminator":"0001", "avatar":null},
                "timestamp":"2026-10-01T00:00:00+00:00", "edited_timestamp":null,
                "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[],
                "attachments":[], "embeds":[], "pinned":false, "type":0
            }))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (bodies, test_rest::install(http))
}

fn said(bodies: &Bodies) -> Vec<(String, u64)> {
    let bodies = bodies.lock().unwrap();
    let message = |body: &Value| body["message_reference"]["message_id"].as_str().and_then(|id| id.parse().ok());
    let notice = |text: &str| text.ends_with("즉시주입 하지 못하고 메시지큐에 들어갔습니다");
    bodies.iter().map(|b| (b["content"].as_str().unwrap_or_default().to_string(), message(b).unwrap_or(0)))
        .filter(|(text, _)| notice(text)).collect()
}

async fn adopt(shared: &Arc<crate::services::discord::SharedData>, channel: ChannelId) -> (ChannelMailboxHandle, std::path::PathBuf) {
    let path = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap()
        .join("claude").join(&shared.token_hash).join(format!("{channel}.json"));
    let handle = shared.mailbox(channel);
    handle.v2_adopt(path.clone(), shared.token_hash.clone()).await.unwrap();
    (handle, path)
}

fn live(channel: ChannelId, message: u64, text: &str) -> IntakeSubmission {
    IntakeSubmission {
        provider: ProviderKind::Claude,
        request: request(channel, message, text),
        origin: IntakeOrigin::LiveMessage,
        preserve_on_cancel: true,
        has_nonportable_uploads: false,
        attachments: Vec::new(),
        preloaded_uploads: Vec::new(),
        voice_announcement: None,
    }
}

async fn items(handle: &ChannelMailboxHandle) -> Vec<PendingQueueItem> {
    handle.v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.snapshot.items.clone())).await.unwrap()
}

async fn head_members(handle: &ChannelMailboxHandle) -> Vec<u64> {
    handle.v2_request(|slot| {
        let head = slot.as_ref().ok_or("no owner")?.snapshot.head_injection.as_ref();
        Ok(head.map(|head| head.members().map(|m| m.message_id).collect()).unwrap_or_default())
    }).await.unwrap()
}

/// Stands in for the pump and the leaf: reserves everything queued as one unit and returns the
/// exact frame bytes a writer would type.
async fn reserve(handle: &ChannelMailboxHandle, nonce: &str) -> (Value, String) {
    let attempt = json!({"nonce":nonce,"provider":"claude",
        "target":{"host":"tmux","owner":format!("owner-ingress-{nonce}"),"root":"/test","execution_nonce":"e"},
        "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
    handle.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let attempt = serde_json::from_value(attempt).map_err(|e| e.to_string())?;
        let key = owner.reserve_unit(attempt, |item| Some((format!("user{}", item.author_id), member_body(item, &[]))))?;
        let head = owner.snapshot.head_injection.as_ref().ok_or("no head")?;
        let frame = head.payload.as_ref().ok_or("no payload")?.frame.clone();
        Ok((serde_json::to_value(key).map_err(|e| e.to_string())?, frame))
    }).await.unwrap()
}

/// Rebuilds a unit whose write permit is not handed over yet over everything queued now.
async fn reprepare(handle: &ChannelMailboxHandle, key: &Value, nonce: &str) -> (Value, String) {
    let (key, nonce) = (key.clone(), nonce.to_string());
    handle.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let key = serde_json::from_value(key).map_err(|e| e.to_string())?;
        let next = owner.reprepare(&key, nonce, |item| Some((format!("user{}", item.author_id), member_body(item, &[]))))?;
        let head = owner.snapshot.head_injection.as_ref().ok_or("no head")?;
        let frame = head.payload.as_ref().ok_or("no payload")?.frame.clone();
        Ok((serde_json::to_value(next).map_err(|e| e.to_string())?, frame))
    }).await.unwrap()
}

/// Arms the unit and opens its payload window, as a writer does before its first byte.
async fn write(handle: &ChannelMailboxHandle, key: &Value) {
    let key = key.clone();
    handle.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let key = serde_json::from_value(key).map_err(|e| e.to_string())?;
        drop(owner.arm(&key, 1)?);
        owner.payload_started(&key, 2)
    }).await.unwrap()
}

async fn submitted(handle: &ChannelMailboxHandle, key: &Value) {
    let key = key.clone();
    handle.v2_request(move |slot| {
        let key = serde_json::from_value(key).map_err(|e| e.to_string())?;
        slot.as_mut().ok_or("no owner")?.settle(&key, true, 3)
    }).await.unwrap()
}

async fn turn(handle: &ChannelMailboxHandle, channel: ChannelId, item: &PendingQueueItem) -> TurnBind {
    let ticket = input_ingress::discord_ticket(&ProviderKind::Claude, channel, item.message_id, &item.source_message_ids);
    input_admission::exact_turn(handle, &ticket, item).await.unwrap()
}

/// (1)+(4): two arrivals held by restart recovery are queued with one notice each; the next
/// arrival needs no notice and all three go in one frame, in order, and the queue empties.
#[tokio::test(flavor = "current_thread")]
async fn held_queue_and_the_new_arrival_inject_together_in_order() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    seed_generation();
    let channel = ChannelId::new(7_272_401);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let (handle, _) = adopt(&shared, channel).await;
    let (bodies, _rest) = notices(channel.get()).await;
    shared.restart.reconcile_done.store(false, Relaxed);
    for (message, text) in [(7_272_401_001, "queued one"), (7_272_401_002, "queued two")] {
        let mut submission = live(channel, message, text);
        submission.request.reply_context = (message == 7_272_401_002).then(|| "quoted line".to_string());
        dispatch_text_intake(&deps, submission).await.unwrap();
    }
    let reason = Hold::RestartRecovery.notice();
    assert_eq!(said(&bodies), [(reason.clone(), 7_272_401_001), (reason, 7_272_401_002)]);
    shared.restart.reconcile_done.store(true, Relaxed);
    dispatch_text_intake(&deps, live(channel, 7_272_401_003, "new arrival")).await.unwrap();
    assert_eq!(said(&bodies).len(), 2, "an immediate arrival leaves no notice");
    let queued = items(&handle).await;
    let (key, frame) = reserve(&handle, "together").await;
    let at = |text: &str| frame.find(text).unwrap_or_else(|| panic!("{text} missing from {frame:?}"));
    assert!(at("queued one") < at("queued two") && at("queued two") < at("new arrival"));
    assert_eq!(frame.matches("new arrival").count(), 1);
    assert!(frame.contains("quoted line\n\nqueued two") && !frame.contains('\u{1e}'), "a reply context is rendered, never its envelope: {frame:?}");
    write(&handle, &key).await;
    submitted(&handle, &key).await;
    assert!(items(&handle).await.is_empty(), "the queue is empty after one injection");
    for item in &queued {
        assert!(matches!(turn(&handle, channel, item).await, TurnBind::Settled(t) if t.attempt == "together"));
    }
}

/// (2) before the hand-over: an arrival while the unit still registers joins it, in order, in
/// one frame; the earlier key never arms, and all three tickets settle under one attempt.
#[tokio::test(flavor = "current_thread")]
async fn an_arrival_while_the_unit_registers_joins_the_same_frame() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    seed_generation();
    let channel = ChannelId::new(7_272_407);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let (handle, _) = adopt(&shared, channel).await;
    let (bodies, _rest) = notices(channel.get()).await;
    for (message, text) in [(7_272_407_001, "q1"), (7_272_407_002, "q2")] {
        dispatch_text_intake(&deps, live(channel, message, text)).await.unwrap();
    }
    let (old, _) = reserve(&handle, "registering").await;
    dispatch_text_intake(&deps, live(channel, 7_272_407_003, "n while registering")).await.unwrap();
    let (key, frame) = reprepare(&handle, &old, "handed-over").await;
    assert_eq!(frame, "q1\n\nq2\n\nn while registering");
    let stale = old.clone();
    assert!(handle.v2_request(move |slot| {
        let stale = serde_json::from_value(stale).map_err(|e| e.to_string())?;
        slot.as_mut().ok_or("no owner")?.arm(&stale, 1).map(drop)
    }).await.is_err(), "the earlier preparation never writes");
    let queued = items(&handle).await;
    write(&handle, &key).await;
    submitted(&handle, &key).await;
    assert!(items(&handle).await.is_empty());
    for item in &queued {
        assert!(matches!(turn(&handle, channel, item).await, TurnBind::Settled(t) if t.attempt == "handed-over"));
    }
    assert!(said(&bodies).is_empty());
}

/// (2): an arrival while the previous unit is being written is accepted without a notice, does
/// not join the frozen unit, and is the whole next unit right after the local submit.
#[tokio::test(flavor = "current_thread")]
async fn an_arrival_during_registration_is_the_next_unit_not_a_second_writer() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    seed_generation();
    let channel = ChannelId::new(7_272_402);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let (handle, _) = adopt(&shared, channel).await;
    let (bodies, _rest) = notices(channel.get()).await;
    dispatch_text_intake(&deps, live(channel, 7_272_402_001, "first input")).await.unwrap();
    let (first, frame) = reserve(&handle, "in-flight").await;
    write(&handle, &first).await;
    assert!(super::super::super::intake_gate::busy_inject::owner_takes(&shared, channel).await,
        "the legacy offer never registers it beside the owner");
    dispatch_text_intake(&deps, live(channel, 7_272_402_002, "while registering")).await.unwrap();
    assert_eq!(head_members(&handle).await, [7_272_402_001], "the reserved unit is not extended");
    assert!(!frame.contains("while registering"));
    submitted(&handle, &first).await;
    let (_, next) = reserve(&handle, "next").await;
    assert!(next.contains("while registering") && !next.contains("first input"));
    assert!(said(&bodies).is_empty(), "joining the next unit is not a hold");
}

/// (3): a Herdr-hosted channel is taken by the owner, which the legacy offer still skips.
#[tokio::test(flavor = "current_thread")]
async fn a_herdr_channel_is_injected_through_the_owner() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    seed_generation();
    let channel = ChannelId::new(7_272_403);
    let _hosts = crate::config::session_hosts::force_for_test(Some("node"), &[(channel.get(), "node")]);
    let _gate = super::super::super::intake_gate::busy_inject::test_support::open_gate(channel.get());
    assert!(crate::services::herdr_launch::herdr_configured_for_tui_launch(Some(channel.get())));
    assert!(!super::super::super::intake_gate::busy_inject::covers(channel, None));
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let (handle, _) = adopt(&shared, channel).await;
    assert!(super::super::super::intake_gate::busy_inject::owner_takes(&shared, channel).await);
    dispatch_text_intake(&deps, live(channel, 7_272_403_001, "to the herdr pane")).await.unwrap();
    let (key, frame) = reserve(&handle, "herdr").await;
    assert_eq!(frame, "to the herdr pane", "a one-member body is the single input's prompt, byte for byte");
    write(&handle, &key).await;
    submitted(&handle, &key).await;
    assert!(items(&handle).await.is_empty());
}

/// (4): each hold notices once per original, in readable words; Pending never notices.
#[tokio::test(flavor = "current_thread")]
async fn every_hold_notices_once_and_pending_or_attempted_never() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    seed_generation();
    let channel = ChannelId::new(7_272_404);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let (handle, path) = adopt(&shared, channel).await;
    let (bodies, _rest) = notices(channel.get()).await;
    dispatch_text_intake(&deps, live(channel, 7_272_404_001, "held input")).await.unwrap();
    let item = items(&handle).await.remove(0);
    let ticket = input_ingress::discord_ticket(&ProviderKind::Claude, channel, item.message_id, &item.source_message_ids);
    let original = input_ingress::Original::with_ticket(item, ticket);
    for hold in Hold::ALL {
        assert!(input_ingress::held(&shared, &handle, channel, &original, hold).await);
        assert!(!input_ingress::held(&shared, &handle, channel, &original, hold).await, "{hold:?} twice");
    }
    let texts: Vec<String> = said(&bodies).into_iter().map(|(text, at)| { assert_eq!(at, 7_272_404_001); text }).collect();
    assert_eq!(texts, Hold::ALL.map(Hold::notice));
    assert!(texts.iter().all(|t| t.starts_with("-# ") && t.ends_with("로 즉시주입 하지 못하고 메시지큐에 들어갔습니다")));
    let (key, _) = reserve(&handle, "attempted").await;
    write(&handle, &key).await;
    let fresh = input_ingress::Original::with_ticket(original.item.clone(), input_admission::AdmissionTicket::stored("other"));
    assert!(!input_ingress::held(&shared, &handle, channel, &fresh, Hold::Modal).await, "an unknown ticket");
    submitted(&handle, &key).await;
    shared.restart.reconcile_done.store(false, Relaxed);
    let watch = Watch::new(&path, Plan { fail: vec![Step::ParentSync], ..Default::default() });
    let pending = dispatch_text_intake(&deps, live(channel, 7_272_404_002, "pending input")).await;
    drop(watch);
    let error = pending.expect_err("Pending is not an execution");
    let pending = error.downcast_ref::<OwnerPending>().expect("Pending stays typed");
    assert!(pending.to_string().contains(input_ingress::PENDING_CONTRACT));
    let raw = crate::services::discord::router::input_payload::owned_original(
        &request(channel, 7_272_404_002, "pending input"), false, Vec::new(), None, false,
    );
    let unconfirmed = input_ingress::original(&ProviderKind::Claude, channel, None, &raw).unwrap();
    assert!(!input_ingress::held(&shared, &handle, channel, &unconfirmed, Hold::Modal).await, "held never notices Pending");
    assert_eq!(said(&bodies).len(), Hold::ALL.len(), "Pending leaves no queue notice");
}

/// Pending keeps the original and its ticket: the worker holds its row, and the re-entry of the
/// same message after the owner recovers is accepted once under the same ticket.
#[tokio::test(flavor = "current_thread")]
async fn pending_is_held_by_the_worker_and_the_same_ticket_resumes() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    seed_generation();
    let channel = ChannelId::new(7_272_405);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let (handle, path) = adopt(&shared, channel).await;
    let request = request(channel, 7_272_405_001, "worker raw");
    let watch = Watch::new(&path, Plan { fail: vec![Step::ParentSync], ..Default::default() });
    let held = crate::services::discord::router::execute_intake_turn_core(
        &http, &shared, "token", request.clone(), Vec::new(),
    ).await;
    drop(watch);
    assert!(matches!(held, Ok(crate::services::discord::router::IntakeExecution::Deferred)), "{held:?}");
    let ticket = input_ingress::discord_ticket(&ProviderKind::Claude, channel, 7_272_405_001, &[7_272_405_001]);
    assert_ne!(input_admission::status(&handle, &ticket).await, Ok(input_admission::AdmissionState::Absent));
    handle.v2_adopt(path, shared.token_hash.clone()).await.unwrap();
    let resumed = crate::services::discord::router::execute_intake_turn_core(
        &http, &shared, "token", request, Vec::new(),
    ).await;
    assert!(matches!(resumed, Ok(crate::services::discord::router::IntakeExecution::Executed)), "{resumed:?}");
    let queued = items(&handle).await;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].text, "worker raw");
    assert_eq!(turn(&handle, channel, &queued[0]).await, TurnBind::NotArmed);
    assert_eq!(Intake::Pending(OwnerPending(ticket.clone())).worker().ok(), Some(crate::services::discord::router::IntakeExecution::Deferred));
}

/// A live arrival moves its checkpoint only once the owner accepted it; Pending leaves it for
/// catch-up, which re-enters the same message under the same ticket.
#[tokio::test(flavor = "current_thread")]
async fn live_pending_keeps_the_checkpoint() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    seed_generation();
    let channel = ChannelId::new(7_272_406);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let (handle, path) = adopt(&shared, channel).await;
    let moved = std::sync::atomic::AtomicUsize::new(0);
    let watch = Watch::new(&path, Plan { fail: vec![Step::ParentSync], ..Default::default() });
    let pending = super::super::dispatch_live_text_intake(&deps, live(channel, 7_272_406_001, "live raw"), || {
        moved.fetch_add(1, Relaxed);
    }).await;
    drop(watch);
    assert!(pending.is_err_and(|e| e.downcast_ref::<OwnerPending>().is_some()));
    assert_eq!(moved.load(Relaxed), 0, "Pending keeps the checkpoint");
    handle.v2_adopt(path, shared.token_hash.clone()).await.unwrap();
    super::super::dispatch_live_text_intake(&deps, live(channel, 7_272_406_001, "live raw"), || {
        moved.fetch_add(1, Relaxed);
    }).await.unwrap();
    assert_eq!(moved.load(Relaxed), 1);
    assert_eq!(items(&handle).await.len(), 1, "the re-entry is admitted once");
}
