//! Admission-order source guard, read from the intake, headless and restore files.

pub(super) fn original_and_restore_source_coverage_preserves_admission_order() {
    for (source, call) in [
        (
            include_str!("../../router/message_handler/intake_turn.rs"),
            "register_or_requeue(",
        ),
        (
            include_str!("../../router/message_handler/headless_turn.rs"),
            "register_headless_original(",
        ),
    ] {
        let register = source.find(call).unwrap();
        let create = source[register..]
            .find("save_inflight_state_create_new(")
            .unwrap()
            + register;
        let producer = source[create..].find("spawn_blocking(move ||").unwrap() + create;
        assert!(register < create && create < producer);
        assert!(!source[register..create].contains("transcript_turns"));
        assert!(source[producer..].contains("let _original_registration = producer_registration;"));
        assert!(source.contains("producer_registration = original_registration.clone()"));
    }
    let restore = include_str!("../../recovery_engine/restore_inflight.rs");
    for reader in ["spawn_observed_tmux_watcher(", "read_restored_output("] {
        let start = restore.find(reader).unwrap();
        assert!(restore[..start].contains(if reader.starts_with("spawn") {
            "reregister_active_turn_from_inflight("
        } else {
            "mailbox_recovery_kickoff("
        }));
    }
}
