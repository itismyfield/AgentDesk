use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn snapshot_kanban_diagnostic_uses_published_manager_channel() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "server: {}\nkanban: {manager_channel_id: '41'}",
            "server: {}\nkanban: {manager_channel_id: '42'}",
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-channel-id", "41".parse().unwrap());
        let (_, values) = crate::config::snapshot_reader_tests::capture_identity(|| {
            log_kanban_identity_consumption("snapshot fixture", &headers, &None, "fixture");
        });
        assert_eq!(values, vec![true]);
        fixture.reload();
        let (_, values) = crate::config::snapshot_reader_tests::capture_identity(|| {
            log_kanban_identity_consumption("snapshot fixture", &headers, &None, "fixture");
        });
        assert_eq!(values, vec![false]);
    });
}
