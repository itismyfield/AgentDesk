use super::*;
use crate::services::discord::settings::{MemoryBackendKind, resolve_role_binding};
use axum::{Json, Router, extract::State, routing::post};

fn fixture(root: &Path, group: Option<&str>) -> crate::services::discord::settings::RoleBinding {
    let mut config = serde_yaml::to_value(crate::config::Config::default()).unwrap();
    config["agents"] = serde_yaml::from_str(&format!(
        "- id: ch-td\n  name: Test\n  channels:\n    claude:\n      id: '4242'\n      workspace: /fixture/ch-td\n{}",
        group.map(|g| format!("  memory_group: '{g}'\n")).unwrap_or_default(),
    )).unwrap();
    let path = runtime_layout::config_file_path(root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_yaml::to_string(&config).unwrap()).unwrap();
    resolve_role_binding(ChannelId::new(4242), None).unwrap()
}

async fn automatic_calls(group: Option<&str>) -> Vec<Value> {
    scoped_calls(group, false, None).await
}

async fn scoped_calls(
    group: Option<&str>,
    anchor: bool,
    workspace_override: Option<&str>,
) -> Vec<Value> {
    scoped_calls_with_agent(group, anchor, workspace_override, Some("ch-td")).await
}

async fn scoped_calls_with_agent(
    group: Option<&str>,
    anchor: bool,
    workspace_override: Option<&str>,
    agent_id: Option<&str>,
) -> Vec<Value> {
    scoped_calls_with_workspace(group, anchor, workspace_override, agent_id, None).await
}

async fn scoped_calls_with_workspace(
    group: Option<&str>,
    anchor: bool,
    workspace_override: Option<&str>,
    agent_id: Option<&str>,
    workspace: Option<&str>,
) -> Vec<Value> {
    scoped_calls_with_group_change(group, anchor, workspace_override, agent_id, workspace, None)
        .await
}

async fn scoped_calls_with_group_change(
    group: Option<&str>,
    anchor: bool,
    workspace_override: Option<&str>,
    agent_id: Option<&str>,
    workspace: Option<&str>,
    changed_group: Option<&str>,
) -> Vec<Value> {
    let root = tempfile::tempdir().unwrap();
    let _guard = crate::config::set_agentdesk_root_for_test(root.path());
    let _key = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "I7131_FIXTURE_KEY",
        Path::new("fixture-only"),
    );
    let _override = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "MEMENTO_WORKSPACE",
        std::ffi::OsStr::new(workspace_override.unwrap_or("")),
    );
    let mut binding = fixture(root.path(), group);
    binding.memory.backend = MemoryBackendKind::Memento;
    let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
    let app = Router::new().route("/mcp", post(
        |State(calls): State<Arc<Mutex<Vec<Value>>>>, Json(body): Json<Value>| async move {
            let result = if body["method"] == "initialize" { json!({}) } else {
                calls.lock().unwrap_or_else(|p| p.into_inner()).push(body["params"].clone());
                json!({"content": [{"type": "text", "text": json!({"success":true,"anchorCount":0,"anchors":{"permanent":[]}}).to_string()}]})
            };
            ([("mcp-session-id", "scope-fixture")], Json(json!({"jsonrpc":"2.0","id":body["id"],"result":result})))
        },
    )).with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let path = runtime_layout::memory_backend_path(root.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, json!({"version":2,"backend":"memento","mcp":{"endpoint":endpoint,"access_key_env":"I7131_FIXTURE_KEY"}}).to_string()).unwrap();
    // The HTTP adapter starts with global settings and resolves scope from the channel.
    let backend = MementoBackend::new(ResolvedMemorySettings {
        memory_group: None,
        ..binding.memory.clone()
    });
    backend
        .remember(MementoRememberRequest {
            content: "confirmed fixture fact".into(),
            topic: "scope".into(),
            kind: "fact".into(),
            channel_id: Some(4242),
            agent_id: agent_id.map(str::to_owned),
            workspace: workspace.map(str::to_owned),
            ..Default::default()
        })
        .await
        .unwrap();
    let recall = backend
        .recall(RecallRequest {
            provider: crate::services::provider::ProviderKind::Claude,
            role_id: "ch-td".into(),
            channel_id: 4242,
            channel_name: None,
            session_id: root.path().display().to_string(),
            dispatch_profile: DispatchProfile::Full,
            user_text: "scope fixture".into(),
        })
        .await;
    assert!(recall.memento_context_loaded, "{:?}", recall.warnings);
    if anchor {
        let repo = root.path().join("legacy-project");
        std::fs::create_dir(&repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        let request = |fresh| SessionAnchorRequest {
            settings: &binding.memory,
            provider: &crate::services::provider::ProviderKind::Claude,
            current_path: repo.to_str().unwrap(),
            channel_id: 4242,
            memory_scope_channel_id: 4242,
            role_binding: Some(&binding),
            session_id: Some("scope-anchor-session"),
            fresh,
        };
        load_session_anchor_prompt(request(true)).await;
        load_session_anchor_prompt(request(false)).await;
        if let Some(group) = changed_group {
            let mut changed_binding = binding.clone();
            changed_binding.memory.memory_group = Some(group.to_owned());
            load_session_anchor_prompt(SessionAnchorRequest {
                settings: &changed_binding.memory,
                role_binding: Some(&changed_binding),
                ..request(false)
            })
            .await;
        }
    }
    let captured = calls.lock().unwrap_or_else(|p| p.into_inner()).clone();
    server.abort();
    captured
}

