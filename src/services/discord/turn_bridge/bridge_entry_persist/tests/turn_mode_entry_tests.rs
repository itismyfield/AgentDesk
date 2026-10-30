//! A turn-mode TUI turn enters without a response anchor; off turn mode, or for a runtime whose
//! body O does not post, the entry still materializes one.
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::CodexTui;
use crate::services::discord::turn_bridge::stream_tick::provider_output_guard_tests::CapturingGateway;

/// Runs the real entry once and returns how many messages it sent and the durable anchor after.
async fn entry(channel_id: u64, turn_mode: bool, tui: bool) -> (usize, u64) {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let owned = [(channel_id, CodexTui)];
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&owned);
    let _turn =
        turn_mode.then(|| crate::services::tui_o::turn_mode::TestConfirmation::new(channel_id));
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mut row = notice_row(channel_id, channel_id + 1, "turn-mode entry");
    row.current_msg_id = 0;
    row.runtime_kind = tui.then_some(CodexTui);
    row.turn_nonce = Some(format!("entry-{channel_id}"));
    crate::services::discord::inflight::save_inflight_state(&row).expect("seed the row");
    let gateway = std::sync::Arc::new(CapturingGateway {
        direct: true,
        ..Default::default()
    });
    let mut bridge = bridge_with(row.clone(), &gateway);
    let mut durable = row.clone();
    let harness = ReconcileHarness::new(&mut durable, ChannelId::new(channel_id));
    *harness.runtime.current_msg_id = detached_current_msg_id_from_durable(0);
    let (mut rowless, mut created, mut last_edit, mut cleared) =
        (false, None, String::new(), false);
    let ctx = BridgeEntryAuthorityContext {
        entry_was_rowless: &mut rowless,
        bridge: &mut bridge,
        shared: &shared,
        bridge_created_placeholder: &mut created,
        last_edit_text: &mut last_edit,
        resumed_placeholder_clear_applied: &mut cleared,
        captured_nonce: row.turn_nonce.as_deref(),
    };
    assert!(
        establish_bridge_entry_authority(ctx, harness.runtime, "⠋ 계속 처리 중")
            .await
            .is_ok(),
        "turn_mode={turn_mode} tui={tui}: the entry proceeds"
    );
    let sends = gateway.sends.lock().expect("sends lock").len();
    let stored =
        crate::services::discord::inflight::load_inflight_state(&ProviderKind::Codex, channel_id)
            .expect("the row stays")
            .current_msg_id;
    (sends, stored)
}

#[tokio::test(flavor = "current_thread")]
async fn a_turn_mode_tui_entry_materializes_no_anchor() {
    assert_eq!(entry(4_259_901, true, true).await, (0, 0));
}

#[tokio::test(flavor = "current_thread")]
async fn other_entries_still_materialize_their_anchor() {
    for (channel_id, turn_mode, tui) in [(4_259_902, true, false), (4_259_903, false, true)] {
        let (sends, stored) = entry(channel_id, turn_mode, tui).await;
        assert_eq!(sends, 1, "turn_mode={turn_mode} tui={tui}");
        assert_ne!(stored, 0, "turn_mode={turn_mode} tui={tui}: bound");
    }
}
