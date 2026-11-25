//! Shipped skills-tree test that audits through the root skills-manifest route, moved out of KRX
//! runtime_layout.

use crate::runtime_layout::{
    SkillSyncReport,
    copy_skill_dir_resolving_symlinks_for_test as copy_skill_dir_resolving_symlinks,
    managed_skills_manifest_path, managed_skills_root, sync_managed_skills,
};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn entries_under(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
            entries_under(&path, base, out);
        } else {
            out.push(path.strip_prefix(base).unwrap().to_path_buf());
        }
    }
}

/// The repo `skills/` tree, deployed as-is, audits clean and through boot and sync adds
/// no provider link: not for a vault skill boot caches, nor by reviving a retired one.
#[test]
fn shipped_skills_tree_links_nothing_and_revives_no_stale_link() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime");
    let home = temp.path().join("home");
    let _root_env = crate::config::set_agentdesk_root_for_test(&root);
    let _home_env =
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("HOME", &home);
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
    copy_skill_dir_resolving_symlinks(&shipped, &managed_skills_root(&root)).unwrap();
    let audit = crate::server::routes::skills_manifest_audit::audit_skill_manifest_agents(
        crate::server::routes::skills_manifest_audit::build_manifest_audit_request(
            ["live".to_string()].into(),
            Some(["live".to_string()].into()),
            Some(BTreeSet::new()),
            vec![(
                managed_skills_manifest_path(&root),
                crate::server::routes::skills_manifest_audit::DirectoryScope::OwnManifest,
            )],
        ),
    );
    assert_eq!(
        audit.warning(),
        None,
        "the shipped manifest must audit clean"
    );

    // A retired skill's link dangles once deploy replaced the tree; a vault skill
    // linked into the codex home is what the boot migration copies into the cache.
    let stale = home.join(".claude").join("commands").join("memory-read.md");
    let vault_link = home.join(".codex").join("skills").join("vault-skill");
    let vault_skill = temp.path().join("vault").join("vault-skill");
    for dir in [
        stale.parent().unwrap(),
        vault_link.parent().unwrap(),
        &vault_skill,
    ] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(vault_skill.join("SKILL.md"), "vault\n").unwrap();
    let stale_target = managed_skills_root(&root)
        .join("memory-read")
        .join("SKILL.md");
    std::os::unix::fs::symlink(&stale_target, &stale).unwrap();
    std::os::unix::fs::symlink(&vault_skill, &vault_link).unwrap();

    crate::runtime_layout::ensure_runtime_layout(&root).unwrap();
    let report = sync_managed_skills(&root).unwrap();

    assert_eq!(report, SkillSyncReport::default());
    let mut linked = Vec::new();
    entries_under(&home, &home, &mut linked);
    linked.sort();
    let expected = [&stale, &vault_link].map(|p| p.strip_prefix(&home).unwrap().to_path_buf());
    assert_eq!(linked, expected, "no provider link may be created");
    assert_eq!(fs::read_link(&vault_link).unwrap(), vault_skill);
    assert!(
        !stale.exists(),
        "the retired skill's link must stay unresolved"
    );
}
