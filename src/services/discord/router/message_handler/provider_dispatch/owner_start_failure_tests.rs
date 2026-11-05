#![doc = include_str!("../../../../../../docs/statecharts/input-single-path/start-failure.classification.md")]

use super::*;
use crate::services::discord::queue_io::cancel_backstop_test_support as counted;
use crate::services::discord::queue_io::input_ingress;
use crate::services::discord::router::message_handler::provider_dispatch as dispatch;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::mpsc;

const BUDGET: Duration = Duration::from_secs(30);
const A: u64 = 7_787_001;
const B: u64 = 7_787_002;

#[derive(Debug)]
enum Signal {
    Wake,
    Pass,
    ThirdStart,
    Payload(u64, String),
    Closed(u64),
}

struct WakeScope(ChannelId);
impl Drop for WakeScope {
    fn drop(&mut self) {
        counted::set_owner_wake_for_tests(self.0, None);
        *dispatch::CLAUDE_LEGACY_PROBE.lock().unwrap() = None;
        *dispatch::start_failure::FACTS.lock().unwrap() = None;
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
async fn fixture(
    failure: &'static str,
    held: bool,
    live: bool,
    recover: bool,
    policy_early: bool,
    confirmed: u8,
) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
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
        .v2_adopt(path.clone(), shared.token_hash.clone())
        .await
        .unwrap();
    let (signals, mut received) = mpsc::unbounded_channel();
    let starts = Arc::new(AtomicUsize::new(0));
    let cause_cleared = Arc::new(AtomicBool::new(false));
    let idle: Handoff = {
        let cause_cleared = cause_cleared.clone();
        let (shared, starts, signals) = (shared.clone(), starts.clone(), signals.clone());
        Arc::new(move |front| {
            let (shared, starts, signals) = (shared.clone(), starts.clone(), signals.clone());
            let cause_cleared = cause_cleared.clone();
            Box::pin(async move {
                let id = front.1.message_id;
                let ordinal = if id == A {
                    starts.fetch_add(1, Ordering::SeqCst) + 1
                } else {
                    0
                };
                if id == A && ordinal == 3 {
                    signals.send(Signal::ThirdStart).unwrap();
                    return Err("fixture third A start witness".into());
                }
                let recovered = (recover && ordinal > 1) || cause_cleared.load(Ordering::SeqCst);
                if id == B || recovered {
                    *dispatch::CLAUDE_LEGACY_DONE.lock().unwrap() =
                        Some((channel.get(), format!("fixture completed {id}")));
                }
                let payload = signals.clone();
                *dispatch::CLAUDE_LEGACY_PROBE.lock().unwrap() = Some((
                    channel.get(),
                    Box::new(move || {
                        println!("START_FAILURE_PRODUCER id={id} before_hook=true");
                        if id == A && !recovered {
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
                if confirmed == 0 || id != A {
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
                }
                let _hosts = (confirmed > 0 && id == A).then(|| {
                    crate::config::session_hosts::force_for_test(
                        Some("fixture-local"),
                        &[(channel.get(), "fixture-remote")],
                    )
                });
                start_idle(shared, channel, front).await
            })
        })
    };
    let mut drive = owner_drive(
        (mailbox.clone(), Some(pool.clone())),
        (ProviderKind::Claude, channel.get()),
        idle,
        Some(shared.clone()),
    );
    let settled = drive.settled.clone();
    let pass = signals.clone();
    Arc::get_mut(&mut drive).unwrap().settled = Some(Arc::new(move || {
        let (settled, pass) = (settled.clone(), pass.clone());
        Box::pin(async move {
            if let Some(settled) = settled {
                settled().await;
            }
            let _ = pass.send(Signal::Pass);
        })
    }));
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
        if confirmed == 2 && id == A {
            let original: PendingQueueItem = serde_json::from_value(json!({"author_id":A,"message_id":A,"channel_id":channel.get(),"queued_generation":1,"text":"입력 A — 원문 보존","has_reply_boundary":true})).unwrap();
            let admitted = crate::services::discord::queue_io::input_owner::admission::admit_with_context(
                &mailbox, &original, crate::services::discord::queue_io::input_owner::admission::AdmissionTicket::new(),
                json!({"origin":"headless","request_owner":A,"request_owner_name":"fixture","source":"fixture","is_dm":false,"synthetic_label":"fixture-headless"})
            ).await;
            assert!(matches!(
                admitted,
                crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
            ));
            continue;
        }
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
    let (facts, mut observed_facts) = mpsc::unbounded_channel();
    if policy_early {
        *dispatch::start_failure::FACTS.lock().unwrap() = Some((channel.get(), facts));
    }
    let (bridge_entered, bridge_release) = if policy_early {
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, gate) = tokio::sync::oneshot::channel();
        *dispatch::start_failure::BRIDGE_GATE.lock().unwrap() =
            Some((channel.get(), entered, gate));
        (Some(waiting), Some(release))
    } else {
        (None, None)
    };
    let (commit_entered, commit_release) = if policy_early {
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, gate) = tokio::sync::oneshot::channel();
        *dispatch::start_failure::COMMIT_GATE.lock().unwrap() =
            Some((channel.get(), entered, gate));
        (Some(waiting), Some(release))
    } else {
        (None, None)
    };
    let mut notice_lock = if std::env::var_os("I7787_NOTICE_BLOCKED").is_some() {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE message_outbox IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .unwrap();
        Some(tx)
    } else {
        None
    };
    crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
    if let (Some(entered), Some(release)) = (commit_entered, commit_release) {
        tokio::time::timeout(BUDGET, entered)
            .await
            .unwrap()
            .unwrap();
        let before = mailbox
            .v2_request(|slot| {
                let owner = slot.as_ref().ok_or("owner")?;
                Ok(json!({"snapshot":owner.json()?,"front_blocked":owner.front().is_none()}))
            })
            .await
            .unwrap();
        assert_eq!(
            before["front_blocked"], true,
            "actor closing committed before removal"
        );
        assert!(mailbox.has_active_turn().await.unwrap());
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        let remove = || {
            ctx.with(|ctx| {
                crate::engine::ops::register_globals_with_supervisor_and_pg(
                    &ctx,
                    None,
                    crate::supervisor::BridgeHandle::new(),
                )
                .unwrap();
                ctx.eval(format!(
                    "JSON.stringify(agentdesk.inflight.remove('claude','{}'))",
                    channel.get()
                ))
                .unwrap()
            })
        };
        let mut removed: String = remove();
        for _ in 0..4 {
            let result = serde_json::from_str::<Value>(&removed).unwrap();
            println!("POLICY_REMOVE_RESULT {result}");
            if result["ok"] == true {
                break;
            }
            assert_eq!(result["reason"], "Busy", "policy removal refused: {result}");
            assert!(
                observed_facts.try_recv().is_err(),
                "Busy supplies no cleanup fact"
            );
            let path = crate::services::discord::runtime_store::discord_inflight_root()
                .unwrap()
                .join("claude")
                .join(format!("{channel}.json"));
            tokio::task::spawn_blocking(move || {
                let lock = crate::services::discord::inflight::lock_inflight_state_path(&path)?;
                drop(lock);
                Ok::<(), String>(())
            })
            .await
            .unwrap()
            .unwrap();
            removed = remove();
        }
        assert_eq!(
            serde_json::from_str::<Value>(&removed).unwrap()["ok"],
            true,
            "policy removal never succeeded: {removed}"
        );
        loop {
            let (_, fact) = tokio::time::timeout(BUDGET, observed_facts.recv())
                .await
                .unwrap()
                .unwrap();
            if fact == "row_removed" {
                break;
            }
        }
        assert!(mailbox.has_active_turn().await.unwrap());
        release.send(()).unwrap();
        tokio::time::timeout(BUDGET, bridge_entered.unwrap())
            .await
            .unwrap()
            .unwrap();
        loop {
            let event = tokio::time::timeout(BUDGET, received.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(event, Signal::Pass) {
                break;
            }
            assert!(
                !matches!(event, Signal::Wake),
                "row alone must not wake the drive"
            );
            println!("POLICY_EARLY_PENDING {event:?}");
        }
        assert!(mailbox.has_active_turn().await.unwrap());
        let blocked = mailbox
            .v2_request(|slot| Ok(slot.as_ref().ok_or("owner")?.front().is_none()))
            .await
            .unwrap();
        assert!(blocked, "row deletion alone cannot clear closing");
        while let Ok(event) = received.try_recv() {
            assert!(
                !matches!(event, Signal::Wake),
                "no wake before exact token release"
            );
        }
        println!(
            "POLICY_EARLY_ROW_REMOVAL token_active=true actor_committed=true wake_pass_finished=true state={}",
            mailbox
                .v2_request(|slot| slot.as_ref().ok_or("owner")?.json())
                .await
                .unwrap()
        );
        bridge_release.unwrap().send(()).unwrap();
    }
    let mut payloads = Vec::new();
    let mut closes = 0;
    let mut closed_ids = Vec::new();
    let mut third = false;
    tokio::time::timeout(BUDGET, async {
        while let Some(signal) = received.recv().await {
            println!("START_FAILURE_SIGNAL {signal:?}");
            match signal {
                Signal::Pass => {}
                Signal::Wake => {
                    if !third {
                        crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
                    }
                }
                Signal::ThirdStart => {
                    assert!(
                        !live,
                        "third A start: failure budget did not retain the original"
                    );
                    third = true;
                    if closes >= 2 {
                        break;
                    }
                }
                Signal::Payload(id, frame) => {
                    payloads.push((id, frame));
                    if id == B {
                        if let Some(tx) = notice_lock.take() {
                            println!("B_PAYLOAD_WHILE_NOTICE_OUTBOX_LOCKED");
                            tx.rollback().await.unwrap();
                        }
                    }
                }
                Signal::Closed(id) => {
                    closes += 1;
                    closed_ids.push(id);
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
    if live && !recover && confirmed == 0 {
        cause_cleared.store(true, Ordering::SeqCst);
        crate::services::discord::queue_io::input_owner::recovery::recover(
            &mailbox,
            (path, shared.token_hash.clone()),
            crate::services::tui_input::human_frame::history::v2::Registry::runtime().unwrap(),
            None,
            (
                "claude",
                channel.get(),
                chrono::Utc::now().timestamp_millis() as u64,
            ),
        )
        .await
        .unwrap();
        while received.try_recv().is_ok() {}
        crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
        tokio::time::timeout(BUDGET, async {
            while let Some(event) = received.recv().await {
                match event {
                    Signal::Pass => break,
                    Signal::ThirdStart | Signal::Payload(A, _) => {
                        panic!("retained A replayed after recovery")
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("post-retention recovery pass");
        assert_eq!(
            starts.load(Ordering::SeqCst),
            2,
            "cause recovery never requeues retained A"
        );
        println!("RETENTION_CAUSE_CLEARED recovery_pass=true auto_restarts=0");
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
    let drain = crate::services::discord::queue_io::input_owner::notice::drain;
    crate::services::discord::queue_io::input_owner::notice::drain_synchronously_for_tests(
        &pool,
        &mailbox,
        "claude",
        channel.get(),
    )
    .await
    .unwrap();
    assert_eq!(
        drain(&pool, &mailbox, "claude", channel.get())
            .await
            .unwrap(),
        0
    );
    let owed: Vec<String> = sqlx::query_scalar("SELECT content FROM message_outbox WHERE source='retained_input_notice' AND target=$1 ORDER BY id")
        .bind(format!("channel:{}",channel.get())).fetch_all(&pool).await.unwrap();
    for content in &owed {
        channel.say(&api.http, content).await.unwrap();
    }
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
    let notices = posts
        .iter()
        .filter(|w| w.contains("이 입력은 전달되지 않은 것을 확인해 원문을 보관했습니다"))
        .count();
    let b_summaries = posts
        .iter()
        .filter(|w| w.contains("fixture completed") || w.contains("✅"))
        .count();
    let error_cards = writes
        .iter()
        .filter(|w| w.contains("provider가 응답을 완료하지") || w.contains("오류 ·"))
        .count();
    let deletes = writes.iter().filter(|w| w.starts_with("DELETE ")).count();
    let evidence = json!({"starts_a":starts.load(Ordering::SeqCst),"third":third,"closed":closes,
        "placeholder_posts":placeholders,"placeholder_deletes":deletes,"notice_posts":notices,
        "inflight":row,"mailbox_active":occupied,"payloads":payloads,"snapshot":snapshot,"rest":writes,
        "closed_ids":closed_ids,"b_success_posts":b_summaries,"generic_error_cards":error_cards,"outbox_notices":owed.len()});
    println!("START_FAILURE_EVIDENCE {evidence}");
    if live {
        assert!(!third, "third A start is forbidden: {evidence}");
        assert_eq!(
            starts.load(Ordering::SeqCst),
            if confirmed > 0 { 1 } else { 2 }
        );
        assert_eq!(
            notices,
            usize::from(!recover),
            "A retention is separate from placeholders and B output"
        );
        assert_eq!(error_cards, 0, "start failures use the NotSent arm");
        assert_eq!(
            closed_ids.iter().filter(|id| **id == A).count(),
            if confirmed > 0 { 0 } else { 2 }
        );
        assert_eq!(closed_ids.iter().filter(|id| **id == B).count(), 1);
        assert_eq!(
            deletes,
            if confirmed > 0 {
                0
            } else if recover {
                1
            } else {
                starts.load(Ordering::SeqCst)
            },
            "each failed A placeholder is deleted"
        );
        if recover {
            assert_eq!(payloads.iter().filter(|(id, _)| *id == A).count(), 1);
            assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
        } else {
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
        }
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
    pool.close().await;
    db.drop().await;
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
async fn start_failure_repeated_retains_once_and_delivers_b_pg() {
    if !isolated(
        concat!(
            module_path!(),
            "::start_failure_repeated_retains_once_and_delivers_b_pg"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    fixture(
        "herdr turn: bound execution not confirmed: Herdr(Unknown(ProbeFailed))",
        false,
        true,
        false,
        false,
        0,
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
    fixture(
        "NotSent(\"herdr_root_recovery_held\")",
        true,
        false,
        false,
        false,
        0,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_failure_recovers_before_retry_without_a_notice_pg() {
    if !isolated(
        concat!(
            module_path!(),
            "::start_failure_recovers_before_retry_without_a_notice_pg"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    fixture("opaque context I/O", false, true, true, false, 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_failure_policy_removal_before_token_release_keeps_progress_pg() {
    if !isolated(
        concat!(
            module_path!(),
            "::start_failure_policy_removal_before_token_release_keeps_progress_pg"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    fixture("opaque before input", false, true, false, true, 0).await;
}

#[tokio::test]
async fn start_failure_discord_preserves_typed_confirmed_refusal_pg() {
    if !isolated(
        concat!(
            module_path!(),
            "::start_failure_discord_preserves_typed_confirmed_refusal_pg"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    fixture("unused typed refusal", false, true, false, false, 1).await;
}
#[tokio::test]
async fn start_failure_headless_preserves_typed_confirmed_refusal_pg() {
    if !isolated(
        concat!(
            module_path!(),
            "::start_failure_headless_preserves_typed_confirmed_refusal_pg"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    fixture("unused typed refusal", false, true, false, false, 2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_failure_blocked_notice_does_not_block_b_pg() {
    if !isolated(
        concat!(
            module_path!(),
            "::start_failure_blocked_notice_does_not_block_b_pg"
        )
        .trim_start_matches("agentdesk::"),
    ) {
        return;
    }
    unsafe {
        std::env::set_var("I7787_NOTICE_BLOCKED", "1");
    }
    fixture("opaque before input", false, true, false, false, 0).await;
}
