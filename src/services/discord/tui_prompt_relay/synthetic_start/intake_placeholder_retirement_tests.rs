use super::*;

#[test]
fn synthetic_inflight_tracks_the_actual_processing_placeholder_byte_length() {
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        let lease = ExternalInputRelayLease::unassigned(Some(42));
        let state = build_tui_direct_synthetic_inflight_state(
            provider,
            ChannelId::new(42),
            MessageId::new(101),
            Some(MessageId::new(202)),
            "typed in TUI",
            "fixture-placeholder",
            None,
            17,
            &lease,
            RelayOwnerKind::Watcher,
        );
        let placeholder = crate::services::discord::formatting::build_processing_status_block("⠋");
        assert_eq!(placeholder, "⠋ 계속 처리 중");
        assert_eq!(state.current_msg_len, placeholder.len());
        assert_eq!(state.current_msg_id, 202);
        assert_eq!(state.user_msg_id, 101);
        assert_eq!(state.turn_start_offset, Some(17));
    }
}
