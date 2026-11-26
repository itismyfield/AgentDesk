//! A keyless first registration confirms as the V1 writer did, on Windows as well.
use crate::services::discord::runtime_store::durable_publish::flush_probe::{Plan, Step, Watch};
use crate::services::tui_input::human_frame::{history as h, idle};

/// On a missing ledger a keyless register succeeds without a directory fsync (Windows), while a
/// failed directory fsync still refuses it (Unix).
#[test]
fn keyless_first_register_succeeds_unless_a_directory_fsync_fails() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().join("native.jsonl");
    std::fs::write(&t, "").unwrap();
    let ledger = h::path().unwrap();
    let register = |id: &str| h::register(("claude", "pane", id, "7"), &t, 0, &idle("a", "7", id));
    if cfg!(unix) {
        let fail = vec![Step::ParentSync];
        let _failing = Watch::new(ledger.parent().unwrap(), Plan { fail, block: None });
        let refused = register("k0").unwrap_err();
        assert!(refused.contains("publication unconfirmed"), "{refused}");
    }
    register("k1").unwrap();
    let bytes = std::fs::read(&ledger).unwrap();
    assert!(bytes.starts_with(b"{\"version\":2,"));
}
