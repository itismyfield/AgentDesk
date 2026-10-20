use super::*;
use crate::config::disk_write::test_support::*;

#[test]
fn migrate_apply_keeps_disk_secrets_and_adds_the_imported_agent() {
    let temp = tempfile::tempdir().unwrap();
    let runtime_root = temp.path().join("runtime");
    let _env = crate::config::test_env::set_agentdesk_root_for_test(&runtime_root);
    let yaml_path = crate::runtime_layout::config_file_path(&runtime_root);
    write_secret_config(&yaml_path, "");
    let source = temp.path().join("openclaw");
    let workspace = source.join("workspace-alpha");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(source.join("agents")).unwrap();
    let openclaw = serde_json::json!({"agents": {"list": [{
        "id": "alpha", "default": true, "name": "Alpha",
        "workspace": workspace, "model": "anthropic/claude-sonnet-4"
    }]}});
    std::fs::write(source.join("openclaw.json"), openclaw.to_string()).unwrap();

    let mut args = OpenClawMigrateArgs {
        root_path: Some(source.display().to_string()),
        agentdesk_root: Some(runtime_root.display().to_string()),
        agent_ids: vec!["alpha".to_string()],
        all_agents: false,
        dry_run: false,
        resume: None,
        fallback_provider: Some("claude".to_string()),
        workspace_root_rewrite: Vec::new(),
        write_org: false,
        write_bot_settings: false,
        write_db: false,
        overwrite: false,
        with_channel_bindings: false,
        with_sessions: false,
        snapshot_source: false,
        no_workspace: true,
        no_memory: true,
        no_prompts: true,
        tool_policy_mode: "report".to_string(),
        discord_token_mode: "report".to_string(),
    };
    let resolved =
        resolve_source_root(args.root_path.as_deref(), temp.path(), Some(&runtime_root)).unwrap();
    let mut overwrite_plan = build_import_plan(&resolved, &args, Some(&runtime_root)).unwrap();
    cmd_migrate_openclaw(args.clone()).unwrap();

    assert_secrets_on_disk(&yaml_path);
    let mut saved = crate::config::load_from_path(&yaml_path).unwrap();
    assert!(saved.agents.iter().any(|agent| agent.name == "Alpha"));
    saved.agents[0].memory_group = Some("existing-group".into());
    saved.agents[0].name = "Changed before overwrite".into();
    crate::config::save_to_path(&yaml_path, &saved).unwrap();
    args.overwrite = true;
    overwrite_plan.audit_root = Some(temp.path().join("overwrite-audit").display().to_string());
    apply_import_plan(&overwrite_plan, &resolved, &args, &runtime_root).unwrap();
    let overwritten = crate::config::load_from_path(&yaml_path).unwrap();
    assert!(
        overwritten.agents[0].memory_group.is_none(),
        "overwrite replaces optional fields with imported defaults"
    );
    assert_secrets_on_disk(&yaml_path);
}
