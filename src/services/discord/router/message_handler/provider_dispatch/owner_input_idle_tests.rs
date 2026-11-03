//! Owned fronts into an idle Claude pane through the drive and the provider's own observer, whose
//! transcript is found as the live path finds it: by the account's home or the pane's binding.
use super::*;
use crate::services::agent_protocol::StreamMessage;
use crate::services::tui_prompt_dedupe::{TuiRuntimeBinding, register_tmux_runtime_binding};
use std::io::Write;
use std::sync::mpsc::Sender;

use crate::services::claude_tui::input::submission::tests::wait;
use crate::services::discord::router::message_handler::provider_dispatch::{
    CLAUDE_LEGACY_PROBE, ClaudeLegacyProbe,
};

const ASSISTANT: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"answer {n}"}]}}"#;

/// Each Enter the pane records gets its AI turn: an answer, then the turn's end.
fn answers(rig: &Rig) -> std::thread::JoinHandle<()> {
    let (renderer, transcript) = (rig.renderer.clone(), rig.transcript());
    std::thread::spawn(move || {
        for n in 1..=2 {
            let entered = || events(&renderer).iter().filter(|e| *e == "Enter").count() >= n;
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while !entered() {
                assert!(std::time::Instant::now() < deadline, "Enter {n} never came");
                std::thread::sleep(Duration::from_millis(20));
            }
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap();
            let answer = ASSISTANT.replace("{n}", &n.to_string());
            writeln!(file, "{answer}\n{}", IDLE.trim_end()).unwrap();
        }
    })
}

/// The pane's runtime binding naming `session`, as the relay rehydrates it after each turn ends.
fn rehydrate(rig: &Rig, session: &str) {
    let binding = TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        output_path: rig.transcript().display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session.to_string()),
        last_offset: 0,
        relay_last_offset: None,
    };
    register_tmux_runtime_binding(&rig.name, binding);
}

/// A Discord turn on the rig's pane for `front`, resuming `session` in a working dir that holds no
/// transcript of it, as a session cached for a pane whose transcript lives elsewhere; `driver`
/// stands in for the provider's driver when the turn launches.
fn idle_turn(
    rig: &Rig,
    (session, dir): (&str, &Path),
    (front, settled): (Front, Settled),
    driver: impl FnOnce(&Sender<StreamMessage>) -> Option<ClaudeLegacyProbe>,
) -> (Result<(), String>, Vec<StreamMessage>) {
    let claim = rig.claim(&front).expect("granted front");
    let owner = OwnerTurn {
        mailbox: rig.mailbox.clone(),
        claim,
        settled,
        origin: InputOrigin::Discord,
    };
    let (host, provider) = (TurnHost::Tmux, ProviderKind::Claude);
    let turn = StreamingTurn {
        pool: None,
        shared: None,
        provider: &provider,
        prompt: &front.1.text,
        session_id: Some(session),
        working_dir: dir.to_str().unwrap(),
        system_prompt: None,
        allowed_tools: &[],
        cancel: Arc::new(CancelToken::new()),
        codex_observation: None,
        tmux_session_name: Some(&rig.name),
        teardown: None,
        host: &host,
        channel_id: rig.channel,
        model: None,
        native_fast_mode: None,
        effort: None,
        codex_goals: None,
        compact_percent: None,
        compact_lower_bound_tokens: 0,
        compact_token_limit: None,
        cache_ttl_minutes: None,
        dispatch_type: None,
        force_fresh: false,
        submission: None,
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    if let Some(driver) = driver(&sender) {
        *CLAUDE_LEGACY_PROBE.lock().unwrap() = Some((rig.channel, driver));
    }
    let _entered = rig.runtime.enter();
    let result = execute_owned(turn, sender, owner);
    (result, receiver.try_iter().collect())
}

/// After a person's turn ends, one wake hands two queued inputs to an idle pane whose transcript only
/// its binding names: in order, each pasted, entered and answered once, then nothing more.
#[test]
fn an_idle_pane_takes_each_queued_front_once_through_its_own_observer() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oi-idle-d2", 7_272_595, "normal", view("", false), BUSY);
    let second: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":rig.original.message_id + 1,"channel_id":rig.channel,
        "source_message_ids":[rig.original.message_id + 1],"queued_generation":1,
        "text":"second question"}))
    .unwrap();
    let admitted = admit(&rig.mailbox, &second, AdmissionTicket::new());
    rig.runtime.block_on(admitted);
    let session = uuid::Uuid::new_v4().to_string();
    let elsewhere = tempfile::tempdir().unwrap();
    let (fronts, inbox) = std::sync::mpsc::channel::<(Front, Settled)>();
    let fronts = std::sync::Mutex::new(fronts);
    let idle: Handoff = Arc::new(move |front| {
        let (settled, answer) = tokio::sync::oneshot::channel();
        let sent = fronts.lock().unwrap().send((front, settled));
        Box::pin(async move {
            sent.map_err(|e| e.to_string())?;
            answer.await.map_err(|e| e.to_string())?
        })
    });
    let drive = owner_drive(
        (rig.mailbox.clone(), None),
        (ProviderKind::Claude, rig.channel),
        idle,
    );
    // The person's turn ends; the supervisor's boundary wake is the one event.
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(rig.transcript())
        .unwrap();
    writeln!(file, "{}", IDLE.trim_end()).unwrap();
    let answering = answers(&rig);
    {
        let _entered = rig.runtime.enter();
        crate::services::discord::queue_io::input_owner::drive::wake(drive);
    }
    let mut handed = Vec::new();
    let mut said = Vec::new();
    while let Ok((front, settled)) = inbox.recv_timeout(Duration::from_secs(15)) {
        handed.push(front.1.text.clone());
        rehydrate(&rig, &session);
        let at = (session.as_str(), elsewhere.path());
        let (result, sent) = idle_turn(&rig, at, (front, settled), |_| None);
        assert_eq!(result, Ok(()), "{handed:?}");
        said.extend(sent.into_iter().filter_map(|m| match m {
            StreamMessage::Text { content } => Some(content),
            _ => None,
        }));
        if handed.len() == 2 {
            break;
        }
    }
    answering.join().unwrap();
    assert_eq!(handed, [PROMPT, "second question"], "in order, each once");
    let events = rig.events();
    let count = |key: &str| events.iter().filter(|e| *e == key).count();
    assert_eq!((count("paste"), count("Enter")), (2, 2), "{events:?}");
    let answered = |n: &str| said.iter().filter(|text| text.contains(n)).count();
    assert_eq!(
        (answered("answer 1"), answered("answer 2")),
        (1, 1),
        "{said:?}"
    );
    assert!(
        inbox.recv_timeout(Duration::from_secs(3)).is_err(),
        "nothing more is handed on"
    );
    let snapshot = rig.snapshot();
    assert!(
        snapshot["items"].as_array().unwrap().is_empty(),
        "{snapshot}"
    );
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}

