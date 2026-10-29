use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn stable_host_identity_prefers_snapshot_before_environment() {
    isolated(|| {
        let fixture = Fixture::new();
        // This child runs only this test, before any async work starts.
        unsafe { std::env::set_var("AGENTDESK_INSTANCE_ID", "  environment-id  ") };
        fixture.install(
            "cluster: {instance_id: '  snapshot-id  '}",
            "cluster: {instance_id: disk-id}",
        );
        assert_eq!(stable_host_identity().as_deref(), Some("snapshot-id"));
        fixture.reload();
        assert_eq!(stable_host_identity().as_deref(), Some("disk-id"));
    });
}

#[test]
fn stable_host_identity_keeps_trim_and_nonempty_environment_fallback() {
    isolated(|| {
        let fixture = Fixture::new();
        // This child runs only this test, before any async work starts.
        unsafe { std::env::set_var("AGENTDESK_INSTANCE_ID", "  environment-id  ") };
        fixture.install(
            "cluster: {instance_id: '   '}",
            "cluster: {instance_id: disk-id}",
        );
        assert_eq!(stable_host_identity().as_deref(), Some("environment-id"));
        // No other test can read this child's process environment.
        unsafe { std::env::set_var("AGENTDESK_INSTANCE_ID", "   ") };
        assert!(stable_host_identity().is_none());
    });
}
