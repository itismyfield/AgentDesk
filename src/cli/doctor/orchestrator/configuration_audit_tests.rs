use super::super::{CheckStatus, FixSafety};
use super::*;
use std::collections::BTreeSet;

fn prompt_fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("workspaces/agentdesk");
    let inventory = repo.join("src/server/routes/docs/inventory/endpoints");
    std::fs::create_dir_all(&inventory).unwrap();
    std::fs::write(inventory.join("mod.rs"), "mod part_01;\n").unwrap();
    std::fs::write(
        inventory.join("part_01.rs"),
        "ep(\"POST\", \"/api/discord/send\", \"ops\", \"send\")\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    std::fs::write(
        repo.join("scripts/check_prompt_api_routes.py"),
        include_str!("../../../../scripts/check_prompt_api_routes.py"),
    )
    .unwrap();
    let prompts = crate::runtime_layout::managed_agents_root(temp.path());
    std::fs::create_dir_all(&prompts).unwrap();
    (temp, repo, prompts)
}

#[test]
fn mirror_prompt_fixture_reports_file_and_unknown_path_and_clean_has_zero() {
    let (_temp, repo, prompts) = prompt_fixture();
    let shared = prompts.join("_shared.prompt.md");
    std::fs::write(&shared, "curl http://localhost:1234/api/missing\n").unwrap();
    let check = prompt_api_routes_check(&repo, &prompts);
    assert!(check.status == CheckStatus::Warn);
    assert!(
        check
            .detail
            .contains("_shared.prompt.md:1: unknown_path /api/missing")
    );
    assert_eq!(check.fix_safety, FixSafety::ReadOnly);
    std::fs::write(&shared, "<!-- http://localhost:1234/api/missing -->\n> http://localhost:1234/api/missing\n```text\nhttp://localhost:1234/api/missing\n```\ncurl http://localhost:1234/api/discord/send\n").unwrap();
    let clean = prompt_api_routes_check(&repo, &prompts);
    assert!(clean.status == CheckStatus::Pass);
    assert!(clean.detail.contains("unknown_paths=0"));
    assert!(!clean.detail.contains("unknown_path /"));
}

#[test]
fn missing_unreadable_prompt_or_inventory_is_unavailable_instead_of_pass() {
    let (temp, repo, prompts) = prompt_fixture();
    let empty = prompt_api_routes_check(&repo, &prompts);
    assert!(empty.status == CheckStatus::Warn);
    assert!(empty.detail.contains("no_prompt_files"));
    std::fs::write(temp.path().join("file"), "not a directory").unwrap();
    for unavailable in [temp.path().join("missing"), temp.path().join("file")] {
        let check = prompt_api_routes_check(&repo, &unavailable);
        assert!(check.status == CheckStatus::Warn);
        assert!(check.detail.contains("inspection_unavailable"));
    }
    let prompt = prompts.join("broken.prompt.md");
    std::fs::write(&prompt, [0xff, 0xfe]).unwrap();
    let unreadable = prompt_api_routes_check(&repo, &prompts);
    assert!(unreadable.status == CheckStatus::Warn);
    assert!(unreadable.detail.contains("inspection_unavailable"));
    std::fs::write(&prompt, "curl http://localhost:1234/api/discord/send").unwrap();
    std::fs::remove_file(repo.join("src/server/routes/docs/inventory/endpoints/mod.rs")).unwrap();
    let missing_inventory = prompt_api_routes_check(&repo, &prompts);
    assert!(missing_inventory.status == CheckStatus::Warn);
    assert!(missing_inventory.detail.contains("inspection_unavailable"));
}

