use super::*;
use serde_json::Value;

fn isolated(run: impl FnOnce(&Path)) {
    let name = std::thread::current().name().unwrap().to_string();
    if std::env::var("ADK_SKILL_PROVIDER_TEST").as_deref() == Ok(&name) {
        run(&PathBuf::from(std::env::var_os("HOME").unwrap()).join("runtime"));
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env("ADK_SKILL_PROVIDER_TEST", &name)
        .env("HOME", temp.path())
        .env("USERPROFILE", temp.path())
        .env("AGENTDESK_ROOT_DIR", temp.path().join("runtime"))
        .env_remove("AGENTDESK_CONFIG")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture(root: &Path, providers: Value) -> (PathBuf, PathBuf) {
    let skill = managed_skills_root(root).join("demo");
    fs::create_dir_all(&skill).unwrap();
    fs::write(skill.join("SKILL.md"), "demo").unwrap();
    let workspace = root.join("workspaces/project");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(
        managed_skills_manifest_path(root),
        serde_json::to_string(&serde_json::json!({
            "skills": {"demo": {"providers":providers, "global": false, "workspaces":["project"]}}
        }))
        .unwrap(),
    )
    .unwrap();
    (skill, workspace)
}

#[test]
fn retired_gemini_is_not_recreated_by_default_explicit_or_legacy_manifests() {
    isolated(|root| {
        for providers in [serde_json::json!([]), serde_json::json!(["gemini"])] {
            let retired_only = providers == serde_json::json!(["gemini"]);
            let (_, workspace) = fixture(root, providers);
            let report = sync_managed_skills(root).unwrap();
            assert!(!workspace.join(".gemini/skills").exists());
            if retired_only {
                assert_eq!(report.created_links, 0);
                for path in [
                    ".claude/commands/demo.md",
                    ".codex/skills/demo",
                    ".qwen/skills/demo",
                    ".agents/skills/demo",
                ] {
                    assert!(!workspace.join(path).exists());
                }
            }
        }
        let (_, workspace) = fixture(root, serde_json::json!([]));
        fs::write(
            managed_skills_manifest_path(root),
            serde_json::to_string(&serde_json::json!({
                "demo":{"targets":[format!("gemini@{}",workspace.display())]}
            }))
            .unwrap(),
        )
        .unwrap();
        sync_managed_skills(root).unwrap();
        assert!(!workspace.join(".gemini/skills").exists());
    });
}

#[test]
fn retired_gemini_cleans_only_skill_symlinks_even_without_managed_skills() {
    isolated(|root| {
        let (skill, workspace) = fixture(root, serde_json::json!(["codex"]));
        let links = workspace.join(".gemini/skills");
        fs::create_dir_all(&links).unwrap();
        create_symlink_entry(&skill, &links.join("demo"), true).unwrap();
        fs::write(skill.join("source.txt"), "preserve").unwrap();
        ensure_runtime_layout(root).unwrap();
        sync_managed_skills(root).unwrap();
        assert!(!links.exists());
        assert_eq!(fs::read_to_string(skill.join("SKILL.md")).unwrap(), "demo");
        assert_eq!(
            fs::read_to_string(skill.join("source.txt")).unwrap(),
            "preserve"
        );
        sync_managed_skills(root).unwrap();
        let empty = root.join("workspaces/empty/.gemini/skills");
        fs::create_dir_all(&empty).unwrap();
        fs::remove_file(skill.join("SKILL.md")).unwrap();
        sync_managed_skills(root).unwrap();
        assert!(!empty.exists());
    });
}

#[test]
fn retired_gemini_removes_dangling_links_alone_or_with_skill_links() {
    isolated(|root| {
        let (skill, workspace) = fixture(root, serde_json::json!(["codex"]));
        let links = workspace.join(".gemini/skills");
        for with_skill in [false, true] {
            fs::create_dir_all(&links).unwrap();
            create_symlink_entry(&root.join("skills/removed"), &links.join("removed"), true)
                .unwrap();
            assert_eq!(
                fs::metadata(links.join("removed")).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
            if with_skill {
                create_symlink_entry(&skill, &links.join("demo"), true).unwrap();
            }
            sync_managed_skills(root).unwrap();
            assert!(!links.exists(), "with_skill={with_skill}");
            assert_eq!(fs::read_to_string(skill.join("SKILL.md")).unwrap(), "demo");
        }
    });
}

#[test]
fn retired_gemini_preserves_dangling_links_mixed_with_user_entries() {
    isolated(|root| {
        for entry in ["file", "directory", "file-link", "directory-link"] {
            let case = root.join(entry);
            let (skill, workspace) = fixture(&case, serde_json::json!(["codex"]));
            let links = workspace.join(".gemini/skills");
            fs::create_dir_all(&links).unwrap();
            create_symlink_entry(&skill, &links.join("demo"), true).unwrap();
            create_symlink_entry(&case.join("missing"), &links.join("removed"), true).unwrap();
            let user = links.join("user");
            match entry {
                "file" => fs::write(&user, "preserve").unwrap(),
                "directory" => fs::create_dir(&user).unwrap(),
                "file-link" => {
                    let target = workspace.join("user.txt");
                    fs::write(&target, "preserve").unwrap();
                    create_symlink_entry(&target, &user, false).unwrap();
                }
                "directory-link" => {
                    let target = workspace.join("user-dir");
                    fs::create_dir(&target).unwrap();
                    create_symlink_entry(&target, &user, true).unwrap();
                }
                _ => unreachable!(),
            }
            sync_managed_skills(&case).unwrap();
            assert!(user.exists(), "{entry}");
            assert!(
                fs::symlink_metadata(links.join("removed")).is_ok(),
                "{entry}"
            );
            assert!(fs::symlink_metadata(links.join("demo")).is_ok(), "{entry}");
            if matches!(entry, "file" | "file-link") {
                assert_eq!(fs::read_to_string(&user).unwrap(), "preserve");
            }
        }
    });
}

#[cfg(unix)]
#[test]
fn retired_gemini_preserves_links_with_non_not_found_metadata_errors() {
    isolated(|root| {
        let (skill, workspace) = fixture(root, serde_json::json!(["codex"]));
        let links = workspace.join(".gemini/skills");
        fs::create_dir_all(&links).unwrap();
        create_symlink_entry(&skill, &links.join("demo"), true).unwrap();
        let looping = links.join("loop");
        create_symlink_entry(&looping, &looping, true).unwrap();
        assert_ne!(
            fs::metadata(&looping).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        sync_managed_skills(root).unwrap();
        assert!(
            fs::symlink_metadata(&looping)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(fs::symlink_metadata(links.join("demo")).is_ok());
        assert_eq!(fs::read_to_string(skill.join("SKILL.md")).unwrap(), "demo");
    });
}

#[cfg(windows)]
#[test]
fn retired_gemini_removes_deleted_directory_symlinks_windows() {
    isolated(|root| {
        use std::os::windows::fs::FileTypeExt;

        let (skill, workspace) = fixture(root, serde_json::json!(["codex"]));
        let links = workspace.join(".gemini/skills");
        for with_skill in [false, true] {
            fs::create_dir_all(&links).unwrap();
            let source = root.join("skills/removed");
            fs::create_dir(&source).unwrap();
            let link = links.join("removed");
            std::os::windows::fs::symlink_dir(&source, &link).unwrap();
            fs::remove_dir(&source).unwrap();
            assert_eq!(
                fs::metadata(&link).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink_dir()
            );
            if with_skill {
                create_symlink_entry(&skill, &links.join("demo"), true).unwrap();
            }
            sync_managed_skills(root).unwrap();
            assert_eq!(
                fs::symlink_metadata(&link).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
            assert_eq!(
                fs::symlink_metadata(&links).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
            assert_eq!(fs::read_to_string(skill.join("SKILL.md")).unwrap(), "demo");
        }
    });
}

#[cfg(windows)]
#[test]
fn windows_link_removal_preserves_live_targets_and_removes_dangling_links() {
    isolated(|root| {
        use std::os::windows::fs::FileTypeExt;

        fs::create_dir_all(root).unwrap();
        for kind in ["file", "directory", "junction"] {
            for dangling in [false, true] {
                let target = root.join(format!("{kind}-{dangling}-target"));
                let link = root.join(format!("{kind}-{dangling}-link"));
                let content = if kind == "file" {
                    target.clone()
                } else {
                    fs::create_dir(&target).unwrap();
                    target.join("owned.txt")
                };
                fs::write(&content, "preserve").unwrap();
                match kind {
                    "file" => std::os::windows::fs::symlink_file(&target, &link).unwrap(),
                    "directory" => std::os::windows::fs::symlink_dir(&target, &link).unwrap(),
                    "junction" => {
                        crate::runtime_layout::windows_links::create_directory_junction(
                            &target, &link,
                        )
                        .unwrap();
                    }
                    _ => unreachable!(),
                }
                let file_type = fs::symlink_metadata(&link).unwrap().file_type();
                assert_eq!(file_type.is_symlink_file(), kind == "file");
                assert_eq!(file_type.is_symlink_dir(), kind != "file");
                if dangling {
                    fs::remove_file(&content).unwrap();
                    if kind != "file" {
                        fs::remove_dir(&target).unwrap();
                    }
                    assert_eq!(
                        fs::metadata(&link).unwrap_err().kind(),
                        std::io::ErrorKind::NotFound
                    );
                }
                remove_link_or_path(&link).unwrap();
                assert_eq!(
                    fs::symlink_metadata(&link).unwrap_err().kind(),
                    std::io::ErrorKind::NotFound
                );
                if !dangling {
                    assert_eq!(fs::read_to_string(&content).unwrap(), "preserve");
                }
            }
        }
    });
}

#[test]
fn retired_gemini_preserves_mixed_files_unknown_links_and_directory_aliases() {
    isolated(|root| {
        let (skill, workspace) = fixture(root, serde_json::json!(["codex"]));
        let links = workspace.join(".gemini/skills");
        fs::create_dir_all(&links).unwrap();
        create_symlink_entry(&skill, &links.join("demo"), true).unwrap();
        fs::write(links.join("user.txt"), "preserve").unwrap();
        fs::create_dir_all(workspace.join(".gemini/config")).unwrap();
        fs::write(
            workspace.join(".gemini/config/mcp_config.json"),
            "antigravity",
        )
        .unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let output = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || Capture(output.clone()))
            .finish();
        crate::logging::test_capture::pin_callsite_interest();
        tracing::subscriber::with_default(subscriber, || sync_managed_skills(root).unwrap());
        assert!(
            String::from_utf8(captured.lock().unwrap().clone())
                .unwrap()
                .contains("Preserving Gemini skills directory")
        );
        assert_eq!(
            fs::read_to_string(links.join("user.txt")).unwrap(),
            "preserve"
        );
        assert!(
            fs::symlink_metadata(links.join("demo"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(workspace.join(".gemini/config/mcp_config.json")).unwrap(),
            "antigravity"
        );
        fs::remove_file(links.join("user.txt")).unwrap();
        fs::write(workspace.join("unknown"), "user").unwrap();
        create_symlink_entry(&workspace.join("unknown"), &links.join("unknown"), false).unwrap();
        sync_managed_skills(root).unwrap();
        assert!(links.join("unknown").exists());
        for level in ["workspaces", "workspace", ".gemini", "skills"] {
            let case = root.join(level);
            let outside = tempfile::tempdir().unwrap();
            let (skill, _) = fixture(&case, serde_json::json!(["codex"]));
            let (alias, target, skills) = match level {
                "workspaces" => (
                    case.join("workspaces"),
                    outside.path().join("workspaces"),
                    "project/.gemini/skills",
                ),
                "workspace" => (
                    case.join("workspaces/project"),
                    outside.path().join("project"),
                    ".gemini/skills",
                ),
                ".gemini" => (
                    case.join("workspaces/project/.gemini"),
                    outside.path().join(".gemini"),
                    "skills",
                ),
                _ => (
                    case.join("workspaces/project/.gemini/skills"),
                    outside.path().join("skills"),
                    "",
                ),
            };
            let links = target.join(skills);
            fs::create_dir_all(&links).unwrap();
            create_symlink_entry(&skill, &links.join("demo"), true).unwrap();
            if alias.exists() {
                if level == "workspaces" {
                    fs::remove_dir(alias.join("project")).unwrap();
                }
                fs::remove_dir(&alias).unwrap();
            }
            fs::create_dir_all(alias.parent().unwrap()).unwrap();
            create_symlink_entry(&target, &alias, true).unwrap();
            captured.lock().unwrap().clear();
            let output = captured.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_writer(move || Capture(output.clone()))
                .finish();
            tracing::subscriber::with_default(subscriber, || sync_managed_skills(&case).unwrap());
            assert!(
                fs::symlink_metadata(&alias)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{level}"
            );
            assert!(
                fs::symlink_metadata(links.join("demo"))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{level}"
            );
            assert_eq!(fs::read_to_string(skill.join("SKILL.md")).unwrap(), "demo");
            assert!(
                String::from_utf8(captured.lock().unwrap().clone())
                    .unwrap()
                    .contains("Preserving"),
                "{level}"
            );
        }
    });
}

struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn antigravity_skills_support_default_explicit_alias_and_pruning_without_touching_codex() {
    isolated(|root| {
        for providers in [
            serde_json::json!([]),
            serde_json::json!(["antigravity"]),
            serde_json::json!(["agy", "antigravity", "codex"]),
        ] {
            let (skill, workspace) = fixture(root, providers);
            sync_managed_skills(root).unwrap();
            let agy = workspace.join(".agents/skills/demo");
            assert!(same_canonical_path(&agy, &skill));
            assert!(fs::symlink_metadata(&agy).unwrap().file_type().is_symlink());
            let first = fs::read_link(&agy).unwrap();
            assert_eq!(sync_managed_skills(root).unwrap().created_links, 0);
            assert_eq!(fs::read_link(&agy).unwrap(), first);
        }
        let (_, workspace) = fixture(root, serde_json::json!(["codex"]));
        sync_managed_skills(root).unwrap();
        assert!(!workspace.join(".agents/skills/demo").exists());
        assert!(workspace.join(".codex/skills/demo/SKILL.md").exists());
        let shared = workspace.join(".agents/skills");
        fs::remove_dir(&shared).unwrap();
        create_symlink_entry(&workspace.join(".codex/skills"), &shared, true).unwrap();
        let (skill, _) = fixture(root, serde_json::json!(["antigravity", "codex"]));
        remove_link_or_path(&workspace.join(".codex/skills/demo")).unwrap();
        assert_eq!(sync_managed_skills(root).unwrap().created_links, 1);
        fixture(root, serde_json::json!(["antigravity"]));
        assert_eq!(sync_managed_skills(root).unwrap().created_links, 0);
        assert!(same_canonical_path(&shared.join("demo"), &skill));
        assert!(workspace.join(".codex/skills/demo/SKILL.md").exists());
    });
}
