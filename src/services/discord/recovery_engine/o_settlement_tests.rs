use super::*;
use crate::services::discord::catch_up::classification_order_tests::i7441_tests::assert_aged_delivery_settled;
use crate::services::discord::outbound::{
    completed_turn_ledger, o_delivery_evidence::tests::Fixture,
};
use crate::services::session_host::{
    HostLiveness, HostPresence, HostSessionRef,
    test_support::{InjectedLivenessGuard, InjectedPresenceGuard},
};
use crate::services::tui_o::cutover::test_override::force_channels;

thread_local! {
    static BEFORE_CLAIM: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static TAKEOVER: std::cell::RefCell<Option<crate::services::tui_o::cutover::test_override::ChannelsGuard>> = const { std::cell::RefCell::new(None) };
}

pub(in crate::services::discord) fn before_claim() {
    BEFORE_CLAIM.with(|slot| {
        if let Some(run) = slot.borrow_mut().take() {
            run();
        }
    });
}

pub(in crate::services::discord) fn arm_takeover(
    channel: u64,
    kind: crate::services::agent_protocol::RuntimeHandoffKind,
) {
    BEFORE_CLAIM.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            TAKEOVER.with(|held| *held.borrow_mut() = Some(force_channels(&[(channel, kind)])));
        }))
    });
}
pub(in crate::services::discord) fn reset_takeover() {
    TAKEOVER.with(|held| drop(held.borrow_mut().take()));
}

#[tokio::test]
async fn i7441b_restore_marker_three_completion_paths_then_aged_scan_pg() {
    for mode in 0..3 {
        for variant in 0..5 {
            let root = tempfile::tempdir().unwrap();
            let _root = crate::config::set_agentdesk_root_for_test(root.path());
            let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
                "i7441b_restore",
                "O marker recovery",
            )
            .await;
            let pool = db.connect_and_migrate().await;
            let covered = variant == 0;
            let f = Fixture::new(
                744140 + mode * 10 + variant,
                ProviderKind::Claude,
                &["restored prefix. ", "tail"],
            )
            .await;
            let (http, _http) = f.http().await;
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
                pool.clone(),
            ));
            {
                let mut settings = shared.settings.write().await;
                settings.allow_all_users = true;
                settings.owner_user_id = Some(7);
            }
            let _o = force_channels(&[(
                f.row.channel_id,
                crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            )]);
            let mut row = f.row.clone();
            row.last_offset = 0;
            if mode == 2 {
                row.full_response.clear();
            }
            match variant {
                1 => f.port.messages.lock().unwrap().clear(),
                2 => {
                    f.port.messages.lock().unwrap().remove(0);
                }
                3 => row.user_text = "another episode".into(),
                4 => row.tui_terminal_source_file_identity = Some((f.source.dev, f.source.ino + 1)),
                _ => {}
            }
            use std::io::Write;
            let terminal = serde_json::json!({"type":"result","subtype":"success","is_error":false,"result":f.body,"session_id":"native-session"});
            writeln!(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&f.source.path)
                    .unwrap(),
                "{terminal}"
            )
            .unwrap();
            inflight::save_inflight_state(&row).unwrap();
            if mode == 0 {
                let context = restart_report::RestartReportContext::from_bridge(
                    ProviderKind::Claude,
                    row.channel_id,
                    None,
                    None,
                );
                restart_report::announce_restart(&context).unwrap();
            }
            let session = HostSessionRef::tmux(row.tmux_session_name.as_ref().unwrap());
            let _live = InjectedLivenessGuard::set(session.clone(), HostLiveness::Live);
            let _present = InjectedPresenceGuard::set(session, HostPresence::Present);
            super::restore_inflight::restore_inflight_states(
                &http,
                &shared,
                &ProviderKind::Claude,
                vec![row.clone()],
            )
            .await;
            assert!(
                inflight::load_inflight_state(&ProviderKind::Claude, row.channel_id).is_none(),
                "restore mode {mode} must clear its captured row"
            );
            assert!(
                completed_turn_ledger::read_ledger(&ProviderKind::Claude, row.channel_id).is_none(),
                "marker is not whole-body proof"
            );
            if covered {
                let pending = crate::services::discord::runtime_store::runtime_root()
                    .unwrap()
                    .join("discord_pending_o_deliveries/claude")
                    .join(format!("{}.json", row.channel_id));
                eprintln!(
                    "RESTORE mode={mode} variant={variant} captured={}",
                    std::fs::read_to_string(pending).unwrap_or_else(|e| e.to_string())
                );
                assert_aged_delivery_settled(
                    root.path(),
                    &shared,
                    &ProviderKind::Claude,
                    ChannelId::new(row.channel_id),
                    MessageId::new(row.user_msg_id),
                )
                .await;
            } else {
                completed_turn_ledger::o_pending::project(
                    &shared,
                    &ProviderKind::Claude,
                    row.channel_id,
                )
                .await;
                assert!(
                    completed_turn_ledger::read_ledger(&ProviderKind::Claude, row.channel_id)
                        .is_none()
                );
                crate::services::discord::catch_up::classification_order_tests::i7441_tests::assert_aged_o_unsettled(root.path(),&shared,ChannelId::new(row.channel_id),MessageId::new(row.user_msg_id)).await;
            }
            pool.close().await;
            db.drop().await;
        }
    }
}

#[tokio::test]
async fn i7441b_recovery_early_o_return_preserves_context_without_marker_receipt() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    for late in [false, true] {
        let f = Fixture::new(744185 + u64::from(late), ProviderKind::Claude, &["body"]).await;
        let (http, _guard) = f.http().await;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let mut recovered = f.row.clone();
        if late {
            recovered.tui_terminal_source_file_identity = None;
            recovered.current_msg_id = 0;
        }
        let selected = [(
            f.row.channel_id,
            crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        )];
        let _o = force_channels(if late { &[] } else { &selected });
        if late {
            BEFORE_CLAIM.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    TAKEOVER.with(|held| *held.borrow_mut() = Some(force_channels(&selected)));
                }))
            });
        }

        inflight::save_inflight_state(&recovered).unwrap();
        let captured = super::completion_delivery::relay_captured_recovery_terminal_notice(
            &http,
            &shared,
            &ProviderKind::Claude,
            &recovered,
            &f.body,
        )
        .await;
        assert_eq!(captured.outcome, RecoveryRelayOutcome::Delivered);
        assert!(
            completed_turn_ledger::read_ledger(&ProviderKind::Claude, f.row.channel_id).is_none()
        );
        let _ = inflight::clear_inflight_state_for_captured_episode(
            &ProviderKind::Claude,
            f.row.channel_id,
            &inflight::InflightTurnIdentity::from_state(&recovered),
            f.row.turn_nonce.as_deref(),
        );
        completed_turn_ledger::o_pending::project(&shared, &ProviderKind::Claude, f.row.channel_id)
            .await;
        assert!(
            completed_turn_ledger::settled_user_msg_ids(&ProviderKind::Claude, f.row.channel_id)
                .contains(&f.row.user_msg_id)
        );
        TAKEOVER.with(|held| drop(held.borrow_mut().take()));
    }
}
