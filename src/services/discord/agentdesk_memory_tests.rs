use super::*;

#[test]
fn memory_group_reaches_channel_voice_dm_and_meeting_bindings() {
    let root = tempfile::tempdir().unwrap();
    let _guard = crate::config::set_agentdesk_root_for_test(root.path());
    let agent: AgentDef = serde_yaml::from_str("id: ch-td\nname: Test\nmemory_group: cookingheart\nchannels:\n  claude:\n    id: '4242'\nvoice:\n  channel_id: '4243'\n").unwrap();
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
    let mut config = Config::default();
    config.agents.push(agent);
    for entry in [
        MeetingAgentEntry::RoleId("ch-td".into()),
        MeetingAgentEntry::Detailed(crate::config::MeetingAgentDef {
            role_id: "ch-td".into(),
            ..Default::default()
        }),
    ] {
        assert_eq!(
            meeting_agent_from_entry(&config, &entry)
                .unwrap()
                .memory
                .memory_group
                .as_deref(),
            Some("cookingheart")
        );
    }
    config.meeting =
        Some(serde_yaml::from_str("channel_name: fixture\nsummary_agent: ch-td").unwrap());
    let path = crate::runtime_layout::config_file_path(root.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_yaml::to_string(&config).unwrap()).unwrap();
    assert_eq!(
        load_meeting_config().unwrap().available_agents[0]
            .memory
            .memory_group
            .as_deref(),
        Some("cookingheart")
    );
}
