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
            vec![manifest.clone()],
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
    assert!(check.detail.contains(&unlisted.display().to_string()));
    assert!(check.detail.contains(&manifest.display().to_string()));
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
