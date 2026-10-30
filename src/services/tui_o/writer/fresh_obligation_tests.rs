//! Durable writer obligations prevent an otherwise valid fresh source selection.

use super::*;
use crate::services::tui_o::store::ledger::LocalRetryMark;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Obligation {
    None,
    LocalRetry,
    ApprovedResume,
    Unresolved,
}

#[tokio::test(start_paused = true)]
async fn durable_retry_approval_and_open_prepared_hold_a_valid_fresh_scope() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::durable_retry_approval_and_open_prepared_hold_a_valid_fresh_scope"
    )) {
        return;
    }
    let _environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = DedupeGuard::acquire();
    for obligation in [
        Obligation::None,
        Obligation::LocalRetry,
        Obligation::ApprovedResume,
        Obligation::Unresolved,
    ] {
        let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
        let _scope = ScopeFixture::new();
        let _tmux =
            crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
        dedupe::reset_state_for_tests();
        dedupe::binding_events::set_test_root(Some(root.path()));
        let (_, old) = proven(root.path());
        let harness = Harness::build_channel(CANARY_CHANNEL, |_| {
            vec![InitSource {
                source_id: old.clone(),
                delivery_start: std::fs::metadata(&old.path).unwrap().len(),
                prefix_hash: hex::encode(Sha256::digest(std::fs::read(&old.path).unwrap())),
            }]
        });
        let mut store = harness.channel();
        let checkpoint = dedupe::binding_events::binding_events_since(CANARY_CHANNEL, 0)
            .unwrap()
            .last()
            .unwrap()
            .seq;
        store.set_binding_checkpoint(checkpoint).unwrap();
        if obligation != Obligation::None {
            let mut key = unit("retained obligation");
            key.channel_id = CANARY_CHANNEL;
            key.provider = ShadowProvider::Codex;
            store
                .append_ledger(LedgerEntry::Prepared {
                    notice_origin: None,
                    serial: 0,
                    unit_key: key,
                    piece_index: 0,
                    payload: "retained payload".into(),
                    anchor_id: 100,
                    epoch: 1,
                    local_retry: (obligation == Obligation::LocalRetry)
                        .then_some(LocalRetryMark::Budget),
                })
                .unwrap();
            match obligation {
                Obligation::LocalRetry => store
                    .append_ledger(LedgerEntry::NotFound { serial: 0 })
                    .unwrap(),
                Obligation::ApprovedResume => store
                    .append_ledger(LedgerEntry::Rejected {
                        serial: 0,
                        status: 403,
                    })
                    .unwrap(),
                Obligation::Unresolved => {}
                Obligation::None => unreachable!(),
            }
        }
        drop(store);
        if obligation == Obligation::ApprovedResume {
            harness
                .store
                .record_operator_resume(CANARY_CHANNEL, 0, "operator", "retained approval")
                .unwrap();
        }
        let old_cursor = harness.channel().cursor(&old).unwrap().clone();
        let old_bytes = std::fs::read(&old.path).unwrap();
        let io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&old.path);
        let (current_context, current) = proven(root.path());
        answer(&current, "fresh current answer");
        assert!(dedupe::clear_relaunch::fresh_scope_eligible(CANARY_CHANNEL));
        let mut could_install = false;
        assert_eq!(
            dedupe::clear_relaunch::with_fresh_scope(
                CANARY_CHANNEL,
                None,
                std::slice::from_ref(&old),
                true,
                |_, _| could_install = true,
            ),
            Ok(true),
            "{obligation:?}: exact proof must otherwise admit selection"
        );
        assert!(could_install);
        let mut writer = harness.writer();
        assert_eq!(
            writer.has_local_retry(),
            obligation == Obligation::LocalRetry
        );
        assert_eq!(
            !writer.resume_pieces().is_empty(),
            obligation == Obligation::ApprovedResume
        );
        assert_eq!(
            writer.store().ledger().unresolved().is_some(),
            obligation == Obligation::Unresolved
        );
        let selection = writer.suspension.clone();
        let ledger_path = harness.port.ledger.lock().unwrap().clone().unwrap();
        let ledger_before = std::fs::read(&ledger_path).unwrap();
        harness.gate.acquired();
        let (stop, stopped) = watch::channel(false);
        let (resumed, ready) = watch::channel(false);
        let actor = tokio::spawn(run_channel(
            writer,
            ShadowProvider::Codex,
            Arc::new(ChannelBindingLog::new(
                CANARY_CHANNEL,
                ShadowProvider::Codex,
            )),
            stopped,
            resumed,
        ));
        polls(6).await;
        let live = !actor.is_finished();
        let ready = *ready.borrow();
        let posts = harness.port.posts();
        stop.send(true).unwrap();
        assert_eq!(actor.await.unwrap(), None, "{obligation:?}");
        assert!(live, "{obligation:?}: actor must hold rather than stop");
        if obligation == Obligation::None {
            assert!(ready);
            assert_eq!(posts, ["fresh current answer", "-# ✅ 완료"]);
            assert_eq!(selection.nonce(), Some(current_context.execution_nonce));
            assert!(selection.contains(&old));
        } else {
            assert!(!ready, "{obligation:?}: unresolved work cannot resume");
            assert!(posts.is_empty(), "{obligation:?}: {posts:?}");
            assert!(selection.is_empty(), "{obligation:?}: actor refroze work");
            assert_eq!(selection.nonce(), None);
            assert_eq!(std::fs::read(&ledger_path).unwrap(), ledger_before);
            assert_eq!(
                harness.channel().ledger().piece(0).unwrap().payload,
                "retained payload"
            );
        }
        assert_eq!(harness.channel().cursor(&old), Some(&old_cursor));
        assert_eq!(std::fs::read(&old.path).unwrap(), old_bytes);
        assert_eq!(io.stats(), Default::default(), "{obligation:?}");
        dedupe::binding_events::forget_channel_for_tests(CANARY_CHANNEL);
        dedupe::binding_events::set_test_root(None);
        dedupe::reset_state_for_tests();
    }
}
