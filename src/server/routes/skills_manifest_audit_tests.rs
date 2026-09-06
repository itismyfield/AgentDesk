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
    assert_eq!(pinned_agent_ids(&raw), ids(&["agentdesk", "ch-*", "claude"]));
}

#[test]
fn union_covers_db_only_ids_and_a_failed_source_stops_grading() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = manifest_at(temp.path(), r#"{"skills":{"s":{"workspaces":["db-only"]}}}"#);
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

    // Dropping either half of the union has to be visible, or the wiring
    // could quietly narrow the roster and manufacture findings.
    let db_dropped = audit(
        &["cfg-only"],
        Some(&[] as &[&str]),
        walked,
        vec![manifest.clone()],
    );
    assert_eq!(db_dropped.findings.len(), 1, "{:?}", db_dropped.findings);

    let db_failed = audit(&["cfg-only"], None, walked, vec![manifest]);
    assert!(db_failed.findings.is_empty(), "{:?}", db_failed.findings);
    assert_eq!(skips(&db_failed), vec![ROSTER_SOURCE_UNAVAILABLE]);
}

#[test]
fn an_empty_roster_skips_instead_of_reporting_a_clean_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = manifest_at(temp.path(), r#"{"skills":{"s":{"workspaces":["agentdesk"]}}}"#);

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
fn a_flat_skip_does_not_swallow_the_nested_entries_beside_it() {
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
    // `core-skill` pins only the wildcard, so it needs no roster and adds no
    // skip; `flat-skill` names an agent whose roster lives in Python.
    assert_eq!(skips(&report), vec![FLAT_ROSTER_UNAVAILABLE]);
    // The nested entry beside it is still graded: `agentdesk` is a directory
    // the distributor walks, and no agent answers to that id.
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    assert_eq!(report.findings[0]["agent_id"], serde_json::json!("agentdesk"));
    assert_eq!(report.findings[0]["skill"], serde_json::json!("nested-skill"));
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
    let unreadable = audit(&["agentdesk"], roster, walked, vec![temp.path().to_path_buf()]);
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

    let selected = skill_manifest_paths(
        Some(runtime.path().to_path_buf()),
        Some(home.path().to_path_buf()),
    );
    assert_eq!(selected, vec![managed]);
}
