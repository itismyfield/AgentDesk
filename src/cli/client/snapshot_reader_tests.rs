use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

#[test]
fn api_base_uses_snapshot_but_environment_still_wins() {
    isolated(|| {
        let fixture = Fixture::new();
        unsafe { std::env::remove_var("AGENTDESK_API_URL") };
        fixture.write("server: {host: 127.0.0.1, port: 41001}");
        assert_eq!(api_base(), "http://127.0.0.1:41001");
        fixture.install(
            "server: {host: 127.0.0.1, port: 41001}",
            "server: {host: 127.0.0.1, port: 41002}",
        );
        assert_eq!(api_base(), "http://127.0.0.1:41001");
        fixture.reload();
        assert_eq!(api_base(), "http://127.0.0.1:41002");
        unsafe { std::env::set_var("AGENTDESK_API_URL", "http://cli-target.invalid///") };
        assert_eq!(api_base(), "http://cli-target.invalid");
    });
}

#[test]
fn auth_token_uses_snapshot_and_keeps_none() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.write("server: {auth_token: validated-token}");
        assert_eq!(auth_token().as_deref(), Some("validated-token"));
        fixture.install(
            "server: {auth_token: validated-token}",
            "server: {auth_token: disk-token}",
        );
        assert_eq!(auth_token().as_deref(), Some("validated-token"));
        fixture.reload();
        assert_eq!(auth_token().as_deref(), Some("disk-token"));
        fixture.install("server: {}", "server: {auth_token: ignored-token}");
        assert!(auth_token().is_none());
    });
}
