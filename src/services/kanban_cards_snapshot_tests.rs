use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

fn headers(token: &'static str, channel: &'static str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", token.parse().unwrap());
    headers.insert("x-channel-id", channel.parse().unwrap());
    headers
}

#[test]
fn bearer_and_manager_channel_checks_use_one_validated_snapshot() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "server: {auth_token: snapshot-token}\nkanban: {manager_channel_id: '41'}",
            "server: {auth_token: disk-token}\nkanban: {manager_channel_id: '73'}",
        );
        assert!(
            require_explicit_bearer_token(&headers("Bearer snapshot-token", "41"), "fixture")
                .is_ok()
        );
        let error = require_explicit_bearer_token(&headers("Bearer disk-token", "41"), "fixture")
            .unwrap_err();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
        assert_eq!(error.1.0["error"], "fixture requires explicit Bearer token");
        let error =
            require_explicit_bearer_token(&headers("Bearer snapshot-token", "73"), "fixture")
                .unwrap_err();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            error.1.0["error"],
            "fixture requires PMD channel authorization"
        );
        fixture.reload();
        assert!(
            require_explicit_bearer_token(&headers("Bearer disk-token", "73"), "fixture").is_ok()
        );
    });
}
