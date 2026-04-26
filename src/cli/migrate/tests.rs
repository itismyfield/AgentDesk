use std::fs;
use std::path::Path;

use tempfile::TempDir;

use super::*;

fn write_openclaw_config(root: &Path, body: &str) {
    fs::create_dir_all(root.join("agents")).unwrap();
    fs::write(root.join(source::OPENCLAW_CONFIG_NAME), body).unwrap();
}

fn base_args() -> OpenClawMigrateArgs {
    OpenClawMigrateArgs {
        root_path: None,
        agentdesk_root: None,
        agent_ids: Vec::new(),
        all_agents: false,
        dry_run: true,
        resume: None,
        fallback_provider: None,
        workspace_root_rewrite: Vec::new(),
        write_org: false,
        write_bot_settings: false,
        write_db: false,
        overwrite: false,
        with_channel_bindings: false,
        with_sessions: false,
        snapshot_source: false,
        no_workspace: false,
        no_memory: false,
        no_prompts: false,
        tool_policy_mode: "report".to_string(),
        discord_token_mode: "report".to_string(),
    }
}

fn resolve_source(temp: &TempDir) -> source::ResolvedSourceRoot {
    resolve_source_root(Some(temp.path().to_str().unwrap()), temp.path(), None).unwrap()
}

fn read_json_value(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn audit_root(plan: &plan::ImportPlan) -> std::path::PathBuf {
    std::path::PathBuf::from(plan.audit_root.as_ref().unwrap())
}

#[test]
fn resolves_explicit_openclaw_json_file() {
    let temp = TempDir::new().unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{"agents":{"list":[{"id":"alpha","model":"openai/gpt-5"}]}}"#,
    );

    let resolved = resolve_source_root(
        Some(
            temp.path()
                .join(source::OPENCLAW_CONFIG_NAME)
                .to_str()
                .unwrap(),
        ),
        temp.path(),
        None,
    )
    .unwrap();

    assert_eq!(resolved.root, temp.path());
    assert_eq!(
        resolved.config_path,
        temp.path().join(source::OPENCLAW_CONFIG_NAME)
    );
}

#[test]
fn resolves_json5_with_include_and_model_union() {
    let temp = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("workspace")).unwrap();
    fs::write(
        temp.path().join("agents.json5"),
        r#"{
            list: [
                {
                    id: "alpha",
                    default: true,
                    workspace: "workspace",
                    model: {
                        primary: "openai/gpt-5",
                        fallbacks: ["anthropic/claude-3"],
                    },
                },
            ],
        }"#,
    )
    .unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{
            // JSON5 comments and trailing commas are allowed.
            agents: { $include: "./agents.json5", },
        }"#,
    );

    let source = resolve_source(&temp);
    let plan = build_import_plan(&source, &base_args(), None).unwrap();

    assert_eq!(plan.agents[0].model_hint.as_deref(), Some("openai/gpt-5"));
    assert_eq!(plan.agents[0].mapped_provider.as_deref(), Some("codex"));
    assert!(source.resolved_config_paths.len() >= 2);
    assert!(
        source
            .resolved_config_paths
            .iter()
            .any(|path| path.ends_with("agents.json5"))
    );
}

#[test]
fn fails_when_multiple_candidates_exist() {
    let temp = TempDir::new().unwrap();
    let root_a = temp.path().join("a");
    let root_b = temp.path().join("b");
    fs::create_dir_all(&root_a).unwrap();
    fs::create_dir_all(&root_b).unwrap();
    write_openclaw_config(
        &root_a,
        r#"{"agents":{"list":[{"id":"alpha","model":"openai/gpt-5"}]}}"#,
    );
    write_openclaw_config(
        &root_b,
        r#"{"agents":{"list":[{"id":"beta","model":"openai/gpt-5"}]}}"#,
    );

    let err =
        resolve_source_root(Some(temp.path().to_str().unwrap()), temp.path(), None).unwrap_err();
    assert!(err.contains("Multiple valid 'openclaw.json' candidates"));
}

#[test]
fn defaults_to_default_agent_selection() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{
                "list":[
                    {"id":"alpha","model":"openai/gpt-5","workspace":"workspace-alpha"},
                    {"id":"beta","default":true,"model":"openai/gpt-5","workspace":"workspace"}
                ]
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let plan = build_import_plan(&source, &base_args(), None).unwrap();

    assert_eq!(plan.selection_mode, "default_agent");
    assert_eq!(plan.selected_agent_ids, vec!["beta"]);
    assert!(plan.selected_discord_account_ids.is_empty());
    assert_eq!(plan.agents.len(), 1);
    assert_eq!(plan.agents[0].source_id, "beta");
}

#[test]
fn legacy_default_agent_selection_still_resolves() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{
                "defaultAgent":"alpha",
                "list":[
                    {"id":"alpha","model":"openai/gpt-5","workspace":"workspace"},
                    {"id":"beta","model":"openai/gpt-5","workspace":"workspace-beta"}
                ]
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let plan = build_import_plan(&source, &base_args(), None).unwrap();

    assert_eq!(plan.selection_mode, "default_agent");
    assert_eq!(plan.selected_agent_ids, vec!["alpha"]);
}

#[test]
fn fails_without_agent_hint_for_multi_agent_source() {
    let temp = TempDir::new().unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{
                "list":[
                    {"id":"alpha","model":"openai/gpt-5"},
                    {"id":"beta","model":"openai/gpt-5"}
                ]
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let err = build_import_plan(&source, &base_args(), None).unwrap_err();
    assert!(err.contains("Pass --agent or --all-agents"));
}

#[test]
fn rejects_candidate_without_agents_state_dir() {
    let temp = TempDir::new().unwrap();
    fs::write(
        temp.path().join(source::OPENCLAW_CONFIG_NAME),
        r#"{"agents":{"list":[{"id":"alpha","model":"openai/gpt-5"}]}}"#,
    )
    .unwrap();

    let err =
        resolve_source_root(Some(temp.path().to_str().unwrap()), temp.path(), None).unwrap_err();
    assert!(err.contains("missing required agents/ directory"));
}

#[test]
fn role_ids_prefix_when_org_yaml_collides() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let config_dir = runtime.path().join("config");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    fs::write(
        config_dir.join("org.yaml"),
        "agents:\n  alpha:\n    name: Existing\n",
    )
    .unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{"agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]}}"#,
    );

    let source = resolve_source(&temp);
    let plan = build_import_plan(&source, &base_args(), Some(runtime.path())).unwrap();

    assert_eq!(plan.agents[0].final_role_id, "openclaw-alpha");
    assert_eq!(plan.existing_role_ids, vec!["alpha"]);
}

