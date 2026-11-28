use super::*;
use crate::services::discord::outbound::{
    completed_turn_ledger, o_delivery_evidence::tests::Fixture,
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
