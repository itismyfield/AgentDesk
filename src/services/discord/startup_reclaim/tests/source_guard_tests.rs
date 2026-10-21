pub(super) fn assert_orphan_delete_order() {
    let module_src = include_str!("../../startup_reclaim.rs");
    let guard_needle = ["terminal_cleanup_", "protects_delete("].concat();
    let attempt_needle = ["attempted ", "+= 1;"].concat();
    let delete_needle = [".delete_", "message(http, msg.id)"].concat();
    let result_needle = ["emit_relay_delete_", "result("].concat();
    let guard = module_src
        .find(&guard_needle)
        .expect("orphan reclaim must consult terminal cleanup protection");
    let attempt = module_src
        .find(&attempt_needle)
        .expect("orphan reclaim must count destructive attempts");
    let delete = module_src
        .find(&delete_needle)
        .expect("orphan reclaim must retain its bounded delete");
    let result = module_src
        .find(&result_needle)
        .expect("orphan reclaim delete result must be durably observed");

    assert!(
        guard < attempt && attempt < delete && delete < result,
        "committed and retry-pending terminal placeholders must be skipped before a delete attempt"
    );
}
