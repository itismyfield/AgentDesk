use super::*;
use std::path::Path;

fn ids(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(ToString::to_string).collect()
}

fn manifest_at(dir: &Path, body: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("manifest.json");
    std::fs::write(&path, body).unwrap();
    path
}

fn audit(
    config: &[&str],
    db: Option<&[&str]>,
    workspaces: Option<&[&str]>,
    manifests: Vec<PathBuf>,
) -> ManifestAuditReport {
    audit_skill_manifest_agents(build_manifest_audit_request(
        ids(config),
        db.map(ids),
        workspaces.map(ids),
        manifests,
    ))
}

fn skips(report: &ManifestAuditReport) -> Vec<&'static str> {
    report.skipped.iter().copied().collect()
}

#[test]
fn only_the_bare_wildcard_is_reserved() {
    let raw: Vec<String> = ["  agentdesk ", "", "   ", "*", "ch-*", "claude"]
        .iter()
        .map(ToString::to_string)
        .collect();
    // `ch-*` is a glob the Python distributor expands; this audit must not
    // reinterpret it, and blanks follow the existing trim/drop handling.
    assert_eq!(
        pinned_agent_ids(&raw),
        ids(&["agentdesk", "ch-*", "claude"])
    );
}

#[test]
fn union_covers_db_only_ids_and_a_failed_source_stops_grading() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = manifest_at(temp.path(), r#"{"skills":{"s":{"agents":["db-only"]}}}"#);
    let walked = Some(&["db-only"][..]);

    // Config alone does not know `db-only`; the db half of the union does.
    let united = audit(
        &["cfg-only"],
        Some(&["db-only"][..]),
        walked,
        vec![manifest.clone()],
    );
    assert!(united.skipped.is_empty(), "{:?}", skips(&united));
    assert!(united.findings.is_empty(), "{:?}", united.findings);

    let db_dropped = audit(
        &["cfg-only"],
        Some(&[] as &[&str]),
        walked,
        vec![manifest.clone()],
    );
    assert_eq!(db_dropped.findings[0]["agent_id"], "db-only");
    assert!(skips(&db_dropped).is_empty());

    let db_failed = audit(&["cfg-only"], None, walked, vec![manifest]);
    assert!(db_failed.findings.is_empty(), "{:?}", db_failed.findings);
    assert_eq!(skips(&db_failed), vec![ROSTER_SOURCE_UNAVAILABLE]);
}

#[test]
fn an_empty_roster_skips_instead_of_reporting_a_clean_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = manifest_at(
        temp.path(),
        r#"{"skills":{"s":{"workspaces":["agentdesk"]}}}"#,
    );

    // MX-G: a roster that came back empty must never read as audited-clean.
    let report = audit(
        &[],
        Some(&[] as &[&str]),
        Some(&["agentdesk"][..]),
        vec![manifest],
    );
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(skips(&report), vec![EMPTY_ROSTER]);
    assert_eq!(report.to_json()["audited"], serde_json::json!(false));
}

#[test]
fn flat_agent_findings_keep_workspace_directory_names_separate() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = manifest_at(
        temp.path(),
        r#"{
            "version": 1,
            "global_core_skills": ["core"],
            "skills": {"nested-skill": {"workspaces": ["agentdesk", "*"]}},
            "flat-skill": {"targets": ["claude"], "agents": ["ch-td"]},
            "core-skill": {"targets": ["claude"], "agents": ["*"]}
        }"#,
    );

    let report = audit(
        &["other"],
        Some(&["other"][..]),
        Some(&["agentdesk"][..]),
        vec![manifest],
    );
    assert!(skips(&report).is_empty());
    assert_eq!(report.findings[0]["agent_id"], "ch-td");
}

#[test]
fn nested_ids_are_graded_only_against_directories_the_distributor_walks() {
    let runtime = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(runtime.path().join("workspaces").join("agentdesk")).unwrap();
    let walked = crate::runtime_layout::distributed_workspace_names(runtime.path()).unwrap();
    assert_eq!(walked, vec!["agentdesk".to_string()]);
    let walked: Vec<&str> = walked.iter().map(String::as_str).collect();

    // The vault token is `project-agentdesk` while the directory is
    // `agentdesk`. Round three graded exactly this shape and reported eleven
    // false positives, so an unmatched id is a skip and never a finding.
    let manifest = manifest_at(
        runtime.path(),
        r#"{"skills":{"s":{"workspaces":["project-agentdesk"]}}}"#,
    );
    let report = audit(
        &["project-agentdesk"],
        Some(&["project-agentdesk"][..]),
        Some(walked.as_slice()),
        vec![manifest],
    );
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(skips(&report), vec![NESTED_ID_MISMATCH]);
}

