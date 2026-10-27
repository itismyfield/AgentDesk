use super::*;

fn body(path: &Path, id: &str) -> AgentSetupBody {
    AgentSetupBody {
        agent_id: id.into(),
        channel_id: "123456789012345678".into(),
        provider: "claude".into(),
        prompt_template_path: path.display().to_string(),
        skills: vec![],
        dry_run: true,
    }
}

#[test]
fn setup_rejects_absolute_and_parent_templates_outside_runtime_root() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("runtime");
    std::fs::create_dir(&root).unwrap();
    let outside = parent.path().join("outside.md");
    std::fs::write(&outside, "outside").unwrap();
    let _env = crate::config::test_env::set_agentdesk_root_for_test(&root);
    for path in [outside, PathBuf::from("../outside.md")] {
        let errors = SetupContext::from_body(body(&path, "valid-agent_2"))
            .expect_err("outside template must be rejected before planning");
        assert!(
            errors
                .iter()
                .any(|e| e.kind == "validation" && e.message.contains("runtime root"))
        );
    }
    assert!(!crate::runtime_layout::config_file_path(&root).exists());
    assert!(!root.join("workspaces").exists());
}

#[cfg(unix)]
#[test]
fn setup_rejects_symlink_escape_and_accepts_internal_symlink() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("runtime");
    std::fs::create_dir(&root).unwrap();
    let outside = parent.path().join("outside.md");
    std::fs::write(&outside, "outside").unwrap();
    std::fs::write(root.join("inside.md"), "inside").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("escape.md")).unwrap();
    std::os::unix::fs::symlink(root.join("inside.md"), root.join("alias.md")).unwrap();
    let _env = crate::config::test_env::set_agentdesk_root_for_test(&root);
    assert!(SetupContext::from_body(body(Path::new("escape.md"), "valid")).is_err());
    let ctx = SetupContext::from_body(body(Path::new("alias.md"), "valid")).unwrap();
    assert_eq!(
        ctx.prompt_template_path,
        root.join("inside.md").canonicalize().unwrap()
    );
}

#[test]
fn setup_preserves_root_config_relative_absolute_and_legacy_id_formats() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("config")).unwrap();
    std::fs::write(root.path().join("template.md"), "root").unwrap();
    std::fs::write(root.path().join("config/legacy.md"), "config").unwrap();
    let _env = crate::config::test_env::set_agentdesk_root_for_test(root.path());
    for path in [
        PathBuf::from("template.md"),
        PathBuf::from("legacy.md"),
        root.path().join("template.md"),
    ] {
        for id in [
            "agent",
            "project-agentdesk",
            "agent_2",
            "GPT-6_1",
            "123",
            "-",
            "_",
        ] {
            let ctx = SetupContext::from_body(body(&path, id)).unwrap();
            assert_eq!(ctx.agent_id, id);
            assert!(
                ctx.prompt_template_path
                    .starts_with(root.path().canonicalize().unwrap())
            );
        }
    }
    for id in [
        "",
        ".",
        "..",
        "../bad",
        "a/b",
        "a\\b",
        "a.b",
        "a b",
        "에이전트",
        "a\0b",
    ] {
        assert!(
            SetupContext::from_body(body(Path::new("template.md"), id)).is_err(),
            "{id:?}"
        );
    }
}

#[test]
fn setup_rejects_missing_template_as_validation() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::test_env::set_agentdesk_root_for_test(root.path());
    let errors = SetupContext::from_body(body(Path::new("missing.md"), "valid"))
        .expect_err("missing template must fail closed");
    assert!(errors.iter().any(|e| e.kind == "validation"));
}