#[test]
fn rerun_reuses_existing_prefixed_role_id() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(runtime.path().join("config")).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    fs::write(
        runtime.path().join("agentdesk.yaml"),
        r#"agents:
- id: openclaw-alpha
  name: alpha
  provider: codex
"#,
    )
    .unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{"agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]}}"#,
    );

    let source = resolve_source(&temp);
    let plan = build_import_plan(&source, &base_args(), Some(runtime.path())).unwrap();

    assert_eq!(plan.agents[0].final_role_id, "openclaw-alpha");
}

#[test]
fn resume_reuses_stored_role_id_from_agent_map() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let import_id = "resume-import-123";
    let audit_root = runtime
        .path()
        .join("openclaw")
        .join("imports")
        .join(import_id);
    fs::create_dir_all(runtime.path().join("config")).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&audit_root).unwrap();
    fs::write(
        runtime.path().join("agentdesk.yaml"),
        r#"agents:
- id: alpha
  name: Existing alpha
  provider: codex
- id: openclaw-alpha
  name: Existing imported alpha
  provider: codex
- id: openclaw-alpha-2
  name: Previously imported alpha
  provider: codex
"#,
    )
    .unwrap();
    fs::write(
        audit_root.join("agent-map.json"),
        r#"[
  {"source_id":"alpha","role_id":"openclaw-alpha-2"}
]"#,
    )
    .unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{"agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]}}"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.resume = Some(import_id.to_string());
    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();

    assert_eq!(plan.agents[0].final_role_id, "openclaw-alpha-2");
}

#[test]
fn fallback_provider_maps_unsupported_sources() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{"agents":{"list":[{"id":"alpha","default":true,"model":"meta/llama-3","workspace":"workspace"}]}}"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.fallback_provider = Some("qwen".to_string());
    let plan = build_import_plan(&source, &args, None).unwrap();

    assert_eq!(plan.agents[0].mapped_provider.as_deref(), Some("qwen"));
    assert!(
        plan.warnings
            .iter()
            .any(|warning| warning.contains("fallback 'qwen'"))
    );
}

#[test]
fn defaults_workspace_is_used_for_agents_without_workspace() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("shared-workspace");
    fs::create_dir_all(&workspace).unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{
                "defaults":{"workspace":"shared-workspace"},
                "list":[{"id":"alpha","default":true,"model":"openai/gpt-5"}]
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let plan = build_import_plan(&source, &base_args(), None).unwrap();

    assert!(
        plan.agents[0]
            .workspace_source
            .ends_with("shared-workspace")
    );
    assert!(plan.agents[0].workspace_exists);
}

#[test]
fn windows_absolute_workspace_path_is_not_joined_to_source_root() {
    let temp = TempDir::new().unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{"agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"C:\\openclaw\\workspace-alpha"}]}}"#,
    );

    let source = resolve_source(&temp);
    let plan = build_import_plan(&source, &base_args(), None).unwrap();

    assert_eq!(
        plan.agents[0].workspace_source,
        r"C:\openclaw\workspace-alpha"
    );
}

#[test]
fn workspace_root_rewrite_remaps_absolute_openclaw_paths() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace-main");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{
                "list":[
                    {
                        "id":"alpha",
                        "default":true,
                        "model":"openai/gpt-5",
                        "workspace":"/home/node/.openclaw/workspace-main"
                    }
                ]
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.workspace_root_rewrite = vec![format!("/home/node/.openclaw={}", temp.path().display())];
    let plan = build_import_plan(&source, &args, None).unwrap();

    assert_eq!(
        plan.agents[0].workspace_source,
        workspace.display().to_string()
    );
    assert!(plan.agents[0].workspace_exists);
    assert!(plan.agents[0].eligible_for_v1);
    assert!(
        plan.warnings
            .iter()
            .any(|warning| { warning.contains("remapped via --workspace-root-rewrite") })
    );
}

#[test]
fn channel_bindings_without_write_org_emit_preview_warning() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{"agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]}}"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.with_channel_bindings = true;
    let plan = build_import_plan(&source, &args, None).unwrap();

    assert!(plan.warnings.iter().any(|warning| {
        warning.contains("--with-channel-bindings without --write-org stays preview-only")
    }));
    assert_eq!(plan.effective_modes.channel_bindings, "preview_only");
}

#[test]
fn no_workspace_disables_workspace_copy_task() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{"agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]}}"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.no_workspace = true;
    let plan = build_import_plan(&source, &args, None).unwrap();

    let task = plan.agents[0]
        .tasks
        .iter()
        .find(|task| task.key == "workspace_copy")
        .unwrap();
    assert_eq!(task.mode, "disabled");
}

#[test]
fn with_sessions_enables_session_phase_and_task() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{"agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]}}"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.with_sessions = true;
    let plan = build_import_plan(&source, &args, None).unwrap();

    let phase = plan
        .phases
        .iter()
        .find(|phase| phase.phase == "sessions")
        .unwrap();
    assert_eq!(phase.mode, "preview_only");

    let task = plan.agents[0]
        .tasks
        .iter()
        .find(|task| task.key == "session_import")
        .unwrap();
    assert_eq!(task.mode, "preview_only");
}