/// A boot re-adopts a channel's kept V2 queue and wakes its owner drive, which grants the kept front
/// and hands it to the idle start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_boot_recovery_hands_the_kept_front_to_the_owner_drive() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = poise::serenity_prelude::ChannelId::new(7_272_596);
    let persistence = crate::services::discord::queue_dispatch::persistence_context;
    let persisted = persistence(&shared, &shared.provider, channel);
    let queue = crate::services::turn_orchestrator::v2_queue(&persisted, channel).unwrap();
    let kept: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":72_725_961,"channel_id":channel.get(),"source_message_ids":[72_725_961],
        "queued_generation":1,"text":"kept"}))
    .unwrap();
    {
        let before = ChannelMailboxRegistry::default();
        let handle = before.handle(channel);
        handle.v2_adopt(queue.0, queue.1).await.unwrap();
        let _ = admit(&handle, &kept, AdmissionTicket::new()).await;
    }
    recover(&shared, channel).await;
    let mailbox = shared.mailbox(channel);
    let granted =
        || mailbox.v2_request(|slot| Ok(slot.as_ref().is_some_and(|o| o.front_outstanding())));
    let mut handed = false;
    for _ in 0..100 {
        handed = granted().await.unwrap();
        if handed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(handed, "the recovered owner's drive granted its kept front");
}

