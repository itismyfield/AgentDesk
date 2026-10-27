use super::*;
use crate::test_support::production_scan::{Owner, scan, test_only_paths, unowned_matches};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

fn write_config(path: &Path, port: u16) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut config = Config::default();
    config.server.port = port;
    crate::config::save_to_path(path, &config).unwrap();
}

fn write_broken(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "agents: [\n").unwrap();
}

#[test]
fn explicit_path_wins_over_every_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    write_config(&config_file_path(&root), 1);
    let explicit = dir.path().join("elsewhere.yaml");
    write_config(&explicit, 2);

    let resolved = resolve_with(Some(explicit.clone()), Some(root), None, None);

    assert_eq!(resolved, explicit);
}

#[test]
fn missing_explicit_path_takes_its_other_layout_or_stays_itself() {
    let dir = tempfile::tempdir().unwrap();
    let explicit = config_file_path(dir.path());
    assert_eq!(search(Some(explicit.clone()), &[]), Some(explicit.clone()));

    write_config(&legacy_config_file_path(dir.path()), 1);

    assert_eq!(
        search(Some(explicit), &[]),
        Some(legacy_config_file_path(dir.path()))
    );
}

#[test]
fn without_env_canonical_file_beats_legacy_file() {
    let root = tempfile::tempdir().unwrap();
    write_config(&config_file_path(root.path()), 1);
    write_config(&legacy_config_file_path(root.path()), 2);

    let resolved = resolve_with(None, Some(root.path().to_path_buf()), None, None);

    assert_eq!(resolved, config_file_path(root.path()));
}

#[test]
fn legacy_file_is_read_when_canonical_is_absent() {
    let root = tempfile::tempdir().unwrap();
    write_config(&legacy_config_file_path(root.path()), 2);

    let resolved = resolve_with(None, Some(root.path().to_path_buf()), None, None);

    assert_eq!(resolved, legacy_config_file_path(root.path()));
    assert_eq!(
        config_path_in(root.path()),
        legacy_config_file_path(root.path())
    );
}

#[test]
fn roots_are_searched_runtime_root_then_cwd_then_release() {
    let dir = tempfile::tempdir().unwrap();
    let (root, cwd, home) = (
        dir.path().join("r"),
        dir.path().join("c"),
        dir.path().join("h"),
    );
    let release = home.join(".adk").join("release");
    let resolve = || {
        resolve_with(
            None,
            Some(root.clone()),
            Some(cwd.clone()),
            Some(home.clone()),
        )
    };

    assert_eq!(resolve(), config_file_path(&root));
    write_config(&legacy_config_file_path(&release), 3);
    assert_eq!(resolve(), legacy_config_file_path(&release));
    write_config(&config_file_path(&cwd), 2);
    assert_eq!(resolve(), config_file_path(&cwd));
    write_config(&legacy_config_file_path(&root), 1);
    assert_eq!(resolve(), legacy_config_file_path(&root));
    assert_eq!(
        resolve_with(None, None, None, None),
        PathBuf::from("config").join("agentdesk.yaml")
    );
}

#[test]
fn broken_canonical_file_is_an_error_not_a_legacy_fallback() {
    let root = tempfile::tempdir().unwrap();
    write_broken(&config_file_path(root.path()));
    write_config(&legacy_config_file_path(root.path()), 2);

    let path = config_path_in(root.path());

    assert_eq!(path, config_file_path(root.path()));
    assert!(load_config_file(&path).is_err());
    assert!(
        load_config_file(&root.path().join("absent.yaml"))
            .unwrap()
            .is_none()
    );

    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _root = set_env("AGENTDESK_ROOT_DIR", Some(root.path().as_os_str()));
    let _explicit = set_env(EXPLICIT_ENV, None);
    assert!(load_runtime_config_file().is_none());
}

#[test]
fn env_path_is_used_for_the_runtime_root_only() {
    let dir = tempfile::tempdir().unwrap();
    let (root, other) = (dir.path().join("root"), dir.path().join("other"));
    write_config(&config_file_path(&root), 1);
    write_config(&config_file_path(&other), 3);
    let explicit = dir.path().join("explicit.yaml");
    write_config(&explicit, 2);

    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _root = set_env("AGENTDESK_ROOT_DIR", Some(root.as_os_str()));
    let _explicit = set_env(EXPLICIT_ENV, Some(explicit.as_os_str()));

    let (config, path) = load_runtime_config_file().unwrap();
    assert_eq!((config.server.port, path), (2, explicit.clone()));
    assert_eq!(resolved_config_path(), explicit);
    assert_eq!(config_path_in(&other), config_file_path(&other));
}

/// Sets or removes one variable while the caller holds the shared env lock.
fn set_env(key: &'static str, value: Option<&std::ffi::OsStr>) -> crate::config::TestEnvVarGuard {
    let guard = crate::config::TestEnvVarGuard::capture_after_shared_test_env_lock(key);
    match value {
        Some(value) => unsafe { std::env::set_var(key, value) },
        None => unsafe { std::env::remove_var(key) },
    }
    guard
}