#[test]
fn skill_fixture_reports_stale_agent_and_unlisted_directory_and_clean_has_zero() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = temp.path().join("manifest.json");
    let unlisted = temp.path().join("unlisted");
    std::fs::create_dir(&unlisted).unwrap();
    std::fs::write(unlisted.join("SKILL.md"), "skill").unwrap();
    let ids: BTreeSet<String> = ["live".to_string()].into();
    let audit = || {
        skills::audit_skill_manifest_agents(skills::build_manifest_audit_request(
            ids.clone(),
            Some(ids.clone()),
            Some(ids.clone()),
            vec![(
                manifest.clone(),
                crate::server::routes::skills_manifest_audit::DirectoryScope::OwnManifest,
            )],
        ))
    };
    std::fs::write(
        &manifest,
        r#"{"skills":{"not-installed":{"agents":["ghost-agent"]}}}"#,
    )
    .unwrap();
    let check = skill_manifest_check(&audit());
    assert!(check.status == CheckStatus::Warn);
    assert!(check.detail.contains("ghost-agent"));
    // The detail embeds paths as JSON strings, which escape Windows separators.
    let json_path =
        |path: &std::path::Path| serde_json::json!(path.display().to_string()).to_string();
    assert!(check.detail.contains(&json_path(&unlisted)));
    assert!(check.detail.contains(&json_path(&manifest)));
    assert_eq!(
        check.evidence.as_ref().unwrap()["findings"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(check.fix_safety, FixSafety::ReadOnly);
    std::fs::write(
        &manifest,
        r#"{"skills":{"not-installed":{"agents":["live"]},"unlisted":{}}}"#,
    )
    .unwrap();
    let clean = skill_manifest_check(&audit());
    assert!(clean.status == CheckStatus::Pass);
    assert_eq!(clean.evidence.unwrap()["findings"], serde_json::json!([]));
}

#[test]
fn runtime_cache_copies_of_vault_listed_skills_are_not_unlisted() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let home = temp.path().join("home");
    let cache = crate::runtime_layout::managed_skills_root(&runtime);
    let vault = home.join("ObsidianVault/RemoteVault/99_Skills");
    for dir in [&cache, &vault] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(
        cache.join("manifest.json"),
        r#"{"skills":{"memory-read":{}}}"#,
    )
    .unwrap();
    std::fs::write(
        vault.join("manifest.json"),
        r#"{"skills":{"named":{}},"create-issue":{"agents":["*"]},"global_core_skills":["core"]}"#,
    )
    .unwrap();
    for dir in [
        cache.join("create-issue"),
        cache.join("named"),
        cache.join("core"),
        vault.join("create-issue"),
    ] {
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "skill").unwrap();
        assert!(
            !std::fs::symlink_metadata(&dir)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    let audit = || {
        let ids: BTreeSet<String> = ["live".to_string()].into();
        skills::audit_skill_manifest_agents(skills::build_manifest_audit_request(
            ids.clone(),
            Some(ids),
            Some(BTreeSet::new()),
            skills::skill_manifest_paths(Some(runtime.clone()), Some(home.clone())),
        ))
    };
    let clean = audit();
    assert_eq!(clean.to_json()["findings"], serde_json::json!([]));
    assert_eq!(clean.warning(), None);
    assert!(skill_manifest_check(&clean).status == CheckStatus::Pass);
    let orphan = cache.join("orphan");
    std::fs::create_dir(&orphan).unwrap();
    std::fs::write(orphan.join("SKILL.md"), "skill").unwrap();
    let bad = audit();
    assert_eq!(bad.to_json()["findings"].as_array().unwrap().len(), 1);
    assert_eq!(
        bad.to_json()["findings"][0]["path"],
        orphan.display().to_string()
    );
    assert!(skill_manifest_check(&bad).status == CheckStatus::Warn);
    assert!(bad.warning().is_some());
}

#[test]
fn source_directories_still_require_their_own_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let home = temp.path().join("home");
    let cache = crate::runtime_layout::managed_skills_root(&runtime);
    let vault = home.join("ObsidianVault/RemoteVault/99_Skills");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(vault.join("borrowed")).unwrap();
    std::fs::write(cache.join("manifest.json"), r#"{"skills":{"borrowed":{}}}"#).unwrap();
    std::fs::write(vault.join("manifest.json"), "{}").unwrap();
    std::fs::write(vault.join("borrowed/SKILL.md"), "skill").unwrap();
    let ids: BTreeSet<String> = ["live".to_string()].into();
    let report = skills::audit_skill_manifest_agents(skills::build_manifest_audit_request(
        ids.clone(),
        Some(ids),
        Some(BTreeSet::new()),
        skills::skill_manifest_paths(Some(runtime), Some(home)),
    ));
    assert_eq!(report.to_json()["findings"].as_array().unwrap().len(), 1);
    // Compare as paths: the vault root is built from separate components on Windows.
    assert_eq!(
        std::path::Path::new(report.to_json()["findings"][0]["path"].as_str().unwrap()),
        vault.join("borrowed")
    );
}

#[test]
fn boot_warning_caps_sample_but_doctor_and_api_keep_all_findings() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = temp.path().join("manifest.json");
    std::fs::write(&manifest, "{}").unwrap();
    for i in 0..10 {
        let dir = temp.path().join(format!("skill{i:02}"));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "skill").unwrap();
    }
    let ids: BTreeSet<String> = ["live".to_string()].into();
    let report = skills::audit_skill_manifest_agents(skills::build_manifest_audit_request(
        ids.clone(),
        Some(ids),
        Some(BTreeSet::new()),
        vec![(manifest, skills::DirectoryScope::OwnManifest)],
    ));
    let warning = report.warning().unwrap();
    assert!(warning.contains("findings=10 skipped=[]"));
    let sample: serde_json::Value =
        serde_json::from_str(warning.split_once(" sample=").unwrap().1).unwrap();
    assert_eq!(sample.as_array().unwrap().len(), 5);
    assert!(!warning.contains("skill09"));
    assert_eq!(report.to_json()["findings"].as_array().unwrap().len(), 10);
    let check = skill_manifest_check(&report);
    assert_eq!(
        check.evidence.unwrap()["findings"]
            .as_array()
            .unwrap()
            .len(),
        10
    );
    assert!(check.detail.contains("skill09"));
}

#[test]
fn sleeping_prompt_checker_is_timed_out_and_reported_unavailable() {
    let (_temp, repo, prompts) = prompt_fixture();
    std::fs::write(
        repo.join("scripts/check_prompt_api_routes.py"),
        "import time\ntime.sleep(1)\nprint('finished')\n",
    )
    .unwrap();
    let start = std::time::Instant::now();
    let check =
        prompt_api_routes_check_with_timeout(&repo, &prompts, std::time::Duration::from_millis(40));
    assert!(check.status == CheckStatus::Warn);
    assert!(check.detail.contains("inspection_unavailable: timeout"));
    assert!(start.elapsed() < std::time::Duration::from_millis(600));
}

#[test]
fn prompt_checker_stdout_is_drained_before_child_exit() {
    let (_temp, repo, prompts) = prompt_fixture();
    std::fs::write(
        repo.join("scripts/check_prompt_api_routes.py"),
        "print('x' * 200000)\n",
    )
    .unwrap();
    let check =
        prompt_api_routes_check_with_timeout(&repo, &prompts, std::time::Duration::from_secs(2));
    assert!(check.status == CheckStatus::Pass);
    assert!(check.detail.len() >= 200000);
}