#[test]
fn base_apply_writes_agentdesk_prompt_memory_workspace_and_audit() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(workspace.join("memory")).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha Identity\nalpha").unwrap();
    fs::write(workspace.join("AGENTS.md"), "# Alpha Rules\nrules").unwrap();
    fs::write(workspace.join("BOOT.md"), "# Alpha Boot\nboot").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\nstable").unwrap();
    fs::write(
        workspace.join("memory").join("2026-04-02.md"),
        "# Daily\nentry",
    )
    .unwrap();
    fs::create_dir_all(workspace.join("skills")).unwrap();
    fs::write(workspace.join("skills").join("local-skill.txt"), "skill").unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{
                "list":[
                    {"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}
                ]
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;
    args.write_org = true;
    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();

    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    let agentdesk_yaml =
        fs::read_to_string(crate::runtime_layout::config_file_path(runtime.path())).unwrap();
    assert!(agentdesk_yaml.contains("id: alpha"));
    assert!(agentdesk_yaml.contains("provider: codex"));

    let prompt = fs::read_to_string(
        crate::runtime_layout::managed_agents_root(runtime.path())
            .join("alpha")
            .join("IDENTITY.md"),
    )
    .unwrap();
    assert!(prompt.contains("Imported OpenClaw Identity"));
    assert!(prompt.contains("Imported OpenClaw Agent Rules"));
    assert!(prompt.contains("Imported OpenClaw Boot Intent"));
    assert!(prompt.contains("AgentDesk Runtime References"));
    assert!(
        prompt.contains(
            &crate::runtime_layout::long_term_memory_root(runtime.path())
                .join("alpha")
                .display()
                .to_string()
        )
    );
    assert!(
        prompt.contains(
            &runtime
                .path()
                .join("openclaw")
                .join("workspaces")
                .join("alpha")
                .display()
                .to_string()
        )
    );
    assert!(prompt.contains(&workspace.display().to_string()));

    let memory_md = fs::read_to_string(
        crate::runtime_layout::long_term_memory_root(runtime.path())
            .join("alpha")
            .join("MEMORY.md"),
    )
    .unwrap();
    assert!(memory_md.contains("# Memory"));

    let daily_md = fs::read_to_string(
        crate::runtime_layout::long_term_memory_root(runtime.path())
            .join("alpha")
            .join("daily-2026-04-02.md"),
    )
    .unwrap();
    assert!(daily_md.contains("imported_from: openclaw"));
    assert!(daily_md.contains("source_agent: alpha"));

    let org_yaml = fs::read_to_string(runtime.path().join("config").join("org.yaml")).unwrap();
    assert!(org_yaml.contains("alpha:"));
    assert!(org_yaml.contains("display_name: alpha"));
    assert!(org_yaml.contains("provider: codex"));
    assert!(org_yaml.contains("workspace:"));
    assert!(org_yaml.contains("prompt_file:"));

    let copied_workspace_file = runtime
        .path()
        .join("openclaw")
        .join("workspaces")
        .join("alpha")
        .join("skills")
        .join("local-skill.txt");
    assert!(copied_workspace_file.is_file());

    let audit_root = std::path::PathBuf::from(plan.audit_root.as_ref().unwrap());
    assert!(audit_root.join("manifest.json").is_file());
    assert!(audit_root.join("agent-map.json").is_file());
    assert!(audit_root.join("write-plan.json").is_file());
    assert!(audit_root.join("apply-result.json").is_file());
    assert!(audit_root.join("resume-state.json").is_file());
    assert!(audit_root.join("tool-policy-report.json").is_file());
    assert!(audit_root.join("discord-auth-report.json").is_file());
    assert!(audit_root.join("channel-binding-preview.yaml").is_file());
}

#[test]
fn live_write_org_imports_representable_discord_channel_bindings() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\n").unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{
                "list":[
                    {"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}
                ]
            },
            "bindings":[
                {"agentId":"alpha","match":{"channel":"discord"}}
            ],
            "channels":{
                "discord":{
                    "guilds":{
                        "guild-1":{
                            "channels":{
                                "1234567890":{"allow":true},
                                "9999999999":{"allow":false}
                            }
                        }
                    }
                }
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;
    args.write_org = true;
    args.with_channel_bindings = true;

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    let org_yaml = fs::read_to_string(runtime.path().join("config").join("org.yaml")).unwrap();
    assert!(org_yaml.contains("1234567890"));
    assert!(org_yaml.contains("agent: alpha"));
    assert!(org_yaml.contains("provider: codex"));
    assert!(!org_yaml.contains("\"9999999999\""));
}

#[test]
fn live_channel_binding_conflicts_skip_only_shared_channel_ids() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let alpha_workspace = temp.path().join("workspace-alpha");
    let beta_workspace = temp.path().join("workspace-beta");
    fs::create_dir_all(&alpha_workspace).unwrap();
    fs::create_dir_all(&beta_workspace).unwrap();
    fs::write(alpha_workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(alpha_workspace.join("MEMORY.md"), "# Memory\n").unwrap();
    fs::write(beta_workspace.join("IDENTITY.md"), "# Beta\n").unwrap();
    fs::write(beta_workspace.join("MEMORY.md"), "# Memory\n").unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{
                "list":[
                    {"id":"alpha","model":"openai/gpt-5","workspace":"workspace-alpha"},
                    {"id":"beta","model":"openai/gpt-5","workspace":"workspace-beta"}
                ]
            },
            "bindings":[
                {"agentId":"alpha","match":{"channel":"discord","accountId":"a"}},
                {"agentId":"beta","match":{"channel":"discord","accountId":"b"}}
            ],
            "channels":{
                "discord":{
                    "accounts":{
                        "a":{
                            "token":"discord-token-a",
                            "guilds":{"g1":{"channels":{"999":{"allow":true},"111":{"allow":true}}}}
                        },
                        "b":{
                            "token":"discord-token-b",
                            "guilds":{"g2":{"channels":{"999":{"allow":true},"222":{"allow":true}}}}
                        }
                    }
                }
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.all_agents = true;
    args.dry_run = false;
    args.write_org = true;
    args.write_bot_settings = true;
    args.with_channel_bindings = true;
    args.discord_token_mode = "plaintext-only".to_string();

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();

    assert_eq!(plan.selected_discord_account_ids, vec!["a", "b"]);
    assert_eq!(plan.discord.bindings.len(), 2);
    assert_eq!(plan.discord.bindings[0].mode, "live_applicable");
    assert_eq!(plan.discord.bindings[0].channel_ids, vec!["111"]);
    assert!(
        plan.discord.bindings[0]
            .warnings
            .iter()
            .any(|warning| { warning.contains("Conflicting Discord channel ids were skipped") })
    );
    assert_eq!(plan.discord.bindings[1].mode, "live_applicable");
    assert_eq!(plan.discord.bindings[1].channel_ids, vec!["222"]);
    assert!(
        plan.warnings
            .iter()
            .any(|warning| { warning.contains("will be skipped from live channel imports: 999") })
    );

    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    let org_yaml = fs::read_to_string(runtime.path().join("config").join("org.yaml")).unwrap();
    assert!(org_yaml.contains("111"));
    assert!(org_yaml.contains("222"));
    assert!(!org_yaml.contains("999"));

    let bot_settings = read_json_value(&runtime.path().join("config").join("bot_settings.json"));
    let alpha_key = crate::services::discord::settings::discord_token_hash("discord-token-a");
    let beta_key = crate::services::discord::settings::discord_token_hash("discord-token-b");
    assert_eq!(
        bot_settings[&alpha_key]["allowed_channel_ids"],
        serde_json::json!([111u64])
    );
    assert_eq!(
        bot_settings[&beta_key]["allowed_channel_ids"],
        serde_json::json!([222u64])
    );
}

#[test]
fn ambiguous_discord_account_selection_stays_preview_only() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{
                "list":[
                    {"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}
                ]
            },
            "bindings":[
                {"agentId":"alpha","match":{"channel":"discord"}}
            ],
            "channels":{
                "discord":{
                    "accounts":{
                        "a":{
                            "token":"token-a",
                            "guilds":{"g1":{"channels":{"111":{"allow":true}}}}
                        },
                        "b":{
                            "token":"token-b",
                            "guilds":{"g2":{"channels":{"222":{"allow":true}}}}
                        }
                    }
                }
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.with_channel_bindings = true;
    let plan = build_import_plan(&source, &args, None).unwrap();

    assert!(plan.selected_discord_account_ids.is_empty());
    assert_eq!(plan.discord.bindings.len(), 1);
    assert_eq!(plan.discord.bindings[0].mode, "preview_only");
    assert!(plan.discord.bindings[0].selected_account_id.is_none());
}

#[test]
fn live_write_bot_settings_imports_plaintext_token_and_allowed_channels() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\n").unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]},
            "bindings":[{"agentId":"alpha","match":{"channel":"discord"}}],
            "channels":{
                "discord":{
                    "token":"discord-token-plaintext",
                    "guilds":{"g1":{"channels":{"1234567890":{"allow":true}}}}
                }
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;
    args.write_org = true;
    args.write_bot_settings = true;
    args.with_channel_bindings = true;
    args.discord_token_mode = "plaintext-only".to_string();

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    let bot_settings = read_json_value(&runtime.path().join("config").join("bot_settings.json"));
    let key = crate::services::discord::settings::discord_token_hash("discord-token-plaintext");
    assert_eq!(bot_settings[&key]["provider"], "codex");
    assert_eq!(bot_settings[&key]["agent"], "alpha");
    assert_eq!(
        bot_settings[&key]["allowed_channel_ids"],
        serde_json::json!([1234567890u64])
    );
}

#[test]
fn live_write_bot_settings_resolves_file_secret_tokens() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\n").unwrap();
    fs::write(
        temp.path().join("secrets.json"),
        r#"{"discord_token":"discord-token-file"}"#,
    )
    .unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]},
            "bindings":[{"agentId":"alpha","match":{"channel":"discord","accountId":"primary"}}],
            "secrets":{
                "defaults":{"file":"disk"},
                "providers":{"disk":{"source":"file","path":"./secrets.json","mode":"json"}}
            },
            "channels":{
                "discord":{
                    "accounts":{
                        "primary":{
                            "token":{"source":"file","provider":"disk","id":"discord_token"},
                            "guilds":{"g1":{"channels":{"333":{"allow":true}}}}
                        }
                    }
                }
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;
    args.write_org = true;
    args.write_bot_settings = true;
    args.with_channel_bindings = true;
    args.discord_token_mode = "resolve-env-file".to_string();

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    let bot_settings = read_json_value(&runtime.path().join("config").join("bot_settings.json"));
    let key = crate::services::discord::settings::discord_token_hash("discord-token-file");
    assert_eq!(bot_settings[&key]["provider"], "codex");
    assert_eq!(bot_settings[&key]["agent"], "alpha");
    assert_eq!(
        bot_settings[&key]["allowed_channel_ids"],
        serde_json::json!([333u64])
    );
}

#[test]
fn live_write_bot_settings_preserves_existing_allowlist_when_bindings_disabled() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\n").unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]},
            "bindings":[{"agentId":"alpha","match":{"channel":"discord"}}],
            "channels":{
                "discord":{
                    "token":"discord-token-plaintext",
                    "guilds":{"g1":{"channels":{"1234567890":{"allow":true}}}}
                }
            }
        }"#,
    );

    let existing_key =
        crate::services::discord::settings::discord_token_hash("discord-token-plaintext");
    fs::create_dir_all(runtime.path().join("config")).unwrap();
    fs::write(
        runtime.path().join("config").join("bot_settings.json"),
        serde_json::json!({
            existing_key.clone(): {
                "token": "discord-token-plaintext",
                "provider": "codex",
                "agent": "alpha",
                "allowed_channel_ids": [777u64]
            }
        })
        .to_string(),
    )
    .unwrap();

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;
    args.write_bot_settings = true;
    args.discord_token_mode = "plaintext-only".to_string();

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    let bot_settings = read_json_value(&runtime.path().join("config").join("bot_settings.json"));
    assert_eq!(
        bot_settings[&existing_key]["allowed_channel_ids"],
        serde_json::json!([777u64])
    );
}

