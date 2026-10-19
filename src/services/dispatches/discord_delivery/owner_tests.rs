use super::HttpDispatchTransport;

/// The dispatch thread owner is `discord.owner_id`; a legacy `escalation`
/// owner in the same YAML no longer redirects it.
#[test]
fn dispatch_thread_owner_comes_from_discord_owner_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentdesk.yaml");
    std::fs::write(
        &path,
        "server:\n  port: 8791\ndiscord:\n  owner_id: 111\nescalation:\n  owner_user_id: 999\n",
    )
    .unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_CONFIG", &path);
    let transport = HttpDispatchTransport::from_runtime_with_pg(None);
    assert_eq!(transport.thread_owner_user_id, Some(111));

    std::fs::write(&path, "server:\n  port: 8791\n").unwrap();
    let transport = HttpDispatchTransport::from_runtime_with_pg(None);
    assert_eq!(transport.thread_owner_user_id, None);
}
