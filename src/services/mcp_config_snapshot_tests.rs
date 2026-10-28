use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

fn configured_url() -> String {
    let value: serde_json::Value =
        serde_json::from_str(&claude_mcp_config_arg(None).unwrap()).unwrap();
    value["mcpServers"]["fixture"]["url"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn claude_mcp_argument_uses_validated_runtime_catalog() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.install(
            "mcp_servers: {fixture: {url: 'http://snapshot.test/mcp'}}",
            "mcp_servers: {fixture: {url: 'http://disk.test/mcp'}}",
        );
        assert_eq!(configured_url(), "http://snapshot.test/mcp");
        fixture.reload();
        assert_eq!(configured_url(), "http://disk.test/mcp");
    });
}

#[test]
fn claude_mcp_argument_preserves_standalone_file_fallback() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.write("mcp_servers: {fixture: {url: 'http://disk.test/mcp'}}");
        assert_eq!(configured_url(), "http://disk.test/mcp");
    });
}
