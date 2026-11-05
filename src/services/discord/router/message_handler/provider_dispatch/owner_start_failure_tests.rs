//! Fixed classification at owner settlement:
//! | 결과 | 분류 |
//! | reserve commit 이후의 모든 결과 | EffectOwned |
//! | drive handoff 대기(owner_input.rs:167–193), claim 실패(:514–523), owner_front_not_admitted 전체, headless claim 실패(headless_turn.rs:214–224), 비allowlist HerdrRefusal, wrapper 안의 NotSent 결과 | SchedulingHeld — 횟수 미포함, 슬롯 미소비. 오늘 동작 그대로 |
//! | allowlist 4종(host_refusal.rs:28–37에서 typed 보존) | ConfirmedKind, 첫 실패에서 보관 |
//! | wrapper 생성 뒤 reserve 전의 비NotSent Err(Herdr gate 미확인, context I/O, tmux 생성 실패 등), panic, (Err(_), Ok(_)) 무응답 | StartFailedBeforeInput — 2회 예산 |
//! | Forwarded / ConsumedToHold / SkippedDuplicate | 다른 경로가 이미 소비했을 수 있으므로 KnownNotSent 보관 금지 |
//! Allowlist: ProviderUnsupported, EndpointNotLocal, actual ExecutorNotWired after switch-on,
//! HostedHerdrUnconfigured { state: Some(Pending | Bound) } after automatic Return.
//! Preclaim NotSent("herdr_root_recovery_held") is SchedulingHeld; S2 owns its completion wake.
//! Inside-start Herdr bound-gate failure is StartFailedBeforeInput regardless of its string.
//! The S2 case below emulates only its NotSent frame, not its unlanded preclaim implementation.

use super::*;
use crate::services::discord::queue_io::cancel_backstop_test_support as counted;
use crate::services::discord::queue_io::input_ingress;
use crate::services::discord::router::message_handler::provider_dispatch as dispatch;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

const BUDGET: Duration = Duration::from_secs(30);
const A: u64 = 7_787_001;
const B: u64 = 7_787_002;

#[derive(Debug)]
enum Signal {
    Wake,
    ThirdStart,
    Payload(u64, String),
    Closed(u64),
}

struct WakeScope(ChannelId);
impl Drop for WakeScope {
    fn drop(&mut self) {
        counted::set_owner_wake_for_tests(self.0, None);
        *dispatch::CLAUDE_LEGACY_PROBE.lock().unwrap() = None;
    }
}

fn request(channel: ChannelId, message: u64) -> crate::services::discord::IntakeRequest {
    crate::services::discord::IntakeRequest {
        intake_outbox_id: None,
        channel_id: channel,
        user_msg_id: MessageId::new(message),
        source_message_ids: vec![MessageId::new(message)],
        busy_followup_retry_user_msg_id: MessageId::new(message),
        request_owner: UserId::new(message),
        request_owner_name: format!("author-{message}"),
        user_text: format!("입력 {} — 원문 보존", if message == A { "A" } else { "B" }),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: false,
        reply_context: None,
        has_reply_boundary: true,
        dm_hint: Some(false),
        turn_kind: crate::services::discord::TurnKind::Foreground,
        preserve_on_cancel: true,
    }
}

