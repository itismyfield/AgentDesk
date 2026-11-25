use super::*;
use crate::services::claude_tui::readiness;
use crate::services::discord::queue_io::input_owner::held_watch::{self, Verdict};
use crate::services::discord::router::message_handler::provider_dispatch as dispatch;
use crate::services::tui_prompt_dedupe::binding_events;
use crate::services::tui_turn_state::idle_compaction_tests::{compacted_tail, end_turn};
use std::io::Write;

#[test]
fn an_idle_compaction_tail_wakes_and_delivers_the_actual_held_front_pg() {
    let fx = Fixture::with_root(
        "idle-compaction-held-front",
        None,
        crate::config::TestRuntimeRootGuard::new(),
    );
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = reconnect::launch_with(&fx, &launcher, || {
        let started = fx.start_provider(&launcher, true)?;
        assert!(wait_for(&fx.finished, "the first prompt", || fx
            .rig
            .sends()
            .len()
            == 2));
        assert!(!started.path.exists());
        let nonce = launcher.nonces.lock().unwrap()[0].clone();
        let (pin, history) =
            binding_events::claude_history(CHANNEL, fx.logical(), Some(&nonce)).unwrap();
        assert!(
            pin.is_none(),
            "a launch registration alone cannot pin a transcript"
        );
        assert!(history.complete && history.current.is_none());
        assert_eq!(history.awaiting.unwrap().session, started.session);
        fx.answer_with_submit_before_transcript(&started);
        Some(started)
    });
    let before = fx.rig.sends().len();
    assert_eq!(before, 2);
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = crate::services::discord::router::intake_gate::busy_inject::test_support::open_gate(
        CHANNEL,
    );
    let intake = Intake::with_expected_pin(&fx, &started, ExpectedPin::Verified);
    let compaction_tail = || {
        compacted_tail()
            .into_iter()
            .map(|mut record| {
                record["sessionId"] = json!(started.session);
                record
            })
            .collect::<Vec<_>>()
    };
    append(
        &started.path,
        &[
            json!({"type":"user","uuid":uuid(),"sessionId":started.session,
                "timestamp":chrono::Utc::now().to_rfc3339(),
                "message":{"role":"user","content":"work before the idle interval"}}),
            json!({"type":"assistant","uuid":uuid(),"apiBlockIndex":0,
                "sessionId":started.session,"timestamp":chrono::Utc::now().to_rfc3339(),
                "message":{"id":"working-answer","role":"assistant",
                    "content":[{"type":"text","text":"working"}]}}),
        ],
    );
    // Compaction metadata alone leaves this verified source unsettled.
    append(&started.path, &compaction_tail());
    assert!(readiness::observe(fx.logical(), Some(&started.path)).is_none());
    let heads = late_tests::write_on_enter(&fx, &intake, &started.session, &started.path, false);
    intake.runtime.block_on(async {
        let counts = dispatch::OwnerTurn::probes(CHANNEL);
        let front = intake.front().await;
        let original = front.1.clone();
        let (entered, resume) =
            discord::router::intake_dispatch::after_upload_take(ChannelId::new(CHANNEL))
                .await
                .pause();
        let start =
            dispatch::start_idle_for_test(intake.shared.clone(), ChannelId::new(CHANNEL), front);
        let holder = async {
            tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
            intake.install_holder(&fx, &started).await;
            resume.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(tokio::time::timeout(LIMIT, start), holder);
        let result = result.expect("the actual busy admission completes its hold");
        assert!(result.is_err_and(|error| error.starts_with("NotSent(")));
        assert_eq!(fx.rig.sends().len(), before);
        assert!(heads.lock().unwrap().is_empty());
        let (_, observed) =
            held_watch::watch_observed(source_retry_drive(&intake), original.message_id);
        let look = tokio::time::timeout(LIMIT, observed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(look.verdict, Verdict::Keep);
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(true));
        let held = intake
            .mailbox
            .v2_request(|slot| slot.as_ref().ok_or("no owner")?.json())
            .await
            .unwrap();
        assert_eq!(held["items"], json!([original.clone()]));
        assert!(held["head_injection"].is_null());
        assert_eq!(held["retained_inputs"], json!([]));
        let generation = look.generation;
        let (_, observed) =
            held_watch::watch_observed(source_retry_drive(&intake), original.message_id);

        let token = intake.mailbox.cancel_token().await.unwrap().unwrap();
        let persistence = discord::queue_persistence_context(
            &intake.shared,
            &ProviderKind::Claude,
            ChannelId::new(CHANNEL),
        );
        let _ = intake
            .mailbox
            .finish_turn_if_token(Some(token), persistence)
            .await;
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(false));
        assert!(inflight::clear_inflight_state(
            &ProviderKind::Claude,
            CHANNEL
        ));
        let mut end = end_turn();
        end["sessionId"] = json!(started.session);
        end["timestamp"] = json!(chrono::Utc::now().to_rfc3339());
        let completed = std::iter::once(end)
            .chain(compaction_tail())
            .map(|record| format!("{record}\n"))
            .collect::<String>();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&started.path)
            .unwrap()
            .write_all(completed.as_bytes())
            .unwrap();
        let look = tokio::time::timeout(LIMIT, observed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(look.verdict, Verdict::Wake);
        assert_eq!(
            look.generation, generation,
            "the first held probe and its custody wrapper stay installed through recovery"
        );
        source_input_settled(&intake, &original, &heads).await;
        let sends = fx.rig.sends();
        assert_eq!(sends.len() - before, 2, "the same original is sent once");
        assert!(sends[before]["text"].as_str().unwrap().contains(INPUT));
        assert_eq!(sends[before + 1]["keys"], json!(["enter"]));
        let after = dispatch::OwnerTurn::probes(CHANNEL);
        assert_eq!((after.0 - counts.0, after.1 - counts.1), (2, 1));
        {
            let heads = heads.lock().unwrap();
            assert_eq!(heads.len(), 2);
            for head in heads.iter() {
                assert_eq!(head["original"]["message_id"], original.message_id);
                assert_eq!(
                    head["original"]["queued_generation"],
                    original.queued_generation
                );
                assert_eq!(head["attempt"]["target"]["execution_nonce"], nonce);
            }
        }
        idle_turn_unwound(&intake, original.message_id).await;
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    });
}
