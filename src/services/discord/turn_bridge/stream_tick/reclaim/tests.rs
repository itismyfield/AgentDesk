use super::{ReclaimDeliveryState, reclaim_for_missing_watcher};
use crate::services::discord::inflight::{
    InflightTurnState, RelayOwnerKind, StreamRelayAuthority, inflight_runtime_root,
    inflight_state_path, load_inflight_state, save_inflight_state,
};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

const CONFIRMED: &str = "확인된 본문 / ";
const BODY: &str = "확인된 본문 / watcher가 전달했다고 추정한 꼬리";

fn with_runtime_root(test: impl FnOnce(&Path)) {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().expect("isolated runtime root");
    let _reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    test(root.path());
}

fn watcher_state(channel_id: u64) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        channel_id,
        None,
        7,
        77_010,
        18,
        "누락된 watcher 재시도".to_owned(),
        Some("reclaim-session".to_owned()),
        Some("reclaim-watcher".to_owned()),
        Some("/tmp/reclaim-watcher-output".to_owned()),
        None,
        512,
    );
    state.turn_nonce = Some("reclaim-caller-owner-cas".to_owned());
    state.full_response = BODY.to_owned();
    state.response_sent_offset = BODY.len();
    state.set_relay_owner_kind(RelayOwnerKind::Watcher);
    state
}

fn state_path(state: &InflightTurnState) -> PathBuf {
    inflight_state_path(
        &inflight_runtime_root().expect("isolated inflight root"),
        &ProviderKind::Claude,
        state.channel_id,
    )
}

struct Attempt {
    state: InflightTurnState,
    sent: usize,
    watcher_owns: bool,
    watcher_available: bool,
}

impl Attempt {
    fn new(state: InflightTurnState) -> Self {
        Self {
            sent: state.response_sent_offset,
            state,
            watcher_owns: true,
            watcher_available: true,
        }
    }

    fn reclaim(&mut self, confirmed: usize) -> ControlFlow<(), bool> {
        reclaim_for_missing_watcher(
            ChannelId::new(self.state.channel_id),
            false,
            ReclaimDeliveryState {
                full_response: BODY,
                bridge_confirmed_response_sent_offset: confirmed,
                response_sent_offset: &mut self.sent,
                inflight_state: &mut self.state,
                watcher_owns_assistant_relay: &mut self.watcher_owns,
                watcher_relay_available_for_turn: &mut self.watcher_available,
                standby_relay_owns_output: false,
            },
        )
    }
}

#[test]
fn missing_watcher_caller_rewinds_the_matching_watcher_owner() {
    with_runtime_root(|_| {
        let state = watcher_state(6_249_901);
        save_inflight_state(&state).expect("seed watcher-owned row");
        let mut attempt = Attempt::new(state);

        assert_eq!(
            attempt.reclaim(CONFIRMED.len()),
            ControlFlow::Continue(true)
        );
        assert_eq!(attempt.sent, CONFIRMED.len());
        assert_eq!(attempt.state.response_sent_offset, CONFIRMED.len());
        assert_eq!(attempt.state.full_response, BODY);
        assert_eq!(
            attempt.state.effective_relay_owner_kind(),
            RelayOwnerKind::None
        );
        assert!(!attempt.watcher_owns);
        assert!(!attempt.watcher_available);

        let durable = load_inflight_state(&ProviderKind::Claude, attempt.state.channel_id)
            .expect("rewound row");
        assert_eq!(durable.response_sent_offset, CONFIRMED.len());
        assert_eq!(durable.full_response, BODY);
        assert_eq!(durable.effective_relay_owner_kind(), RelayOwnerKind::None);
    });
}

#[test]
fn missing_watcher_caller_rejects_changed_and_already_reclaimed_owners() {
    with_runtime_root(|_| {
        for (channel, owner) in [
            (6_249_902, RelayOwnerKind::StandbyRelay),
            (6_249_903, RelayOwnerKind::None),
        ] {
            let stale = watcher_state(channel);
            save_inflight_state(&stale).expect("seed watcher-owned row");
            let mut current = stale.clone();
            current.set_relay_owner_kind(owner);
            current.full_response = format!("{BODY} / 새 owner가 저장한 응답과 전달 범위");
            current.response_sent_offset = current.full_response.len();
            save_inflight_state(&current).expect("replace durable owner");
            let installed = load_inflight_state(&ProviderKind::Claude, current.channel_id)
                .expect("third owner was installed");
            assert_eq!(
                StreamRelayAuthority::from_state(&installed),
                StreamRelayAuthority::from_state(&current)
            );
            assert_eq!(installed.full_response, current.full_response);
            assert_eq!(installed.response_sent_offset, current.response_sent_offset);
            let path = state_path(&current);
            let before = std::fs::read(&path).expect("durable bytes before reclaim");
            let mut attempt = Attempt::new(stale);

            assert_eq!(attempt.reclaim(CONFIRMED.len()), ControlFlow::Break(()));
            assert_eq!(std::fs::read(&path).expect("durable row survives"), before);
            assert_eq!(attempt.sent, current.response_sent_offset);
            assert_eq!(
                attempt.state.response_sent_offset,
                current.response_sent_offset
            );
            assert_eq!(attempt.state.full_response, current.full_response);
            assert_eq!(
                StreamRelayAuthority::from_state(&attempt.state),
                StreamRelayAuthority::from_state(&current)
            );
            assert!(
                attempt.watcher_owns,
                "rejection keeps body-send suppression"
            );
            assert!(attempt.watcher_available);
        }
    });
}

#[test]
fn missing_watcher_caller_with_no_rewind_does_not_write_w4() {
    with_runtime_root(|_| {
        let mut state = watcher_state(6_249_904);
        state.response_sent_offset = CONFIRMED.len();
        save_inflight_state(&state).expect("seed confirmed watcher-owned row");
        let path = state_path(&state);
        let before = std::fs::read(&path).expect("durable bytes before no-op rewind");
        let mut attempt = Attempt::new(state);

        assert_eq!(
            attempt.reclaim(CONFIRMED.len()),
            ControlFlow::Continue(true)
        );
        assert_eq!(std::fs::read(&path).expect("durable row survives"), before);
        assert_eq!(attempt.sent, CONFIRMED.len());
        assert_eq!(
            attempt.state.effective_relay_owner_kind(),
            RelayOwnerKind::None
        );
        assert!(!attempt.watcher_owns);
        assert!(!attempt.watcher_available);
    });
}

#[test]
fn missing_watcher_caller_preserves_local_rewind_on_io_error() {
    with_runtime_root(|root| {
        let blocker = root.join("blocked-root");
        std::fs::write(&blocker, b"not a directory").expect("block persistence root");
        let _blocked_root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            &blocker,
        );
        let mut attempt = Attempt::new(watcher_state(6_249_905));

        assert_eq!(
            attempt.reclaim(CONFIRMED.len()),
            ControlFlow::Continue(true)
        );
        assert_eq!(attempt.sent, CONFIRMED.len());
        assert_eq!(attempt.state.response_sent_offset, CONFIRMED.len());
        assert_eq!(attempt.state.full_response, BODY);
        assert_eq!(
            attempt.state.effective_relay_owner_kind(),
            RelayOwnerKind::None
        );
        assert!(!attempt.watcher_owns);
        assert!(!attempt.watcher_available);
        assert_eq!(
            std::fs::read(blocker).expect("blocker survives"),
            b"not a directory"
        );
    });
}