#[test]
fn live_write_bot_settings_resolves_env_placeholder_without_provider() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\n").unwrap();

    let env_key = format!(
        "OPENCLAW_MIGRATE_TOKEN_{}",
        temp.path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace('-', "_")
    );
    unsafe {
        std::env::set_var(&env_key, "discord-token-env");
    }

    write_openclaw_config(
        temp.path(),
        &format!(
            r#"{{
                "agents":{{"list":[{{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}}]}},
                "bindings":[{{"agentId":"alpha","match":{{"channel":"discord"}}}}],
                "channels":{{
                    "discord":{{
                        "token":"${{{}}}",
                        "guilds":{{"g1":{{"channels":{{"1234567890":{{"allow":true}}}}}}}}
                    }}
                }}
            }}"#,
            env_key
        ),
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;
    args.write_org = true;
    args.write_bot_settings = true;
    args.with_channel_bindings = true;
    args.discord_token_mode = "resolve-env-file".to_string();

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    let bot_settings = read_json_value(&runtime.path().join("config").join("bot_settings.json"));
    let key = crate::services::discord::settings::discord_token_hash("discord-token-env");
    assert_eq!(bot_settings[&key]["provider"], "codex");
    assert_eq!(
        bot_settings[&key]["allowed_channel_ids"],
        serde_json::json!([1234567890u64])
    );

    unsafe {
        std::env::remove_var(&env_key);
    }
}

