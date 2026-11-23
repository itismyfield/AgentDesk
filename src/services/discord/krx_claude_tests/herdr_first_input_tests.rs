//! Moved from src/services/claude/herdr_turn/first_input.rs: drives the discord input owner.

use crate::services::claude::herdr_turn::first_input::regression_tests::regression_attached;
use crate::services::claude::herdr_turn::first_input::*;
use crate::services::claude_tui::guarded_outcome;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn herdr_composer_refusal_keeps_real_owner_proof_and_wake_then_submits_once() {
    use crate::services::discord::{
        owner_drive_for_test as drive, owner_effect_for_test as effect,
        owner_held_watch_for_test as held_watch,
    };
    use crate::services::session_host::herdr_socket_rig_tests::{
        HerdrRig, KEY, NODE, PANE, SESSION,
    };
    let root = tempfile::tempdir().unwrap();
    let root_guard = crate::config::set_agentdesk_root_for_test(root.path());
    let rig = HerdrRig::start();
    let registry_guard = rig.registry_on_this_thread();
    let context = rig.context("00112233445566778899aabbccddee01");
    rig.run_provider(&context, false);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .on_thread_start(rig.thread_installer(NODE, KEY, rig.socket(), SESSION))
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    let channel = 727487;
    let logical = "herdr-held-regression";
    let marker = crate::services::tmux_common::session_temp_path(logical, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, "herdr").unwrap();
    let transcript = root.path().join("not-created.jsonl");
    let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let target = serde_json::json!({"host":"herdr","owner":logical,"root":rig.socket().display().to_string(),"execution_nonce":"00112233445566778899aabbccddee01"});
    let attempt = serde_json::json!({"nonce":"held-attempt-1","provider":"claude","target":target,"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
    runtime
        .block_on(mailbox.v2_cold_fixture(
            root.path().join("held.json"),
            attempt.clone(),
            "one original".into(),
            channel,
        ))
        .unwrap();
    let capture = |draft: &str| serde_json::json!({"type":"pane_read","read":{"pane_id":PANE,"source":"recent_unwrapped","text":format!("✻ Baked for 2s\n\n{}\n❯ {draft}\n{}\n  🤖 Opus(H) │ 7% │ MCP: 2\n  ⏵⏵ bypass permissions on (shift+tab to cycle)","─".repeat(52),"─".repeat(52)),"revision":1,"truncated":false}});
    rig.answer("pane.read", capture("human draft"));
    let (_, attached) = regression_attached(&rig, logical, &transcript);
    let writer = format!("{}#{}", rig.socket().display(), PANE);
    let leaf = effect::created(
        Box::new(move |frame: String| {
            guarded_outcome::capture_preparation(|| {
                let (sender, receiver) = std::sync::mpsc::channel();
                let result = write(logical, None, attached, &frame, &sender, None).map(drop);
                drop(receiver);
                result
            })
        }),
        Some(writer.clone()),
    );
    let result = runtime
        .block_on(effect::run(&mailbox, Arc::new(|| 10000), leaf))
        .unwrap();
    assert!(matches!(result, effect::Completion::NotSent(_)));
    let snapshot = runtime
        .block_on(mailbox.v2_request(|slot| slot.as_ref().unwrap().json()))
        .unwrap();
    assert!(snapshot["head_injection"].is_null());
    assert_eq!(snapshot["items"].as_array().unwrap().len(), 1);
    assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
    assert!(rig.sends().is_empty());
    let message = snapshot["items"][0]["message_id"].as_u64().unwrap();
    rig.answer("pane.read", capture(""));
    let (woken, wake) = tokio::sync::oneshot::channel();
    let woken = Arc::new(std::sync::Mutex::new(Some(woken)));
    let drive = Arc::new(drive::Drive {
        mailbox: mailbox.clone(),
        channel,
        handoff: Arc::new(move |_| {
            if let Some(woken) = woken.lock().unwrap().take() {
                woken.send(()).unwrap();
            }
            Box::pin(async { Err("observed actual owner wake".into()) })
        }),
        settled: None,
    });
    let observed = runtime.block_on(async {
        let (_, observed) = held_watch::watch_observed(drive, message);
        observed.await.unwrap()
    });
    assert_eq!(observed.verdict, held_watch::Verdict::Wake);
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), wake)
            .await
            .unwrap()
            .unwrap()
    });
    let second = attempt.clone();
    runtime.block_on(mailbox.v2_request(move |slot| {
    let owner = slot.as_mut().unwrap();
    let mut second = second; second["nonce"] = "held-attempt-2".into();
    owner.reserve(serde_json::from_value(second).unwrap(), serde_json::from_value(serde_json::json!({"frame":"one original","digest":"fixture","attribution":{}})).unwrap()).map(drop)
})).unwrap();
    let (_, attached) = regression_attached(&rig, logical, &transcript);
    let leaf = effect::created(
        Box::new(move |frame: String| {
            guarded_outcome::capture_preparation(|| {
                let (sender, receiver) = std::sync::mpsc::channel();
                let result = write(logical, None, attached, &frame, &sender, None).map(drop);
                drop(receiver);
                result
            })
        }),
        Some(writer),
    );
    assert_eq!(
        runtime
            .block_on(effect::run(&mailbox, Arc::new(|| 10001), leaf))
            .unwrap(),
        effect::Completion::Submitted
    );
    assert_eq!(
        rig.sends().len(),
        2,
        "one paste and one Enter after the wake"
    );
    let snapshot = runtime
        .block_on(mailbox.v2_request(|slot| slot.as_ref().unwrap().json()))
        .unwrap();
    assert!(snapshot["items"].as_array().unwrap().is_empty());
    assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
    drop(entered);
    drop(registry_guard);
    drop(root_guard);
}
