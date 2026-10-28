use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn doctor_report_config_uses_published_value_and_boot_fallback() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.write("server: {port: 41001}");
        assert_eq!(report_config().server.port, 41001);
        fixture.install("server: {port: 41001}", "server: {port: 41002}");
        assert_eq!(report_config().server.port, 41001);
        fixture.reload();
        assert_eq!(report_config().server.port, 41002);
    });
}