#[test]
fn live_write_bot_settings_skips_shared_account_without_live_channel_scope() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let alpha_workspace = temp.path().join("workspace-alpha");
    let beta_workspace = temp.path().join("workspace-beta");
    fs::create_dir_all(&alpha_workspace).unwrap();
    fs::create_dir_all(&beta_workspace).unwrap();
    fs::write(alpha_workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(alpha_workspace.join("MEMORY.md"), "# Memory\n").unwrap();
    fs::write(beta_workspace.join("IDENTITY.md"), "# Beta\n").unwrap();
    fs::write(beta_workspace.join("MEMORY.md"), "# Memory\n").unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{
                "list":[
                    {"id":"alpha","model":"openai/gpt-5","workspace":"workspace-alpha"},
                    {"id":"beta","model":"openai/gpt-5","workspace":"workspace-beta"}
                ]
            },
            "bindings":[
                {"agentId":"alpha","match":{"channel":"discord"}},
                {"agentId":"beta","match":{"channel":"discord"}}
            ],
            "channels":{
                "discord":{
                    "token":"discord-token-shared",
                    "guilds":{"g1":{"channels":{"1234567890":{"allow":true}}}}
                }
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.all_agents = true;
    args.dry_run = false;
    args.write_bot_settings = true;
    args.discord_token_mode = "plaintext-only".to_string();

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    assert!(
        plan.discord
            .bindings
            .iter()
            .all(|binding| binding.mode == "preview_only")
    );
    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    assert!(
        !runtime
            .path()
            .join("config")
            .join("bot_settings.json")
            .exists()
    );
}

#[test]
fn live_write_bot_settings_skips_preview_only_bindings() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\n").unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]},
            "bindings":[{"agentId":"alpha","match":{"channel":"discord"}}],
            "channels":{
                "discord":{
                    "accounts":{
                        "a":{
                            "token":"token-a",
                            "guilds":{"g1":{"channels":{"111":{"allow":true}}}}
                        },
                        "b":{
                            "token":"token-b",
                            "guilds":{"g2":{"channels":{"222":{"allow":true}}}}
                        }
                    }
                }
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;
    args.write_bot_settings = true;
    args.discord_token_mode = "plaintext-only".to_string();

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    assert_eq!(plan.discord.bindings[0].mode, "preview_only");

    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    assert!(
        !runtime
            .path()
            .join("config")
            .join("bot_settings.json")
            .exists()
    );
}

#[test]
fn session_import_writes_ai_sessions_session_map_and_db_rows() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let sessions_dir = temp.path().join("agents").join("alpha").join("sessions");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&sessions_dir).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\n").unwrap();
    fs::write(
        sessions_dir.join("sessions.json"),
        serde_json::json!({
            "session-key-1": {
                "sessionId": "session-1",
                "sessionFile": "session-1.jsonl",
                "updatedAt": 1710000000000i64,
                "model": "openai/gpt-5",
                "modelProvider": "codex",
                "cwd": workspace.display().to_string(),
                "lastChannel": "1234567890",
                "lastThreadId": "777",
                "status": "done"
            }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        sessions_dir.join("session-1.jsonl"),
        concat!(
            "{\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"name\":\"Read\",\"input\":{\"path\":\"README.md\"}}]}}\n",
            "{\"message\":{\"role\":\"tool\",\"content\":[{\"type\":\"tool_result\",\"result\":\"done\"}]}}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"finished\"}]}}\n"
        ),
    )
    .unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]}
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;
    args.with_sessions = true;
    args.write_db = true;

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    let audit_root = audit_root(&plan);
    let session_map = read_json_value(&audit_root.join("session-map.json"));
    let ai_session_path =
        std::path::PathBuf::from(session_map[0]["ai_session_path"].as_str().unwrap());
    assert!(ai_session_path.is_file());

    let ai_session = read_json_value(&ai_session_path);
    let history = ai_session["history"].as_array().unwrap();
    assert_eq!(history[0]["item_type"], "User");
    assert_eq!(history[1]["item_type"], "ToolUse");
    assert_eq!(history[2]["item_type"], "ToolResult");
    assert_eq!(history[3]["item_type"], "Assistant");

    let db_path = runtime.path().join("data").join("agentdesk.sqlite");
    let conn = crate::db::open_read_only_connection(&db_path).unwrap();
    let session_key = session_map[0]["db_session_key"].as_str().unwrap();
    let (agent_id, provider, status, model, cwd, session_info, last_heartbeat): (
        String,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
    ) =
        conn.query_row(
            "SELECT agent_id, provider, status, model, cwd, session_info, last_heartbeat FROM sessions WHERE session_key = ?1",
            [session_key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(agent_id, "alpha");
    assert_eq!(provider, "codex");
    assert_eq!(status, "idle");
    assert_eq!(model, "openai/gpt-5");
    assert!(
        std::path::Path::new(&cwd).ends_with(
            std::path::Path::new("openclaw")
                .join("workspaces")
                .join("alpha")
        )
    );
    assert!(session_info.contains("\"source_session_id\":\"session-1\""));
    assert!(last_heartbeat.is_none());
}

#[test]
fn session_import_keeps_claude_runtime_session_id_null_for_running_source_sessions() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let sessions_dir = temp.path().join("agents").join("alpha").join("sessions");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&sessions_dir).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\n").unwrap();
    fs::write(
        sessions_dir.join("sessions.json"),
        serde_json::json!({
            "session-key-1": {
                "sessionId": "claude-source-session-1",
                "sessionFile": "session-1.jsonl",
                "updatedAt": 1710000000000i64,
                "model": "anthropic/claude-3-5-sonnet-latest",
                "modelProvider": "claude",
                "cwd": workspace.display().to_string(),
                "status": "running"
            }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        sessions_dir.join("session-1.jsonl"),
        "{\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
    )
    .unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{"list":[{"id":"alpha","default":true,"model":"anthropic/claude-3-5-sonnet-latest","workspace":"workspace"}]}
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;
    args.with_sessions = true;
    args.write_db = true;

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    let audit_root = audit_root(&plan);
    let session_map = read_json_value(&audit_root.join("session-map.json"));
    let db_path = runtime.path().join("data").join("agentdesk.sqlite");
    let conn = crate::db::open_read_only_connection(&db_path).unwrap();
    let session_key = session_map[0]["db_session_key"].as_str().unwrap();
    let (status, claude_session_id, session_info, last_heartbeat): (
        String,
        Option<String>,
        String,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, claude_session_id, session_info, last_heartbeat FROM sessions WHERE session_key = ?1",
            [session_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();

    assert_eq!(status, "idle");
    assert!(claude_session_id.is_none());
    assert!(last_heartbeat.is_none());
    assert!(session_info.contains("\"source_session_id\":\"claude-source-session-1\""));
    assert!(session_info.contains("\"source_status\":\"running\""));
}

#[test]
fn resume_allows_enabling_sessions_without_fingerprint_mismatch() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let sessions_dir = temp.path().join("agents").join("alpha").join("sessions");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&sessions_dir).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\n").unwrap();
    fs::write(
        sessions_dir.join("sessions.json"),
        serde_json::json!({
            "session-key-1": {
                "sessionId": "session-1",
                "sessionFile": "session-1.jsonl",
                "updatedAt": 1710000000000i64,
                "model": "openai/gpt-5",
                "modelProvider": "codex",
                "cwd": workspace.display().to_string(),
                "status": "done"
            }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        sessions_dir.join("session-1.jsonl"),
        "{\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
    )
    .unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]}
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;
    args.write_db = true;

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    let audit_root = audit_root(&plan);
    let import_id = audit_root
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let mut apply_result = read_json_value(&audit_root.join("apply-result.json"));
    apply_result["status"] = serde_json::json!("failed");
    apply_result["phases"]["sessions"]["status"] = serde_json::json!("pending");
    apply_result["phases"]["sessions"]["started_at"] = serde_json::json!("");
    apply_result["phases"]["sessions"]["ended_at"] = serde_json::json!("");
    apply_result["phases"]["sessions"]["error"] = serde_json::Value::Null;
    apply_result["phases"]["finalize"]["status"] = serde_json::json!("pending");
    apply_result["phases"]["finalize"]["started_at"] = serde_json::json!("");
    apply_result["phases"]["finalize"]["ended_at"] = serde_json::json!("");
    apply_result["phases"]["finalize"]["error"] = serde_json::Value::Null;
    apply_result["agents"]["alpha"]["tasks"]["session_import"]["status"] =
        serde_json::json!("pending");
    fs::write(
        audit_root.join("apply-result.json"),
        serde_json::to_string_pretty(&apply_result).unwrap(),
    )
    .unwrap();

    let mut resume_state = read_json_value(&audit_root.join("resume-state.json"));
    resume_state["status"] = serde_json::json!("failed");
    resume_state["completed_phases"] = serde_json::json!([
        "scan",
        "map",
        "prompt",
        "memory",
        "policy_discord",
        "apply_files",
        "apply_bot_settings",
        "apply_db"
    ]);
    resume_state["pending_phases"] = serde_json::json!(["sessions", "finalize"]);
    resume_state["phases"]["sessions"] = serde_json::json!("pending");
    resume_state["phases"]["finalize"] = serde_json::json!("pending");
    resume_state["agents"]["alpha"]["tasks"]["session_import"] = serde_json::json!("pending");
    resume_state["next_recommended_step"] = serde_json::json!("resume_phase");
    fs::write(
        audit_root.join("resume-state.json"),
        serde_json::to_string_pretty(&resume_state).unwrap(),
    )
    .unwrap();

    let mut resume_args = base_args();
    resume_args.dry_run = false;
    resume_args.write_db = true;
    resume_args.with_sessions = true;
    resume_args.resume = Some(import_id);

    let resume_plan = build_import_plan(&source, &resume_args, Some(runtime.path())).unwrap();
    apply::apply_import_plan(&resume_plan, &source, &resume_args, runtime.path()).unwrap();

    let resumed_apply = read_json_value(&audit_root.join("apply-result.json"));
    assert_eq!(resumed_apply["status"], "completed");
    assert_eq!(resumed_apply["phases"]["sessions"]["status"], "completed");

    let session_map = read_json_value(&audit_root.join("session-map.json"));
    assert_eq!(session_map.as_array().unwrap().len(), 1);
}

#[test]
fn resume_skips_completed_apply_files_tasks() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(workspace.join("memory")).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\n").unwrap();
    fs::write(workspace.join("memory").join("2026-04-02.md"), "# Daily\n").unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]}
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;
    args.write_org = true;

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    let audit_root = audit_root(&plan);
    let import_id = audit_root
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let mut apply_result = read_json_value(&audit_root.join("apply-result.json"));
    apply_result["status"] = serde_json::json!("failed");
    apply_result["phases"]["apply_org"]["status"] = serde_json::json!("pending");
    apply_result["phases"]["apply_org"]["started_at"] = serde_json::json!("");
    apply_result["phases"]["apply_org"]["ended_at"] = serde_json::json!("");
    apply_result["phases"]["apply_org"]["error"] = serde_json::Value::Null;
    apply_result["phases"]["finalize"]["status"] = serde_json::json!("pending");
    apply_result["phases"]["finalize"]["started_at"] = serde_json::json!("");
    apply_result["phases"]["finalize"]["ended_at"] = serde_json::json!("");
    apply_result["phases"]["finalize"]["error"] = serde_json::Value::Null;
    apply_result["agents"]["alpha"]["tasks"]["org_agent_write"]["status"] =
        serde_json::json!("pending");
    fs::write(
        audit_root.join("apply-result.json"),
        serde_json::to_string_pretty(&apply_result).unwrap(),
    )
    .unwrap();

    let mut resume_state = read_json_value(&audit_root.join("resume-state.json"));
    resume_state["status"] = serde_json::json!("failed");
    resume_state["completed_phases"] = serde_json::json!([
        "scan",
        "map",
        "prompt",
        "memory",
        "policy_discord",
        "apply_files"
    ]);
    resume_state["pending_phases"] = serde_json::json!(["apply_org", "finalize"]);
    resume_state["phases"]["apply_org"] = serde_json::json!("pending");
    resume_state["phases"]["finalize"] = serde_json::json!("pending");
    resume_state["agents"]["alpha"]["tasks"]["org_agent_write"] = serde_json::json!("pending");
    resume_state["next_recommended_step"] = serde_json::json!("resume_phase");
    fs::write(
        audit_root.join("resume-state.json"),
        serde_json::to_string_pretty(&resume_state).unwrap(),
    )
    .unwrap();

    let mut resume_args = base_args();
    resume_args.dry_run = false;
    resume_args.write_org = true;
    resume_args.resume = Some(import_id);

    let resume_plan = build_import_plan(&source, &resume_args, Some(runtime.path())).unwrap();
    apply::apply_import_plan(&resume_plan, &source, &resume_args, runtime.path()).unwrap();

    assert!(
        !audit_root
            .join("backups")
            .join("openclaw")
            .join("workspaces")
            .join("alpha")
            .exists()
    );
    let resumed_apply = read_json_value(&audit_root.join("apply-result.json"));
    assert_eq!(resumed_apply["status"], "completed");
    assert_eq!(
        resumed_apply["phases"]["apply_files"]["status"],
        "completed"
    );
    assert_eq!(resumed_apply["phases"]["apply_org"]["status"], "completed");
}

