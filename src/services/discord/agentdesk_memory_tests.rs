use super::*;

#[test]
fn memory_group_reaches_channel_and_dm_bindings() {
    let root = tempfile::tempdir().unwrap();
    let _guard = crate::config::set_agentdesk_root_for_test(root.path());
    let agent: AgentDef = serde_yaml::from_str("id: ch-td\nname: Test\nmemory_group: cookingheart\nchannels:\n  claude:\n    id: '4242'\n").unwrap();
    assert_eq!(
        role_binding_from_channel(&agent, "claude", agent.channels.get("claude").unwrap())
            .memory
            .memory_group
            .as_deref(),
        Some("cookingheart")
    );
    assert_eq!(
        role_binding_from_agent(&agent, ProviderKind::Claude)
            .memory
            .memory_group
            .as_deref(),
        Some("cookingheart")
    );
}
