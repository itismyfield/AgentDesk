use super::*;
use crate::services::claude_tui::busy_inject::Screen;
use crate::services::claude_tui::host_input::busy::BusyState;
use crate::services::claude_tui::readiness;
use crate::services::discord::queue_io::input_owner::held_watch::{self, Verdict};
use crate::services::discord::router::message_handler::provider_dispatch as dispatch;
use crate::services::tui_turn_state::idle_compaction_tests::{end_turn, tail};

#[test]
fn an_idle_compaction_tail_wakes_and_delivers_the_actual_held_front_pg() {
    let fx = Fixture::with_root(
        "idle-compaction-held-front",
        None,
        crate::config::TestRuntimeRootGuard::new(),
    );
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    assert_eq!(before, 2);
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = crate::services::discord::router::intake_gate::busy_inject::test_support::open_gate(
        CHANNEL,
    );
    let intake = Intake::with_expected_pin(&fx, &started, ExpectedPin::Registered);
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
    assert!(readiness::observe(fx.logical(), Some(&started.path)).is_none());
    let heads = late_tests::write_on_enter(&fx, &intake, &started.session, &started.path, false);
    intake.runtime.block_on(async {
        let counts = dispatch::OwnerTurn::probes(CHANNEL);
        let front = intake.front().await;
        let original = front.1.clone();
        let result = tokio::time::timeout(
            LIMIT,
            dispatch::start_idle_for_test(intake.shared.clone(), ChannelId::new(CHANNEL), front),
        )
        .await
        .expect("the actual busy admission completes its hold");
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

        let mut end = end_turn();
        end["sessionId"] = json!(started.session);
        end["timestamp"] = json!(chrono::Utc::now().to_rfc3339());
        append(&started.path, &[end]);
        let records: Vec<_> = tail()
            .into_iter()
            .map(|mut record| {
                record["sessionId"] = json!(started.session);
                record
            })
            .collect();
        append(&started.path, &records);
        let key = discord::adk_session::turn_session_key(
            &intake.shared,
            ChannelId::new(CHANNEL),
            &ProviderKind::Claude,
        )
        .await
        .unwrap();
        let hosted = crate::services::turn_host::for_turn(
            Some(&fx.pool),
            &ProviderKind::Claude,
            CHANNEL,
            Some(&key),
        )
        .await;
        let observation =
            claude_handoff::observe_busy(CHANNEL, fx.logical(), &hosted, &intake.shared.token_hash);
        assert_eq!(observation.state(), BusyState::Idle);
        assert!(matches!(observation.probe()(), Screen::Ready));
        assert!(
            readiness::observe(fx.logical(), Some(&started.path)).is_some(),
            "the same completed source has its public readiness proof despite the metadata tail"
        );
        held_watch::arm(CHANNEL, observation.probe());
        let (_, observed) =
            held_watch::watch_observed(source_retry_drive(&intake), original.message_id);
        let look = tokio::time::timeout(LIMIT, observed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(look.verdict, Verdict::Wake);
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
        idle_turn_unwound(&intake).await;
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    });
}