#[tokio::test]
async fn memory_group_automatic_remember_and_recall_use_group() {
    let calls = automatic_calls(Some("cookingheart")).await;
    assert_eq!(calls.len(), 2);
    for (call, tool) in calls.iter().zip(["remember", "context"]) {
        assert_eq!(call["name"], tool);
        assert_eq!(call["arguments"]["workspace"], "cookingheart");
        assert_eq!(call["arguments"]["agentId"], "ch-td");
    }
}

#[tokio::test]
async fn memory_group_absent_automatic_paths_preserve_legacy_workspace() {
    let calls = automatic_calls(None).await;
    assert_eq!(calls.len(), 2);
    for call in calls {
        assert_eq!(call["arguments"]["workspace"], "agentdesk-ch-td");
        assert_eq!(call["arguments"]["agentId"], "ch-td");
    }
}

#[tokio::test]
async fn memory_group_anchor_uses_shared_group_and_keeps_session_cache() {
    let calls = scoped_calls(Some("cookingheart"), true, None).await;
    assert_eq!(calls.len(), 3, "one anchor fetch plus remember and recall");
    assert_eq!(calls[2]["arguments"]["workspace"], "cookingheart");
    assert_eq!(calls[2]["arguments"]["agentId"], "default");
}

#[tokio::test]
async fn memory_group_absent_anchor_preserves_git_workspace_and_cache() {
    let calls = scoped_calls(None, true, None).await;
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2]["arguments"]["workspace"], "legacy-project");
    assert_eq!(calls[2]["arguments"]["agentId"], "default");
}

#[tokio::test]
async fn memory_group_override_stays_first_for_automatic_and_anchor_paths() {
    let calls = scoped_calls(Some("cookingheart"), true, Some("explicit-scope")).await;
    assert_eq!(calls.len(), 3);
    for call in calls {
        assert_eq!(call["arguments"]["workspace"], "explicit-scope");
    }
}

#[test]
fn memory_group_meeting_participant_settings_win_over_host_channel() {
    let root = tempfile::tempdir().unwrap();
    let _guard = crate::config::set_agentdesk_root_for_test(root.path());
    fixture(root.path(), Some("cookingheart"));
    let backend = MementoBackend::new(ResolvedMemorySettings {
        memory_group: Some("family-counsel".into()),
        ..Default::default()
    });
    let config = MementoRuntimeConfig {
        endpoint: "fixture".into(),
        access_key: "fixture".into(),
        workspace_override: None,
    };
    assert_eq!(
        backend.resolve_workspace("family-counsel", 4242, None, &config),
        "family-counsel"
    );
}

#[tokio::test]
async fn memory_group_api_shape_implicit_remember_defaults_to_private_role() {
    let calls = scoped_calls_with_agent(Some("cookingheart"), false, None, None).await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["arguments"]["workspace"], "cookingheart");
    assert_eq!(calls[0]["arguments"]["agentId"], "ch-td");
}

#[tokio::test]
async fn memory_group_api_shape_absent_group_keeps_omitted_agent_id() {
    let calls = scoped_calls_with_agent(None, false, None, None).await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["arguments"]["workspace"], "agentdesk-ch-td");
    assert!(calls[0]["arguments"].get("agentId").is_none());
}

#[tokio::test]
async fn memory_group_api_shape_explicit_shared_agent_id_stays_default() {
    let calls = scoped_calls_with_agent(Some("cookingheart"), false, None, Some("default")).await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["arguments"]["workspace"], "cookingheart");
    assert_eq!(calls[0]["arguments"]["agentId"], "default");
}

#[tokio::test]
async fn memory_group_api_shape_explicit_workspace_keeps_shared_default() {
    let calls = scoped_calls_with_workspace(
        Some("cookingheart"),
        false,
        None,
        None,
        Some("cookingheart"),
    )
    .await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["arguments"]["workspace"], "cookingheart");
    assert!(calls[0]["arguments"].get("agentId").is_none());
}

#[tokio::test]
async fn memory_group_anchor_cache_tracks_group_changes() {
    let calls = scoped_calls_with_group_change(
        Some("cookingheart"),
        true,
        None,
        Some("ch-td"),
        None,
        Some("family-counsel"),
    )
    .await;
    assert_eq!(calls.len(), 4, "a changed group must fetch its own anchors");
    assert_eq!(calls[2]["arguments"]["workspace"], "cookingheart");
    assert_eq!(calls[3]["arguments"]["workspace"], "family-counsel");
    assert_eq!(calls[3]["arguments"]["agentId"], "default");
}
