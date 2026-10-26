use super::*;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

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
    let _root = TestEnv::set("AGENTDESK_ROOT_DIR", Some(root.path().as_os_str()));
    let _explicit = TestEnv::set(EXPLICIT_ENV, None);
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
    let _root = TestEnv::set("AGENTDESK_ROOT_DIR", Some(root.as_os_str()));
    let _explicit = TestEnv::set(EXPLICIT_ENV, Some(explicit.as_os_str()));

    let (config, path) = load_runtime_config_file().unwrap();
    assert_eq!((config.server.port, path), (2, explicit.clone()));
    assert_eq!(resolved_config_path(), explicit);
    assert_eq!(config_path_in(&other), config_file_path(&other));
}

/// Sets or removes one variable while the caller holds the shared env lock.
struct TestEnv(crate::config::TestEnvVarGuard);

impl TestEnv {
    fn set(key: &'static str, value: Option<&std::ffi::OsStr>) -> Self {
        let guard = crate::config::TestEnvVarGuard::capture_after_shared_test_env_lock(key);
        match value {
            Some(value) => unsafe { std::env::set_var(key, value) },
            None => unsafe { std::env::remove_var(key) },
        }
        Self(guard)
    }
}

/// Production files that may still name a config file path themselves, and why.
const PATH_OWNERS: &[(&str, &str)] = &[
    ("src/config/config_path.rs", "the resolver"),
    ("src/runtime_layout/paths.rs", "defines both layout paths"),
    (
        "src/runtime_layout/legacy_migration.rs",
        "moves the legacy file into config/",
    ),
    (
        "src/runtime_layout/mod.rs",
        "detects a legacy layout to migrate",
    ),
    (
        "src/runtime_layout/config_merge.rs",
        "merges role_map into the migrated file",
    ),
    (
        "src/cli/migrate/plan.rs",
        "reads both layouts as migration input",
    ),
    ("src/cli/migrate/apply.rs", "writes the migrated file"),
    ("src/cli/dcserver.rs", "not converted yet"),
    (
        "src/services/discord/turn_presence/activity/unbound/live.rs",
        "not converted yet",
    ),
];

const BYPASS: &str = r#"var(?:_os)?\(\s*"AGENTDESK_CONFIG"|config_file_path\(|\.join\(\s*"agentdesk\.yaml"\s*\)|"config/agentdesk\.yaml""#;

/// Line numbers of production code (outside `#[cfg(test)]` items) that build a config path.
fn bypass_lines(text: &str) -> Vec<usize> {
    let bypass = regex::Regex::new(BYPASS).unwrap();
    production_lines(text)
        .into_iter()
        .filter(|(_, line)| bypass.is_match(line))
        .map(|(number, _)| number)
        .collect()
}

/// The rest of `code` after a test-only `#[cfg(..)]` attribute, or `None` for other lines.
fn after_test_cfg(code: &str) -> Option<&str> {
    let attr = regex::Regex::new(r"^#\[cfg\((?:test|all\([^\]]*\btest\b[^\]]*)\)\]").unwrap();
    let end = attr.find(code)?.end();
    (!code[..end].contains("not(")).then(|| code[end..].split("//").next().unwrap().trim())
}

fn production_lines(text: &str) -> Vec<(usize, &str)> {
    let braces = |line: &str| line.matches('{').count() as i64 - line.matches('}').count() as i64;
    let (mut lines, mut gated, mut depth) = (Vec::new(), false, 0_i64);
    for (index, line) in text.lines().enumerate() {
        let mut code = line.trim();
        if depth > 0 {
            depth += braces(line);
            continue;
        }
        if let Some(rest) = after_test_cfg(code) {
            (gated, code) = (true, rest);
        } else if !gated {
            lines.push((index + 1, line));
            continue;
        }
        if code.contains('{') {
            (gated, depth) = (false, braces(code));
        } else if code.ends_with(';') || code.ends_with(',') {
            gated = false;
        }
    }
    lines
}