#[test]
fn fails_when_every_selected_agent_is_unsupported_without_fallback() {
    let temp = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("workspace-alpha")).unwrap();
    fs::create_dir_all(temp.path().join("workspace-beta")).unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{
                "list":[
                    {"id":"alpha","model":"meta/llama-3","workspace":"workspace-alpha"},
                    {"id":"beta","model":"xai/grok-2","workspace":"workspace-beta"}
                ]
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.all_agents = true;
    let err = build_import_plan(&source, &args, None).unwrap_err();
    assert!(err.contains("No importable OpenClaw agents remain after provider mapping"));
}

#[test]
fn unsupported_agents_stay_audit_visible_but_are_skipped_on_apply() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("workspace-alpha")).unwrap();
    fs::create_dir_all(temp.path().join("workspace-beta")).unwrap();
    fs::write(
        temp.path().join("workspace-alpha").join("IDENTITY.md"),
        "# Alpha\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("workspace-alpha").join("MEMORY.md"),
        "# Memory\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("workspace-beta").join("IDENTITY.md"),
        "# Beta\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("workspace-beta").join("MEMORY.md"),
        "# Memory\n",
    )
    .unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{
                "list":[
                    {"id":"alpha","model":"openai/gpt-5","workspace":"workspace-alpha"},
                    {"id":"beta","model":"meta/llama-3","workspace":"workspace-beta"}
                ]
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.all_agents = true;
    args.dry_run = false;
    args.write_org = true;

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    assert_eq!(plan.selected_agent_ids, vec!["alpha", "beta"]);
    assert_eq!(plan.importable_agent_ids, vec!["alpha"]);
    let beta = plan
        .agents
        .iter()
        .find(|agent| agent.source_id == "beta")
        .unwrap();
    assert!(beta.tasks.iter().all(|task| task.mode == "disabled"));

    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    let agentdesk_yaml =
        fs::read_to_string(crate::runtime_layout::config_file_path(runtime.path())).unwrap();
    assert!(agentdesk_yaml.contains("id: alpha"));
    assert!(!agentdesk_yaml.contains("id: beta"));

    let audit_root = audit_root(&plan);
    let manifest = read_json_value(&audit_root.join("manifest.json"));
    assert_eq!(
        manifest["selected_agent_ids"],
        serde_json::json!(["alpha", "beta"])
    );
    let apply_result = read_json_value(&audit_root.join("apply-result.json"));
    assert_eq!(
        apply_result["agents"]["beta"]["tasks"]["workspace_copy"]["status"],
        "skipped"
    );
    assert!(
        apply_result["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value
                .as_str()
                .unwrap()
                .contains("Skipped non-importable OpenClaw agents"))
    );
}

