use super::*;

#[test]
fn full_memento_prompt_carries_tool_feedback_contract() {
    let runtime_root = tempfile::tempdir().expect("runtime root");
    let _runtime_guard = crate::config::set_agentdesk_root_for_test(runtime_root.path());
    // #4306: the Proactive Memory Guidance memento branch must carry the
    // always-on `tool_feedback` contract that was dropped during the da7ccb39
    // provider-prompt slim-down. It is gated to the Full profile with the
    // memento backend and the MCP available.
    let settings = ResolvedMemorySettings {
        backend: MemoryBackendKind::Memento,
        ..ResolvedMemorySettings::default()
    };
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp/agentdesk",
        ChannelId::new(1),
        "tok",
        None,
        false,
        DispatchProfile::Full,
        None,
        None,
        None,
        None,
        Some(&settings),
        true,
        true,
    );

    assert!(
        prompt.contains("[Proactive Memory Guidance]"),
        "Full+memento prompt must include the proactive memory guidance block, got: {prompt}"
    );
    assert!(
        prompt.contains(
            "feedback contract: in the same turn you use `recall`/`context` results, \
             call `mcp__memento__tool_feedback` once"
        ),
        "Full+memento prompt must carry the tool_feedback contract, got: {prompt}"
    );
    // Required params surfaced verbatim per the current memento schema:
    // required = tool_name/relevant/sufficient; search_event_id is optional
    // (recommended when the response carries _meta.searchEventId).
    assert!(prompt.contains("required: `tool_name`, `relevant`, `sufficient`"));
    assert!(prompt.contains(
        "when the response carries `_meta.searchEventId`, \
         also pass it as `search_event_id` — recommended"
    ));
    // Deferred-tool loading instruction.
    assert!(prompt.contains("ToolSearch `select:mcp__memento__tool_feedback`"));
    // #5168: the model owns its own recall now — the Full prompt must not carry
    // the automatic-recall ownership claim any more.
    assert!(
        !prompt.contains("Do not call `context` or `recall`"),
        "#5168: the do-not-call-context instruction must be gone, got: {prompt}"
    );
}

#[test]
fn memory_group_full_dispatch_prompt_uses_role_scoped_group() {
    let root = tempfile::tempdir().unwrap();
    let _guard = crate::config::set_agentdesk_root_for_test(root.path());
    let mut binding = test_role_binding("ch-td");
    binding.memory.backend = MemoryBackendKind::Memento;
    binding.memory.memory_group = Some("cookingheart".into());
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/fixture/ch-td",
        ChannelId::new(4242),
        "tok",
        Some(&binding),
        false,
        DispatchProfile::Full,
        None,
        None,
        None,
        None,
        Some(&binding.memory),
        true,
        false,
    );
    assert!(prompt.contains("read=`workspace=cookingheart, agentId=ch-td`"));
    assert!(prompt.contains("default write (private)=`workspace=cookingheart, agentId=ch-td`"));
    assert!(prompt.contains("group shared=`workspace=cookingheart, agentId=default`"));
    assert!(prompt.contains("global=`agentId=default` (omit workspace)"));
    assert!(!prompt.contains("agent-private="));
}