#[test]
fn the_four_manifest_read_states_stay_distinguishable() {
    let temp = tempfile::tempdir().unwrap();
    let roster = Some(&["agentdesk"][..]);
    let walked = Some(&["agentdesk"][..]);

    let missing = audit(&["agentdesk"], roster, walked, vec![]);
    assert_eq!(skips(&missing), vec![NO_MANIFEST]);

    // A path that resolves to a directory reads as unreadable, not missing.
    let unreadable = audit(
        &["agentdesk"],
        roster,
        walked,
        vec![temp.path().to_path_buf()],
    );
    assert_eq!(skips(&unreadable), vec![UNREADABLE_MANIFEST]);

    let broken = manifest_at(&temp.path().join("broken"), "{ not json");
    let unparsable = audit(&["agentdesk"], roster, walked, vec![broken]);
    assert_eq!(skips(&unparsable), vec![UNPARSABLE_MANIFEST]);

    let empty = manifest_at(&temp.path().join("empty"), "{}");
    let no_roster = audit(&[], Some(&[] as &[&str]), walked, vec![empty]);
    assert_eq!(skips(&no_roster), vec![EMPTY_ROSTER]);
}

#[test]
fn skill_manifest_paths_takes_directory_roots_only() {
    let runtime = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let managed = manifest_at(
        &crate::runtime_layout::managed_skills_root(runtime.path()),
        "{}",
    );
    // Same file name under the markdown-file root must never be picked up.
    manifest_at(&home.path().join(".claude").join("commands"), "{}");
    // Every directory root is read, not just the first one.
    let codex = manifest_at(&home.path().join(".codex").join("skills"), "{}");

    let selected = skill_manifest_paths(
        Some(runtime.path().to_path_buf()),
        Some(home.path().to_path_buf()),
    );
    assert_eq!(selected, vec![managed, codex]);
}