/// `live` enables the acceptance oracle when the production policy lands. Both modes use the
/// same ingress, owner drive, intake, producer, bridge and completion listener.
async fn fixture(failure: &'static str, held: bool, live: bool) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().unwrap();
    let _tmux = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux(&root);
    fs::write(
        root.join("tmux"),
        "#!/bin/sh\ncase \"$*\" in *pane_dead*) echo 1;; *has-session*) echo \"can't find session: fixture\" >&2; exit 1;; esac\n",
    )
    .unwrap();
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    fs::create_dir_all(generation.parent().unwrap()).unwrap();
    fs::write(generation, "1").unwrap();
    let boot = serde_json::from_value(json!({"server":{},"agents":[]})).unwrap();
    crate::services::tui_o::channel_policy::install(&boot).unwrap();
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:9".into(),
    );
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
    crate::services::discord::internal_api::init(api.port, None);
    let mock = discord_mock::DiscordMockState::new();
    let (proxy, gateway, server) = discord_mock::start(mock).await;
    let mut ctx = discord_mock::serenity_context(proxy, gateway).await;
    ctx.http = api.http.clone();
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Claude;
    Arc::get_mut(&mut shared).unwrap().api_port = api.port;
    shared.restart.reconcile_done.store(true, Ordering::SeqCst);
    shared.http.cached_serenity_ctx.set(ctx).ok().unwrap();
    shared
        .http
        .cached_bot_token
        .set("fixture-token".into())
        .ok()
        .unwrap();
    let channel = ChannelId::new(if held { 7_787_020 } else { 7_787_010 });
    crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "").await;
    let workspace = tempfile::tempdir().unwrap();
    {
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.channel_name = Some(format!("start-failure-{}", channel.get()));
        session.current_path = Some(workspace.path().display().to_string());
    }
    let mailbox = shared.mailbox(channel);
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&shared.token_hash)
        .join(format!("{channel}.json"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    mailbox
        .v2_adopt(path, shared.token_hash.clone())
        .await
        .unwrap();
    let (signals, mut received) = mpsc::unbounded_channel();
    let starts = Arc::new(AtomicUsize::new(0));
    let idle: Handoff = {
        let (shared, starts, signals) = (shared.clone(), starts.clone(), signals.clone());
        Arc::new(move |front| {
            let (shared, starts, signals) = (shared.clone(), starts.clone(), signals.clone());
            Box::pin(async move {
                let id = front.1.message_id;
                if id == A && starts.fetch_add(1, Ordering::SeqCst) == 2 {
                    signals.send(Signal::ThirdStart).unwrap();
                    return Err("fixture third A start witness".into());
                }
                let payload = signals.clone();
                *dispatch::CLAUDE_LEGACY_PROBE.lock().unwrap() = Some((
                    channel.get(),
                    Box::new(move || {
                        println!("START_FAILURE_PRODUCER id={id} before_hook=true");
                        if id == A {
                            return Err(failure.into());
                        }
                        let hook = scope::launch_hook().ok_or("missing first-prompt hook")?;
                        let target = json!({"host":"tmux","owner":"start-failure-fixture","root":"fixture","execution_nonce":"fixture-born"});
                        let source = json!({"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null});
                        let submit: scope::CreatedSubmit = Box::new(move |frame| {
                            payload
                                .send(Signal::Payload(id, frame))
                                .expect("payload observer");
                            Ok(())
                        });
                        hook(scope::Lineage::Created, target, source, None, submit)
                    }),
                ));
                let (finished, bridge) = tokio::sync::oneshot::channel();
                *crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
                    .lock()
                    .unwrap() = Some((channel, finished));
                tokio::spawn(async move {
                    tokio::time::timeout(BUDGET, bridge)
                        .await
                        .expect("bridge release deadline")
                        .expect("real bridge completed");
                    let _ = signals.send(Signal::Closed(id));
                });
                start_idle(shared, channel, front).await
            })
        })
    };
    let drive = owner_drive(
        (mailbox.clone(), None),
        (ProviderKind::Claude, channel.get()),
        idle,
        Some(shared.clone()),
    );
    let wake = signals.clone();
    assert!(
        counted::set_owner_wake_for_tests(
            channel,
            Some(Arc::new(move || {
                let _ = wake.send(Signal::Wake);
            }))
        )
        .is_none()
    );
    let _wake = WakeScope(channel);
    let completion = counted::KickoffCompletion::install(channel);
    let mut completion_events =
        crate::services::discord::turn_completion_events::subscribe_turn_completion_events(
            shared.as_ref(),
        );
    crate::services::discord::queue_io::spawn_turn_completion_idle_queue_listener(
        shared.clone(),
        ProviderKind::Claude,
    );
    completion.listener_ready(BUDGET).await;
    for id in [A, B] {
        let admitted = input_ingress::intake(
            &shared,
            &ProviderKind::Claude,
            &request(channel, id),
            Vec::new(),
            false,
            false,
        )
        .await;
        assert_eq!(admitted, Some(input_ingress::Intake::Accepted));
    }
    while let Ok(signal) = received.try_recv() {
        assert!(
            matches!(signal, Signal::Wake),
            "only ingress wakes precede the first drive"
        );
    }
    crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
    let mut payloads = Vec::new();
    let mut closes = 0;
    let mut third = false;
    tokio::time::timeout(BUDGET, async {
        while let Some(signal) = received.recv().await {
            println!("START_FAILURE_SIGNAL {signal:?}");
            match signal {
                Signal::Wake => {
                    if !third {
                        crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
                    }
                }
                Signal::ThirdStart => {
                    third = true;
                    if closes >= 2 {
                        break;
                    }
                }
                Signal::Payload(id, frame) => payloads.push((id, frame)),
                Signal::Closed(id) => {
                    closes += 1;
                    if held || id == B || (third && closes >= 2) {
                        break;
                    }
                }
            }
        }
    })
    .await
    .expect("A resolution plus B delivery, or an immediate third-start witness");
    if third {
        completion.processed(2, BUDGET).await;
    }
    while let Ok(event) = completion_events.try_recv() {
        println!("START_FAILURE_COMPLETION {event:?}");
        if held {
            assert!(
                !event.queue_is_eligible(),
                "holds emit no QueueEligible edge"
            );
        }
    }
    let snapshot = mailbox
        .v2_request(|slot| slot.as_ref().ok_or("missing owner")?.json())
        .await
        .unwrap();
    let occupied = mailbox.has_active_turn().await.unwrap();
    let row = crate::services::discord::inflight::load_inflight_state(
        &ProviderKind::Claude,
        channel.get(),
    )
    .is_some();
    let writes = api.take();
    let posts: Vec<_> = writes
        .iter()
        .filter(|w| w.starts_with("POST ") && w.contains("/messages "))
        .collect();
    let placeholders = posts
        .iter()
        .filter(|w| {
            w.contains("\"content\":\"...\"") || w.contains("\"content\":\"⠋ 계속 처리 중\"")
        })
        .count();
    let notices = posts.len() - placeholders;
    let deletes = writes.iter().filter(|w| w.starts_with("DELETE ")).count();
    let evidence = json!({"starts_a":starts.load(Ordering::SeqCst),"third":third,"closed":closes,
        "placeholder_posts":placeholders,"placeholder_deletes":deletes,"notice_posts":notices,
        "inflight":row,"mailbox_active":occupied,"payloads":payloads,"snapshot":snapshot,"rest":writes});
    println!("START_FAILURE_EVIDENCE {evidence}");
    if live {
        assert!(!third, "third A start is forbidden: {evidence}");
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        assert_eq!(notices, 1, "placeholder POSTs are separate");
        assert_eq!(
            deletes,
            starts.load(Ordering::SeqCst),
            "each failed A placeholder is deleted"
        );
        assert!(payloads.iter().all(|(id, _)| *id != A));
        let retained = &snapshot["retained_inputs"][0];
        assert!(retained["input"]["attempt"].is_null());
        assert_eq!(retained["input"]["original"]["message_id"], A);
        assert!(
            retained["reason"]["reason"]
                .as_str()
                .unwrap()
                .starts_with("start_rejected: ")
        );
        assert_eq!(snapshot["retained_inputs"].as_array().unwrap().len(), 1);
        assert_eq!(payloads.iter().filter(|(id, _)| *id == B).count(), 1);
        assert!(!occupied && !row);
    } else if held {
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!((placeholders, deletes, notices), (1, 1, 0), "{evidence}");
        assert!(!third && payloads.is_empty() && !occupied && !row);
    } else {
        assert!(
            third,
            "record the current repeat, never silently accept a GREEN: {evidence}"
        );
        assert_eq!((starts.load(Ordering::SeqCst), closes), (3, 2));
        assert_eq!(placeholders, 2);
        assert!(payloads.is_empty());
        assert_eq!(snapshot["items"].as_array().unwrap().len(), 2);
    }
    shared.mailboxes.remove_fixture_for_test(channel);
    server.abort();
}

fn isolated(test: &str) -> bool {
    if std::env::var_os("ADK_START_FAILURE_CHILD").is_some() {
        return true;
    }
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("ADK_START_FAILURE_CHILD", "1")
        .output()
        .expect("isolated fixture child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    print!("{stdout}{stderr}");
    assert!(output.status.success() && stdout.contains("1 passed"));
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_failure_before_records_third_a_start_from_real_completion() {
    if !isolated(
        concat!(
            module_path!(),
            "::start_failure_before_records_third_a_start_from_real_completion"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    fixture(
        "herdr turn: bound execution not confirmed: Herdr(Unknown(ProbeFailed))",
        false,
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_failure_s2_not_sent_frame_deletes_placeholder_without_notice() {
    if !isolated(
        concat!(
            module_path!(),
            "::start_failure_s2_not_sent_frame_deletes_placeholder_without_notice"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    fixture("NotSent(\"herdr_root_recovery_held\")", true, false).await;
}
