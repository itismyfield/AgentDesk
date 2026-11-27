use crate::services::claude::herdr_turn::hold_record::{self, Held, SendOutcome, UnsentProof};
use crate::services::claude::herdr_turn::{
    Attached, Notice, held_record_for_test, record_outcome_for_test, regression_attached,
    write_for_test,
};
use crate::services::claude_tui::guarded_outcome;
use crate::services::claude_tui::host_input::InputRun;
use std::path::Path;
use std::sync::Arc;

/// A8, A9: a hold left before its first key keeps the owner's front Held and watched, then
/// releases unconfirmed after its notice row; the next input submits once.
#[test]
fn hold_resolve_crash_after_hold_before_first_key_holds_front_then_releases_and_submits_once() {
    use crate::services::discord::{
        owner_effect_for_test as effect, owner_held_watch_for_test as held_watch,
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
    let (channel, logical) = (727488, "herdr-hold-resolve");
    let marker = crate::services::tmux_common::session_temp_path(logical, "host_kind");
    std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, "herdr").unwrap();
    let transcript = root.path().join("not-created.jsonl");
    let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let target = serde_json::json!({"host":"herdr","owner":logical,"root":rig.socket().display().to_string(),"execution_nonce":"00112233445566778899aabbccddee01"});
    let attempt = serde_json::json!({"nonce":"hold-attempt-1","provider":"claude","target":target,"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
    let fixture = mailbox.v2_cold_fixture(
        root.path().join("hold.json"),
        attempt.clone(),
        "next input".into(),
        channel,
    );
    runtime.block_on(fixture).unwrap();
    rig.answer("pane.read", serde_json::json!({"type":"pane_read","read":{"pane_id":PANE,"source":"recent_unwrapped","text":format!("✻ Baked for 2s\n\n{}\n❯ \n{}\n  🤖 Opus(H) │ 7% │ MCP: 2\n  ⏵⏵ bypass permissions on (shift+tab to cycle)","─".repeat(52),"─".repeat(52)),"revision":1,"truncated":false}}));
    let (_, attached) = regression_attached(&rig, logical, &transcript);
    let ago = chrono::Utc::now() - chrono::Duration::hours(1);
    let mut left = held_record_for_test(logical, &attached, "earlier input", 0, ago);
    left.recorded_at = ago.to_rfc3339();
    hold_record::write(&left).unwrap();
    let notices = Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = format!("{}#{}", rig.socket().display(), PANE);
    let leaf = |attached: Attached, notice: Option<Arc<Notice>>| {
        effect::created(
            Box::new(move |frame: String| {
                guarded_outcome::capture_preparation(|| {
                    let (sender, _receiver) = std::sync::mpsc::channel();
                    write_for_test(logical, None, attached, &frame, &sender, notice.as_deref())
                        .map(drop)
                })
            }),
            Some(writer.clone()),
        )
    };
    let held = runtime.block_on(effect::run(
        &mailbox,
        Arc::new(|| 10000),
        leaf(attached, None),
    ));
    assert!(matches!(held.unwrap(), effect::Completion::NotSent(_)));
    let snapshot = runtime
        .block_on(mailbox.v2_request(|slot| slot.as_ref().unwrap().json()))
        .unwrap();
    assert_eq!(snapshot["items"].as_array().unwrap().len(), 1);
    assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
    assert!(held_watch::armed(channel) && rig.sends().is_empty());
    let mut second = attempt;
    second["nonce"] = "hold-attempt-2".into();
    runtime
        .block_on(mailbox.v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            let prepared =
                serde_json::json!({"frame":"next input","digest":"fixture","attribution":{}});
            owner
                .reserve(
                    serde_json::from_value(second).unwrap(),
                    serde_json::from_value(prepared).unwrap(),
                )
                .map(drop)
        }))
        .unwrap();
    let seen = notices.clone();
    let notice: Arc<Notice> = Arc::new(move |hold: &str| {
        seen.lock().unwrap().push(hold.to_owned());
        Ok(7)
    });
    let (_, attached) = regression_attached(&rig, logical, &transcript);
    let submitted = runtime.block_on(effect::run(
        &mailbox,
        Arc::new(|| 10001),
        leaf(attached, Some(notice)),
    ));
    assert_eq!(submitted.unwrap(), effect::Completion::Submitted);
    assert_eq!(
        (rig.sends().len(), notices.lock().unwrap().clone()),
        (2, vec![left.hold_id.clone()])
    );
    assert_eq!(hold_record::read(&left.execution_nonce), Ok(None));
    let (_, attached) = regression_attached(&rig, logical, &transcript);
    let refused = InputRun::Indeterminate {
        confirmed: 1,
        cause: crate::services::claude_tui::host_input::StopCause::Refused(
            crate::services::claude_tui::host_input::InputRefusal::Unknown,
        ),
    };
    record_outcome_for_test(
        held_record_for_test(logical, &attached, "next input", 0, ago),
        &refused,
    );
    let Ok(Some(Held::V2(record))) = hold_record::read(attached.nonce_for_test_for_test()) else {
        panic!("the outcome rewrote the in-flight record");
    };
    let recorded = (
        record.send_outcome,
        record.unsent_proof,
        record.outcome_at.is_some(),
    );
    assert_eq!(
        recorded,
        (SendOutcome::EnterRefused, UnsentProof::SendOutcome, true)
    );
    drop(entered);
    drop(registry_guard);
    drop(root_guard);
}
