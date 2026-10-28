use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn snapshot_dispatch_required_capabilities_keep_explicit_priority() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "server: {}\ncluster: {dispatch_routing: {default_preferred_labels: [snapshot]}}",
            "server: {}\ncluster: {dispatch_routing: {default_preferred_labels: [disk]}}",
        );
        let expected = serde_json::json!({"preferred":{"labels":["snapshot"]}});
        assert_eq!(
            dispatch_required_capabilities("{}", "implementation"),
            Some(expected)
        );
        assert_eq!(
            dispatch_required_capabilities(
                "{\"required_capabilities\":{\"fixed\":true}}",
                "implementation"
            ),
            Some(serde_json::json!({"fixed":true}))
        );
        assert_eq!(
            dispatch_required_capabilities("not json", "implementation"),
            None
        );
        fixture.reload();
        assert_eq!(
            dispatch_required_capabilities("{}", "implementation"),
            Some(serde_json::json!({"preferred":{"labels":["disk"]}}))
        );
    });
}
