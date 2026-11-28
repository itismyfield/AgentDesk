use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::discord::inflight::is_synthetic_create_state;
use crate::services::discord::inflight::save_store::create_monotonic_observer::{
    fixture::{SERIAL, state, write_bytes},
    test_seams,
};
use crate::services::discord::inflight::save_store::{
    save_inflight_state_create_new_in_root, save_inflight_state_if_absent_in_root,
};
use crate::services::provider::ProviderKind;
#[test]
fn create_paths_observe_real_but_exclude_all_synthetic_shapes() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("turn.jsonl");
    write_bytes(&output, b'a', 512);
    let real = state(54_900_003, &output, 256);
    test_seams::clear(ProviderKind::Codex, real.channel_id);
    save_inflight_state_create_new_in_root(temp.path(), &real).unwrap();
    assert_eq!(
        test_seams::offset(ProviderKind::Codex, real.channel_id),
        Some(256)
    );

    let adopted = crate::services::discord::recovery_engine::manual_rebind::build_external_adopted_inflight_state(
        state(54_900_011, &output, 128),
    );
    assert!(is_synthetic_create_state(&adopted));
    test_seams::clear(ProviderKind::Codex, adopted.channel_id);
    save_inflight_state_create_new_in_root(temp.path(), &adopted).unwrap();
    assert_eq!(
        test_seams::offset(ProviderKind::Codex, adopted.channel_id),
        None
    );

    let lease =
        crate::services::tui_prompt_dedupe::ExternalInputRelayLease::unassigned(Some(54_900_006));
    let tui = crate::services::discord::tui_prompt_relay::synthetic_start::build_tui_direct_synthetic_inflight_state(
        ProviderKind::Codex,
        crate::services::discord::ChannelId::new(54_900_006),
        crate::services::discord::MessageId::new(54_900_106),
        None,
        "observer prompt",
        "AgentDesk-codex-observer-5490",
        Some(&output),
        128,
        &lease,
        RelayOwnerKind::Watcher,
    );
    assert!(is_synthetic_create_state(&tui));
    test_seams::clear(ProviderKind::Codex, tui.channel_id);
    assert!(save_inflight_state_if_absent_in_root(temp.path(), &tui).unwrap());
    assert_eq!(
        test_seams::offset(ProviderKind::Codex, tui.channel_id),
        None
    );

    #[cfg(unix)]
    {
        let monitor = crate::services::discord::tmux::build_monitor_triggered_inflight_state(
            state(54_900_004, &output, 128),
        );
        assert!(is_synthetic_create_state(&monitor));
        test_seams::clear(ProviderKind::Codex, monitor.channel_id);
        save_inflight_state_create_new_in_root(temp.path(), &monitor).unwrap();
        assert_eq!(
            test_seams::offset(ProviderKind::Codex, monitor.channel_id),
            None
        );

        let watcher = crate::services::discord::tmux::tmux_watcher::liveness::build_watcher_reacquire_inflight_state(
            state(54_900_005, &output, 128),
        );
        assert!(is_synthetic_create_state(&watcher));
        test_seams::clear(ProviderKind::Codex, watcher.channel_id);
        assert!(save_inflight_state_if_absent_in_root(temp.path(), &watcher).unwrap());
        assert_eq!(
            test_seams::offset(ProviderKind::Codex, watcher.channel_id),
            None
        );
    }

    let real_if_absent = state(54_900_007, &output, 192);
    assert!(!is_synthetic_create_state(&real));
    assert!(!is_synthetic_create_state(&real_if_absent));
    assert!(save_inflight_state_if_absent_in_root(temp.path(), &real_if_absent).unwrap());
    assert_eq!(
        test_seams::offset(ProviderKind::Codex, real_if_absent.channel_id),
        Some(192)
    );
}
