use super::kickoff_identity::*;
use super::*;

fn reacquired_row(channel: u64) -> inflight::InflightTurnState {
    crate::services::discord::tmux::tmux_watcher::liveness::build_watcher_reacquire_inflight_state(
        super::tests::recovery_state(ProviderKind::Claude, channel),
    )
}

#[test]
fn ownerless_reacquired_row_has_no_kickoff_identity() {
    let state = reacquired_row(6_029_001);
    assert_eq!((state.request_owner_user_id, state.user_msg_id), (0, 0));
    assert!(recovery_kickoff_identity(&state).is_none());
}

#[test]
fn owned_row_kickoff_identity_matches_persisted_ids() {
    for user_msg_id in [2, 0] {
        let mut state = super::tests::recovery_state(ProviderKind::Claude, 6_029_002);
        state.user_msg_id = user_msg_id;
        let identity = recovery_kickoff_identity(&state).expect("owned row kicks off");
        assert_eq!(
            identity.request_owner,
            UserId::new(state.request_owner_user_id)
        );
        assert_eq!(identity.user_message_id, optional_message_id(user_msg_id));
    }
}
