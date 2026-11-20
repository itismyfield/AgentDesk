use super::*;

fn full_memento_prompt(
    current_path: &str,
    channel_id: u64,
    role_binding: Option<&RoleBinding>,
) -> String {
    let runtime_root = tempfile::tempdir().expect("runtime root");
    let _runtime_guard = crate::config::set_agentdesk_root_for_test(runtime_root.path());
    let settings = ResolvedMemorySettings {
        backend: MemoryBackendKind::Memento,
        ..ResolvedMemorySettings::default()
    };
    build_system_prompt(
        "ctx",
        &[],
        current_path,
        ChannelId::new(channel_id),
        "tok",
        role_binding,
        false,
        DispatchProfile::Full,
        None,
        None,
        None,
        None,
        Some(role_binding.map_or(&settings, |binding| &binding.memory)),
        true,
        true,
    )
}

#[test]
fn full_memento_prompt_carries_tool_feedback_contract() {
    // Full dispatch with Memento MCP must carry the tool_feedback contract.
    let prompt = full_memento_prompt("/tmp/agentdesk", 1, None);

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

fn agentdesk_checkout(root: &std::path::Path) -> std::path::PathBuf {
    let checkout = root.join("AgentDesk");
    let docs = checkout.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("source-of-truth.md"), "").unwrap();
    std::fs::write(docs.join("memory-scope.md"), "").unwrap();
    checkout
}

fn assert_agentdesk_workspace_spelling(prompt: &str) {
    let policy = prompt
        .lines()
        .find(|line| line.starts_with("- full memory policy:"))
        .expect("the final prompt must include the AgentDesk memory policy");
    assert!(policy.contains("`docs/memory-scope.md`"));
    assert!(
        policy.contains(
            "For calls with a `workspace` field, use `workspace=agentdesk` when targeting the AgentDesk workspace."
        ),
        "expected the conditional lowercase workspace rule, got: {policy}"
    );
    assert!(policy.contains("Follow the scope hints. Omit `workspace` for global writes."));
    assert!(policy.contains("Never use `AgentDesk` as `workspace` or `agentId`."));
}

#[test]
fn agentdesk_workspace_spelling_keeps_ungrouped_prompt_scope() {
    let root = tempfile::tempdir().unwrap();
    let checkout = agentdesk_checkout(root.path());
    let prompt = full_memento_prompt(checkout.to_str().unwrap(), 1, None);
    assert_eq!(
        prompt
            .lines()
            .find(|line| line.starts_with("- scope hints:")),
        Some(
            "- scope hints: project=`workspace=agentdesk, agentId=default`; agent-private=`workspace=channel-1, agentId=agentdesk-channel-1`."
        ),
    );
    assert_agentdesk_workspace_spelling(&prompt);
}

#[test]
fn agentdesk_workspace_spelling_keeps_grouped_prompt_scope() {
    let root = tempfile::tempdir().unwrap();
    let checkout = agentdesk_checkout(root.path());
    let mut binding = test_role_binding("ch-td");
    binding.memory.backend = MemoryBackendKind::Memento;
    binding.memory.memory_group = Some("agentdesk".into());
    let prompt = full_memento_prompt(checkout.to_str().unwrap(), 4242, Some(&binding));
    assert!(prompt.contains("read=`workspace=agentdesk, agentId=ch-td`"));
    assert!(prompt.contains("default write (private)=`workspace=agentdesk, agentId=ch-td`"));
    assert!(prompt.contains("group shared=`workspace=agentdesk, agentId=default`"));
    assert!(prompt.contains("global=`agentId=default` (omit workspace)"));
    assert!(!prompt.contains("agent-private="));
    assert_agentdesk_workspace_spelling(&prompt);
}

#[test]
fn agentdesk_workspace_spelling_is_absent_from_foreign_prompt() {
    let checkout = tempfile::tempdir().unwrap();
    let prompt = full_memento_prompt(checkout.path().to_str().unwrap(), 1, None);
    assert!(prompt.contains("agent-private=`workspace=channel-1, agentId=agentdesk-channel-1`"));
    assert!(!prompt.contains("docs/memory-scope.md"));
    assert!(!prompt.contains("workspace=agentdesk"));
    assert!(!prompt.contains("Never use `AgentDesk` as `workspace` or `agentId`."));
}