/// Files and directories of modules declared under `#[cfg(test)]` in `file`.
fn test_module_paths(file: &Path, text: &str) -> Vec<PathBuf> {
    let declaration = regex::Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;").unwrap();
    let path_attr = regex::Regex::new(r#"^\s*#\[path\s*=\s*"([^"]+)"\]"#).unwrap();
    let dir = file.parent().unwrap();
    let own_dir = match file.file_name().and_then(|name| name.to_str()) {
        Some("mod.rs" | "lib.rs" | "main.rs") => dir.to_path_buf(),
        _ => dir.join(file.file_stem().unwrap()),
    };
    let (mut paths, mut gated, mut attr) = (Vec::new(), false, None);
    for line in text.lines() {
        let line = match after_test_cfg(line.trim()) {
            Some(rest) => {
                gated = true;
                rest
            }
            None => line,
        };
        if line.trim().is_empty() || line.trim().starts_with("//") {
            continue;
        } else if let Some(found) = path_attr.captures(line) {
            attr = Some(found[1].to_string());
        } else if let Some(found) = declaration.captures(line) {
            if gated {
                match attr.take() {
                    Some(path) => paths.push(dir.join(path)),
                    None => {
                        paths.push(own_dir.join(format!("{}.rs", &found[1])));
                        paths.push(own_dir.join(&found[1]));
                    }
                }
            }
            (gated, attr) = (false, None);
        } else if !line.trim().starts_with("#[") {
            (gated, attr) = (false, None);
        }
    }
    let children: Vec<PathBuf> = paths
        .iter()
        .filter(|path| path.extension().is_some())
        .map(
            |path| match path.file_name().and_then(|name| name.to_str()) {
                Some("mod.rs") => path.parent().unwrap().to_path_buf(),
                _ => path.with_extension(""),
            },
        )
        .collect();
    paths.extend(children);
    paths
}

fn source_files(root: &Path) -> Vec<(PathBuf, String)> {
    let mut pending = vec![root.join("src")];
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(
                std::fs::read_dir(&path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            files.push((path.strip_prefix(root).unwrap().to_path_buf(), text));
        }
    }
    files
}

#[test]
fn config_file_paths_are_built_only_by_the_resolver() {
    let files = source_files(Path::new(env!("CARGO_MANIFEST_DIR")));
    let test_only: Vec<PathBuf> = files
        .iter()
        .flat_map(|(path, text)| test_module_paths(path, text))
        .collect();
    let owners: BTreeSet<&str> = PATH_OWNERS.iter().map(|(path, _)| *path).collect();
    let mut bypasses = Vec::new();
    let mut owners_seen = BTreeSet::new();
    for (path, text) in &files {
        if test_only.iter().any(|prefix| path.starts_with(prefix)) {
            continue;
        }
        let lines = bypass_lines(text);
        let name = path.to_str().unwrap();
        if owners.contains(name) {
            if !lines.is_empty() {
                owners_seen.insert(name);
            }
        } else {
            bypasses.extend(lines.iter().map(|line| format!("{name}:{line}")));
        }
    }
    assert!(
        bypasses.is_empty(),
        "build config file paths with crate::config::config_path instead: {bypasses:?}"
    );
    let stale: Vec<_> = owners.difference(&owners_seen).collect();
    assert!(stale.is_empty(), "drop these from PATH_OWNERS: {stale:?}");
}

#[test]
fn bypass_scan_skips_test_items_and_follows_test_modules() {
    let text = "fn a() {\n    std::env::var_os(\"AGENTDESK_CONFIG\");\n}\n\
        #[cfg(test)]\nmod t {\n    fn b() { config_file_path(x); }\n}\n\
        #[cfg(test)]\nfn helper(\n    root: &Path,\n) -> PathBuf {\n    root.join(\"agentdesk.yaml\")\n}\n\
        #[cfg(all(unix, test))] use x::config_file_path;\n\
        #[cfg(test)] // note\nconst P: &str = \"config/agentdesk.yaml\";\n\
        fn c() -> PathBuf { PathBuf::from(\"config/agentdesk.yaml\") }\n";
    assert_eq!(bypass_lines(text), vec![2, 17]);

    let declarations = "#[cfg(test)]\n#[path = \"b/x_tests.rs\"]\nmod x;\n\
        #[cfg(all(test, unix))]\npub(crate) mod y;\nmod z;\n";
    let paths = test_module_paths(Path::new("src/a/b.rs"), declarations);
    let expected: Vec<PathBuf> = [
        "src/a/b/x_tests.rs",
        "src/a/b/y.rs",
        "src/a/b/y",
        "src/a/b/x_tests",
        "src/a/b/y",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    assert_eq!(paths, expected);
}