#[test]
#[cfg(unix)]
fn a_manifest_that_exists_but_cannot_be_read_still_reaches_its_skip() {
    use std::os::unix::fs::PermissionsExt;

    let runtime = tempfile::tempdir().unwrap();
    let root = crate::runtime_layout::managed_skills_root(runtime.path());
    let manifest = manifest_at(&root, r#"{"skills":{}}"#);
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
    let staged = std::fs::read_to_string(&manifest).is_err();
    let selected = skill_manifest_paths(Some(runtime.path().to_path_buf()), None);
    let roster = Some(&["agentdesk"][..]);
    let report = audit(&["agentdesk"], roster, roster, selected.clone());
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A euid the mode bits do not bind cannot stage an unreadable file at all.
    if !staged {
        panic!("mode 000 left the manifest readable; run this off root");
    }

    // `is_file()` answered `false` to the `PermissionDenied` stat and dropped
    // the path, so an existing manifest read as `no_manifest` - the one state
    // that means the file is not there. It reaches its own skip now.
    assert_eq!(selected, vec![manifest]);
    assert_eq!(skips(&report), vec![UNREADABLE_MANIFEST]);
    assert_eq!(report.to_json()["audited"], serde_json::json!(false));
}

#[test]
fn a_padded_wildcard_is_not_the_wildcard_the_distributor_reserves() {
    let temp = tempfile::tempdir().unwrap();
    let roster = Some(&["agentdesk"][..]);

    // `distribute_agent_skills.py` compares the raw pattern bytes to `*` and
    // otherwise fnmatches, so ` * ` reaches no agent there. Trimming before
    // that comparison folded it into the one token that needs no roster.
    let padded = manifest_at(&temp.path().join("padded"), r#"{"s":{"agents":[" * "]}}"#);
    let report = audit(&["agentdesk"], roster, roster, vec![padded]);
    assert!(skips(&report).is_empty());
    assert_eq!(report.findings[0]["agent_id"], " * ");

    // The exact `*` keeps its reserved meaning: no roster, no skip.
    let bare = manifest_at(&temp.path().join("bare"), r#"{"s":{"agents":["*"]}}"#);
    let report = audit(&["agentdesk"], roster, roster, vec![bare]);
    assert!(report.skipped.is_empty(), "{:?}", skips(&report));
}

#[test]
fn both_keys_are_answered_in_both_manifest_locations() {
    let temp = tempfile::tempdir().unwrap();
    let roster = Some(&["agentdesk"][..]);

    let named_flat = manifest_at(
        &temp.path().join("named-flat"),
        r#"{"version":1,"skills":{"s":{"agents":["ghost-agent"]}}}"#,
    );
    let report = audit(&["agentdesk"], roster, roster, vec![named_flat]);
    assert!(skips(&report).is_empty());
    assert_eq!(report.findings[0]["agent_id"], "ghost-agent");
    assert_eq!(report.to_json()["audited"], serde_json::json!(true));

    // `legacy[*].workspaces` is the nested key inside the flattened map and is
    // graded on the same axis as `skills[*].workspaces`.
    let legacy_nested = manifest_at(
        &temp.path().join("legacy-nested"),
        r#"{"legacy-skill":{"targets":["claude"],"workspaces":["ghost-workspace"]}}"#,
    );
    let report = audit(&["agentdesk"], roster, roster, vec![legacy_nested]);
    assert_eq!(skips(&report), vec![NESTED_ID_MISMATCH]);
}

#[test]
fn manifest_paths_read_the_runtime_root_and_the_process_home() {
    // The home half carries three of the four directory roots, so resolving it
    // away leaves the runtime root alone and every manifest under the other
    // roots reads as absent rather than as unread.
    let (root, home) = manifest_path_sources();
    assert_eq!(root, crate::config::runtime_root());
    let home = home.expect("$HOME resolves on every host this audit runs on");
    assert_eq!(Some(&home), dirs::home_dir().as_ref());
    let home_roots = skill_roots(None, Some(home.clone()));
    assert_eq!(home_roots.len(), 4, "{home_roots:?}");
    assert_eq!(
        default_manifest_paths(),
        skill_manifest_paths(root, Some(home))
    );
}

#[test]
fn agent_globs_and_uninstalled_skills_are_clean_but_real_unlisted_skills_are_findings() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = manifest_at(
        temp.path(),
        r#"{"global_core_skills":["core"],"skills":{"future":{"agents":["ch-*","*"]}},"legacy":{"agents":["cfg-only"]}}"#,
    );
    for name in ["_shared", ".system", "core"] {
        let dir = temp.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "skill").unwrap();
    }
    std::fs::create_dir_all(temp.path().join("not-a-skill/SKILL.md")).unwrap();
    let report = audit(
        &["cfg-only"],
        Some(&["ch-db-only"]),
        Some(&[]),
        vec![manifest.clone()],
    );
    assert_eq!(report.to_json()["findings"], json!([]));
    assert_eq!(report.warning(), None);
    let dir = temp.path().join("real-skill");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), "skill").unwrap();
    let report = audit(
        &["cfg-only"],
        Some(&["ch-db-only"]),
        Some(&[]),
        vec![manifest],
    );
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0]["kind"], "unlisted_skill");
    assert_eq!(report.findings[0]["path"], dir.display().to_string());
}

#[test]
fn missing_required_or_installed_source_manifest_is_unavailable_and_never_passes() {
    let runtime = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let vault = home.path().join("ObsidianVault/RemoteVault/99_Skills");
    std::fs::create_dir_all(&vault).unwrap();
    let selected = skill_manifest_paths(
        Some(runtime.path().to_path_buf()),
        Some(home.path().to_path_buf()),
    );
    assert_eq!(
        selected,
        vec![
            crate::runtime_layout::managed_skills_manifest_path(runtime.path()),
            vault.join("manifest.json")
        ]
    );
    let report = audit(&["live"], Some(&["live"]), Some(&[]), selected);
    assert_eq!(skips(&report), vec![NO_MANIFEST]);
    assert_eq!(report.to_json()["audited"], false);
    assert_eq!(
        report.to_json()["inspection_errors"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(report.warning().is_some());
}