/// Stands in for the live path's driver after it replaced the stale pane: the pane runs again under
/// the channel's account, and the owner's hook writes the created execution's first prompt.
fn relaunch(
    rig: &Rig,
    (session, dir): (&str, &Path),
    sender: &Sender<StreamMessage>,
) -> ClaudeLegacyProbe {
    let (name, channel, transcript) = (rig.name.clone(), rig.channel, rig.transcript());
    let (real, socket) = (rig.f.real.clone(), rig.f.socket.clone());
    let (ready, events) = (rig.renderer.join("ready"), rig.renderer.join("events"));
    let (sender, session, dir) = (sender.clone(), session.to_string(), dir.to_path_buf());
    Box::new(move || {
        if events.exists() {
            return Err("a key reached the other account's pane".into());
        }
        let default = crate::services::provider_auth_profile::DEFAULT_PROFILE_ID;
        crate::services::tmux_common::write_tmux_session_auth_profile(&name, default)?;
        fs::remove_file(&ready).map_err(|e| e.to_string())?;
        let pane = format!("={name}:");
        let respawn = [
            "-L",
            &socket,
            "-f",
            "/dev/null",
            "respawn-pane",
            "-k",
            "-t",
            &pane,
        ];
        let status = std::process::Command::new(&real).args(respawn).status();
        status
            .map_err(|e| e.to_string())?
            .success()
            .then_some(())
            .ok_or("respawn failed")?;
        wait("relaunched", || ready.exists());
        let hook = scope::launch_hook().ok_or("no owner hook")?;
        let target = scope::created_tmux_target("claude", &name)?;
        let source = json!({"kind":"pre_source","expected_session_id":null,
            "expected_transcript":null,"binding_watermark":null});
        let observe = crate::services::claude_tui::hosting::owner_observer;
        let cancel = Arc::new(CancelToken::new());
        let at = (name.as_str(), PROMPT, channel);
        let until_done = observe(at, (Some(&session), &dir), sender, cancel).ok_or("unobserved")?;
        let path = transcript.display().to_string();
        let submit: scope::CreatedSubmit = Box::new(move |frame| {
            crate::services::claude::first_input::submit_created(&path, 0, &name, &frame).map(drop)
        });
        hook(scope::Lineage::Created, target, source, None, submit)?;
        until_done()
    })
}

/// A live idle pane under another account than the channel's takes no key: the first front goes to
/// the driver's replacement under the channel's account, the second into that pane, each answered once.
#[test]
fn a_pane_under_another_account_is_replaced_before_any_front_is_written() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oi-idle-acct", 7_272_597, "normal", view("", false), BUSY);
    let second: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":rig.original.message_id + 1,"channel_id":rig.channel,
        "source_message_ids":[rig.original.message_id + 1],"queued_generation":1,
        "text":"second question"}))
    .unwrap();
    let admitted = admit(&rig.mailbox, &second, AdmissionTicket::new());
    rig.runtime.block_on(admitted);
    crate::services::tmux_common::write_tmux_session_auth_profile(&rig.name, "account-a").unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let elsewhere = tempfile::tempdir().unwrap();
    let (fronts, inbox) = std::sync::mpsc::channel::<(Front, Settled)>();
    let fronts = std::sync::Mutex::new(fronts);
    let idle: Handoff = Arc::new(move |front| {
        let (settled, answer) = tokio::sync::oneshot::channel();
        let sent = fronts.lock().unwrap().send((front, settled));
        Box::pin(async move {
            sent.map_err(|e| e.to_string())?;
            answer.await.map_err(|e| e.to_string())?
        })
    });
    let drive = owner_drive(
        (rig.mailbox.clone(), None),
        (ProviderKind::Claude, rig.channel),
        idle,
    );
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(rig.transcript())
        .unwrap();
    writeln!(file, "{}", IDLE.trim_end()).unwrap();
    let answering = answers(&rig);
    {
        let _entered = rig.runtime.enter();
        crate::services::discord::queue_io::input_owner::drive::wake(drive);
    }
    let (mut handed, mut said) = (Vec::new(), Vec::new());
    while let Ok((front, settled)) = inbox.recv_timeout(Duration::from_secs(15)) {
        handed.push(front.1.text.clone());
        rehydrate(&rig, &session);
        let at = (session.as_str(), elsewhere.path());
        let first = handed.len() == 1;
        let driver = |sender: &Sender<StreamMessage>| first.then(|| relaunch(&rig, at, sender));
        let (result, sent) = idle_turn(&rig, at, (front, settled), driver);
        assert_eq!(result, Ok(()), "{handed:?}");
        assert!(
            CLAUDE_LEGACY_PROBE.lock().unwrap().is_none(),
            "the driver ran"
        );
        said.extend(sent.into_iter().filter_map(|m| match m {
            StreamMessage::Text { content } => Some(content),
            _ => None,
        }));
        if handed.len() == 2 {
            break;
        }
    }
    answering.join().unwrap();
    assert_eq!(handed, [PROMPT, "second question"], "in order, each once");
    let events = rig.events();
    let count = |key: &str| events.iter().filter(|e| *e == key).count();
    assert_eq!((count("paste"), count("Enter")), (2, 2), "{events:?}");
    let answered = |n: &str| said.iter().filter(|text| text.contains(n)).count();
    assert_eq!(
        (answered("answer 1"), answered("answer 2")),
        (1, 1),
        "{said:?}"
    );
    let snapshot = rig.snapshot();
    assert!(
        snapshot["items"].as_array().unwrap().is_empty(),
        "{snapshot}"
    );
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}