#[test]
fn audit_reports_include_richer_tool_and_discord_details() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\n").unwrap();
    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{
                "list":[
                    {
                        "id":"alpha",
                        "default":true,
                        "model":"openai/gpt-5",
                        "workspace":"workspace",
                        "tools":{"allow":["TaskCreate","custom-agent-tool"]}
                    }
                ]
            },
            "tools":{
                "allow":["Bash","unknown-global"],
                "subagents":{"tools":{"allow":["Write"]}}
            },
            "bindings":[
                {"agentId":"alpha","match":{"channel":"discord","accountId":"primary"}},
                {"agentId":"alpha","match":{"channel":"discord","accountId":"primary","roles":["ops"]}}
            ],
            "channels":{
                "discord":{
                    "accounts":{
                        "primary":{
                            "token":"discord-token-primary",
                            "allowBots":"mentions",
                            "guilds":{
                                "g1":{
                                    "users":["user-1"],
                                    "roles":["role-1"],
                                    "tools":["Read","unknown-guild-tool"],
                                    "channels":{
                                        "123":{
                                            "allow":true,
                                            "users":["user-2"],
                                            "roles":["role-2"],
                                            "toolsBySender":{"*":{"allow":["Edit"]}}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;
    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    let audit_root = audit_root(&plan);
    let tool_report = read_json_value(&audit_root.join("tool-policy-report.json"));
    assert_eq!(tool_report["has_channel_scoped_policy"], true);
    assert_eq!(tool_report["has_sender_scoped_policy"], true);
    assert_eq!(tool_report["has_subagent_scoped_policy"], true);
    let agent_report = &tool_report["agents"][0];
    assert_eq!(agent_report["agent_id"], "alpha");
    assert!(
        agent_report["normalized_candidate_tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "TaskCreate")
    );
    assert!(
        agent_report["normalized_candidate_tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "Edit")
    );
    assert!(
        agent_report["unsupported_tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "custom-agent-tool")
    );
    assert!(
        agent_report["unsupported_tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "unknown-global")
    );

    let discord_report = read_json_value(&audit_root.join("discord-auth-report.json"));
    assert_eq!(discord_report["default_token_configured"], false);
    assert_eq!(discord_report["has_named_accounts"], true);
    assert_eq!(discord_report["requested_token_mode"], "report");
    let account = discord_report["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["account_id"] == "primary")
        .unwrap();
    assert_eq!(account["token_status"], "skipped");
    assert_eq!(account["guild_count"], 1);
    assert_eq!(account["channel_override_count"], 1);
    assert_eq!(account["user_allowlist_count"], 2);
    assert_eq!(account["role_allowlist_count"], 2);
    assert_eq!(account["binding_roles_present"], true);
    assert_eq!(account["allow_bots_enabled"], true);
    let mapping = discord_report["account_to_bot_mappings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["account_id"] == "primary")
        .unwrap();
    assert_eq!(mapping["mode"], "preview_only");
    assert!(mapping["live_channel_ids"].as_array().unwrap().is_empty());
    assert!(
        mapping["preview_only_binding_agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "alpha")
    );
}

#[test]
fn apply_fails_when_existing_agentdesk_yaml_is_invalid() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\n").unwrap();
    fs::write(runtime.path().join("agentdesk.yaml"), "server: [").unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]}
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    let err = apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap_err();
    assert!(err.contains("Failed to load"));
    assert!(err.contains("agentdesk.yaml"));
}

#[test]
fn apply_fails_when_existing_bot_settings_json_is_invalid() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(runtime.path().join("config")).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\n").unwrap();
    fs::write(
        runtime.path().join("config").join("bot_settings.json"),
        "{not valid json",
    )
    .unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]},
            "bindings":[{"agentId":"alpha","match":{"channel":"discord"}}],
            "channels":{
                "discord":{
                    "token":"discord-token-plaintext",
                    "guilds":{"g1":{"channels":{"1234567890":{"allow":true}}}}
                }
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;
    args.write_bot_settings = true;
    args.discord_token_mode = "plaintext-only".to_string();

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    let err = apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap_err();
    assert!(err.contains("bot_settings.json"));
    assert!(err.contains("Failed to parse"));
}

#[test]
fn report_mode_preserves_existing_allowed_tools_in_bot_settings() {
    let temp = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let workspace = temp.path().join("workspace");
    let config_dir = runtime.path().join("config");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(workspace.join("IDENTITY.md"), "# Alpha\n").unwrap();
    fs::write(workspace.join("MEMORY.md"), "# Memory\n").unwrap();
    fs::write(
        config_dir.join("bot_settings.json"),
        serde_json::json!({
            crate::services::discord::settings::discord_token_hash("discord-token-plaintext"): {
                "token": "discord-token-plaintext",
                "provider": "claude",
                "agent": "legacy",
                "allowed_tools": ["Read"],
                "allowed_channel_ids": [1]
            }
        })
        .to_string(),
    )
    .unwrap();

    write_openclaw_config(
        temp.path(),
        r#"{
            "agents":{"list":[{"id":"alpha","default":true,"model":"openai/gpt-5","workspace":"workspace"}]},
            "bindings":[{"agentId":"alpha","match":{"channel":"discord"}}],
            "channels":{
                "discord":{
                    "token":"discord-token-plaintext",
                    "guilds":{"g1":{"channels":{"1234567890":{"allow":true}}}}
                }
            }
        }"#,
    );

    let source = resolve_source(&temp);
    let mut args = base_args();
    args.dry_run = false;
    args.write_org = true;
    args.write_bot_settings = true;
    args.with_channel_bindings = true;
    args.discord_token_mode = "plaintext-only".to_string();
    args.tool_policy_mode = "report".to_string();

    let plan = build_import_plan(&source, &args, Some(runtime.path())).unwrap();
    apply::apply_import_plan(&plan, &source, &args, runtime.path()).unwrap();

    let bot_settings = read_json_value(&runtime.path().join("config").join("bot_settings.json"));
    let key = crate::services::discord::settings::discord_token_hash("discord-token-plaintext");
    assert_eq!(
        bot_settings[&key]["allowed_tools"],
        serde_json::json!(["Read"])
    );
    assert_eq!(
        bot_settings[&key]["allowed_channel_ids"],
        serde_json::json!([1234567890u64])
    );
}
