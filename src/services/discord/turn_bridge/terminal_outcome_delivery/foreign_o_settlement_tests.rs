use super::*;
use crate::services::discord::outbound::{
    completed_turn_ledger, o_delivery_evidence::tests::Fixture,
};
use crate::services::discord::recovery_engine::o_settlement_tests::{arm_takeover, reset_takeover};

#[tokio::test]
async fn i7441b_custody_o_capture_initial_and_late_takeover_uses_original_input() {
    for late in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let f = Fixture::new(
            744187 + u64::from(late),
            ProviderKind::Claude,
            &["custody answer"],
        )
        .await;
        let (http, _guard) = f.http().await;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let selected = [(
            f.row.channel_id,
            crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        )];
        let _o = crate::services::tui_o::cutover::test_override::force_channels(if late {
            &[]
        } else {
            &selected
        });
        let mut b = f.row.clone();
        b.user_msg_id += 99;
        b.turn_nonce = Some("current B".into());
        crate::services::discord::inflight::save_inflight_state(&b).unwrap();
        let outcome = preserve_or_publish(Handoff {
            provider: &ProviderKind::Claude,
            local: &f.row,
            admitted: None,
            content: &f.body,
            delivery_body: None,
            empty_recovery_notice: None,
            response_sent_offset: 0,
            channel_id: ChannelId::new(f.row.channel_id),
            old_anchor: MessageId::new(f.row.current_msg_id),
            watcher_owner_channel_id: ChannelId::new(f.row.channel_id),
            tmux_last_offset: Some(f.row.last_offset),
            cancelled: false,
            cancel_source: String::new(),
            children: &[],
            dispatch_id: None,
            adk_cwd: None,
            should_complete: false,
            should_fail: false,
            resume_failure: false,
            recovery_retry: false,
        })
        .await;
        assert!(matches!(outcome, Outcome::Deferred { .. }));
        if late {
            arm_takeover(
                f.row.channel_id,
                crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            );
        }
        let n = crate::services::discord::terminal_delivery_custody::drain_for_test(
            |mut payload, checkpoint| {
                let gateway = crate::services::discord::gateway::DiscordGateway::new(
                    http.clone(),
                    shared.clone(),
                    ProviderKind::Claude,
                    None,
                );
                let shared = shared.clone();
                async move {
                    let result =
                        resume_payload_with_gateway(&shared, &gateway, &mut payload, &checkpoint)
                            .await;
                    (payload, result)
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(n, 1, "custody must retire under O ownership");
        completed_turn_ledger::o_pending::project(&shared, &ProviderKind::Claude, f.row.channel_id)
            .await;
        let ids =
            completed_turn_ledger::settled_user_msg_ids(&ProviderKind::Claude, f.row.channel_id);
        assert!(ids.contains(&f.row.user_msg_id));
        assert!(!ids.contains(&b.user_msg_id));
        assert_eq!(
            crate::services::discord::inflight::load_inflight_state(
                &ProviderKind::Claude,
                f.row.channel_id
            )
            .unwrap()
            .turn_nonce,
            b.turn_nonce
        );
        reset_takeover();
    }
}
