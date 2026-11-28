use super::tests::dispatch_process_handoff;
use super::*;
use crate::services::discord::inflight::{GuardedSaveOutcome, load_inflight_state};
use crate::services::discord::shared_state::test_rest;
use std::sync::atomic::AtomicBool;

/// The actual RuntimeReady dispatch, using the fixture REST client and gateway seam. It must
/// replace this exact paused incumbent and publish its successor's actual adopted claim pin.
pub(in crate::services::discord) async fn adopt_codex_replacement_for_test(
    shared: &Arc<SharedData>,
    state: &mut InflightTurnState,
    handoff: RuntimeHandoff,
    incumbent_cancel: &Arc<AtomicBool>,
    http: Arc<serenity::Http>,
) -> WatcherClaimIncarnation {
    let channel = ChannelId::new(state.channel_id);
    let RuntimeHandoff::CodexTui {
        ref tmux_session_name,
        ref rollout_path,
        ..
    } = handoff
    else {
        panic!("fixture must use the actual Codex RuntimeReady frame");
    };
    let (tmux, output) = (tmux_session_name.clone(), rollout_path.clone());
    let old = shared
        .tmux_watchers
        .get(&channel)
        .expect("preserved paused incumbent");
    assert!(Arc::ptr_eq(&old.cancel, incumbent_cancel));
    assert!(old.paused.load(Ordering::Acquire));
    assert!(!old.cancel.load(Ordering::Acquire));
    let old_delivery = old.turn_delivered.clone();
    drop(old);
    let before = load_inflight_state(&ProviderKind::Codex, state.channel_id)
        .expect("same managed row exists before RuntimeReady");
    assert_eq!(before.user_msg_id, state.user_msg_id);
    assert_eq!(before.turn_nonce, state.turn_nonce);

    let _rest = test_rest::install(http);
    let _gateway = super::test_gateway::connect();
    let mut dirty = false;
    let observed = dispatch_process_handoff(
        shared,
        &ProviderKind::Codex,
        state,
        RuntimeHandoffLoopMessage::RuntimeReady { handoff },
        &mut dirty,
        false,
    )
    .await;
    assert_eq!(observed.outcome, Some(GuardedSaveOutcome::Saved));
    assert!(observed.retry_message.is_none());
    assert_eq!(observed.claim_outcome, WatcherHandoffClaimOutcome::Spawned);
    assert!(observed.terminal_control_ready_observed);
    assert!(observed.tmux_handed_off && observed.watcher_relay_available);
    assert_eq!(observed.watcher_owner_channel_id, channel);
    assert_eq!(observed.watcher_slots, 1);
    let pin = observed
        .watcher_delivery_pin
        .expect("RuntimeReady adopted its successor incarnation");
    assert!(!Arc::ptr_eq(&pin.cancel, incumbent_cancel));
    assert!(!Arc::ptr_eq(&pin.turn_delivered, &old_delivery));
    assert!(
        incumbent_cancel.load(Ordering::Acquire),
        "actual claim cancelled the old watcher"
    );
    assert!(!pin.cancel.load(Ordering::Acquire));
    assert!(
        !pin.paused.load(Ordering::Acquire),
        "actual adoption unpaused the successor"
    );
    let successor = shared
        .tmux_watchers
        .get(&channel)
        .expect("adopted successor registered");
    assert_eq!(successor.tmux_session_name, tmux);
    assert_eq!(successor.output_path, output);
    assert!(Arc::ptr_eq(&successor.cancel, &pin.cancel));
    assert!(Arc::ptr_eq(&successor.turn_delivered, &pin.turn_delivered));
    drop(successor);
    let after = load_inflight_state(&ProviderKind::Codex, state.channel_id)
        .expect("runtime stamp preserved the managed turn");
    assert_eq!(after.user_msg_id, before.user_msg_id);
    assert_eq!(after.turn_nonce, before.turn_nonce);
    assert_eq!(after.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
    pin
}
