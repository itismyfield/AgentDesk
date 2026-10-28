use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn snapshot_delivery_owner_stays_published_until_reload() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "server: {}\ndiscord: {owner_id: 41}",
            "server: {}\ndiscord: {owner_id: 73}",
        );
        assert_eq!(resolve_dispatch_thread_owner_user_id(), Some(41));
        fixture.reload();
        assert_eq!(resolve_dispatch_thread_owner_user_id(), Some(73));
    });
}