/// Production functions that may still name a config file path themselves, and why.
/// An empty signature covers the whole file.
const PATH_OWNERS: &[Owner] = &[
    ("src/config/config_path.rs", "", "the resolver"),
    (
        "src/runtime_layout/paths.rs",
        "pub fn config_file_path(",
        "defines the canonical path",
    ),
    (
        "src/runtime_layout/paths.rs",
        "pub fn legacy_config_file_path(",
        "defines the legacy path",
    ),
    (
        "src/runtime_layout/legacy_migration.rs",
        "fn create_legacy_backup(",
        "backs up the legacy file before migration",
    ),
    (
        "src/runtime_layout/legacy_migration.rs",
        "fn normalize_agent_config_channels(",
        "rewrites the migrated file in place",
    ),
    (
        "src/runtime_layout/legacy_migration.rs",
        "fn migrate_legacy_config_file(",
        "moves the legacy file into config/",
    ),
    (
        "src/runtime_layout/mod.rs",
        "fn legacy_layout_needs_migration(",
        "detects a legacy layout to migrate",
    ),
    (
        "src/runtime_layout/config_merge.rs",
        "fn merge_role_map_into_agentdesk_yaml(",
        "merges role_map into the migrated file",
    ),
    (
        "src/cli/migrate/plan.rs",
        "fn load_existing_role_ids(",
        "reads both layouts as migration input",
    ),
    (
        "src/cli/migrate/apply.rs",
        "fn apply_import_plan(",
        "writes the migrated file",
    ),
    (
        "src/cli/dcserver.rs",
        "pub fn handle_dcserver(",
        "body pinned by check_legacy_supervision_census",
    ),
];

const BYPASS: &str = r#"var(?:_os)?\(\s*"AGENTDESK_CONFIG"|config_file_path\(|\.join\(\s*"agentdesk\.yaml"\s*\)|"config/agentdesk\.yaml""#;

static BYPASS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(BYPASS).unwrap());

#[test]
fn config_file_paths_are_built_only_by_the_resolver() {
    let (bypasses, stale) = unowned_matches(&BYPASS_RE, PATH_OWNERS);
    assert!(
        bypasses.is_empty(),
        "build config file paths with crate::config::config_path instead: {bypasses:?}"
    );
    assert!(stale.is_empty(), "drop these from PATH_OWNERS: {stale:?}");
}

#[test]
fn bypass_scan_skips_test_items_and_follows_test_modules() {
    let lines = |text: &str| scan("src/x.rs", text, &BYPASS_RE, &[]).unwrap().lines;
    let text = "fn a() {\n    std::env::var_os(\"AGENTDESK_CONFIG\");\n}\n\
        #[cfg(test)]\nmod t {\n    fn b() { config_file_path(x); }\n}\n\
        #[cfg(test)]\nfn helper(\n    root: &Path,\n) -> PathBuf {\n    root.join(\"agentdesk.yaml\")\n}\n\
        #[cfg(all(unix, test))] use x::config_file_path;\n\
        #[cfg(test)] // note\nconst P: &str = \"config/agentdesk.yaml\";\n\
        // config_file_path(x) in a comment\n\
        fn c() -> PathBuf { PathBuf::from(\"config/agentdesk.yaml\") }\n";
    assert_eq!(lines(text), vec![2, 18]);
    // A brace inside a test string must not hide the production code after it.
    let brace = "#[cfg(test)]\nfn t() { let _ = Some(\"{\"); }\nfn prod(root: &Path) { config_file_path(root); }\n";
    assert_eq!(lines(brace), vec![3]);
    assert!(lines("#![cfg(test)]\nfn t() { config_file_path(x); }\n").is_empty());

    // An owner covers its own function only, and a missing owner fails the scan.
    let owners = [("src/x.rs", "fn migrate(", "migration input")];
    let owned = "fn migrate(root: &Path) { legacy_config_file_path(root); }\n\
        fn load(root: &Path) { legacy_config_file_path(root); }\n";
    let found = scan("src/x.rs", owned, &BYPASS_RE, &owners).unwrap();
    assert_eq!(
        (found.lines, found.owners_used),
        (vec![2], vec!["fn migrate("])
    );
    assert!(scan("src/x.rs", "fn other() {}\n", &BYPASS_RE, &owners).is_err());
    assert!(
        scan(
            "src/x.rs",
            "fn a() { let _ = \"unterminated; }\n",
            &BYPASS_RE,
            &[]
        )
        .is_err()
    );

    let files: Vec<(PathBuf, String)> = [
        (
            "src/a/b.rs",
            "#[cfg(test)]\n#[path = \"b_tests.rs\"]\nmod tests;\nmod prod;\n",
        ),
        (
            "src/a/b_tests.rs",
            "#[path = \"deep_tests.rs\"]\nmod deep;\nmod nested;\n",
        ),
        (
            "src/a/c.rs",
            "#[cfg(all(unix, test))]\npub(crate) mod c_tests;\n",
        ),
        (
            "src/a/d/e.rs",
            "#[cfg(test)]\n#[path = \"../../f_tests.rs\"]\nmod f;\n",
        ),
    ]
    .into_iter()
    .map(|(path, text)| (PathBuf::from(path), text.to_string()))
    .collect();
    let test_only = test_only_paths(&files);
    let covered = |path: &str| {
        test_only
            .iter()
            .any(|prefix| Path::new(path).starts_with(prefix))
    };
    for path in [
        "src/a/b_tests.rs",
        "src/a/deep_tests.rs",
        "src/a/nested.rs",
        "src/a/c/c_tests.rs",
        "src/a/c/c_tests/inner.rs",
        "src/f_tests.rs",
    ] {
        assert!(covered(path), "{path} is test-only");
    }
    assert!(!covered("src/a/b/prod.rs"));
    assert!(!covered("src/a/b.rs"));
}
