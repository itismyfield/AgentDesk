use super::*;
use regex::Regex;
use std::collections::BTreeSet;
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
    (
        "src/cli/dcserver.rs",
        "handle_dcserver's body is pinned by check_legacy_supervision_census",
    ),
    (
        "src/services/discord/turn_presence/activity/unbound/live.rs",
        "not converted yet",
    ),
];

const BYPASS: &str = r#"var(?:_os)?\(\s*"AGENTDESK_CONFIG"|config_file_path\(|\.join\(\s*"agentdesk\.yaml"\s*\)|"config/agentdesk\.yaml""#;

static BYPASS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(BYPASS).unwrap());
static TEST_CFG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^#\[cfg\((?:test|all\([^\]]*\btest\b[^\]]*)\)\]").unwrap());
static MOD_DECL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;").unwrap());
static PATH_ATTR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*#\[path\s*=\s*"([^"]+)"\]"#).unwrap());

/// Line numbers of production code (outside `#[cfg(test)]` items) that build a config path.
fn bypass_lines(text: &str) -> Vec<usize> {
    production_lines(text)
        .into_iter()
        .filter(|(_, line)| BYPASS_RE.is_match(line))
        .map(|(number, _)| number)
        .collect()
}

/// The rest of `code` after a test-only `#[cfg(..)]` attribute, or `None` for other lines.
fn after_test_cfg(code: &str) -> Option<&str> {
    let end = TEST_CFG_RE.find(code)?.end();
    (!code[..end].contains("not(")).then(|| code[end..].split("//").next().unwrap().trim())
}

fn production_lines(text: &str) -> Vec<(usize, &str)> {
    let net = |line: &str, open, close| {
        line.matches(open).count() as i64 - line.matches(close).count() as i64
    };
    let (mut lines, mut gated, mut depth, mut parens) = (Vec::new(), false, 0_i64, 0_i64);
    for (index, line) in text.lines().enumerate() {
        let mut code = line.trim();
        if depth > 0 {
            depth += net(line, '{', '}');
            continue;
        }
        if let Some(rest) = after_test_cfg(code) {
            (gated, code, parens) = (true, rest, 0);
        } else if !gated {
            lines.push((index + 1, line));
            continue;
        }
        // A gated item ends at its body, or at `;`/`,` outside a parameter list.
        parens += net(code, '(', ')');
        if code.contains('{') && parens <= 0 {
            (gated, depth) = (false, net(code, '{', '}'));
        } else if parens <= 0 && (code.ends_with(';') || code.ends_with(',')) {
            gated = false;
        }
    }
    lines
}

/// One `mod name;` declaration in `file`.
struct ModDecl {
    file: PathBuf,
    test_only: bool,
    path_attr: Option<String>,
    name: String,
}

fn mod_decls(file: &Path, text: &str) -> Vec<ModDecl> {
    let (mut decls, mut gated, mut attr) = (Vec::new(), false, None);
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
        } else if let Some(found) = PATH_ATTR_RE.captures(line) {
            attr = Some(found[1].to_string());
        } else if let Some(found) = MOD_DECL_RE.captures(line) {
            decls.push(ModDecl {
                file: file.to_path_buf(),
                test_only: gated,
                path_attr: attr.take(),
                name: found[1].to_string(),
            });
            gated = false;
        } else if !line.trim().starts_with("#[") {
            (gated, attr) = (false, None);
        }
    }
    decls
}

/// `a/b/../c` as `a/c`, the way `#[path = "../c"]` resolves.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Files and directories that only test-only modules (and modules they declare) live in.
fn test_only_paths(files: &[(PathBuf, String)]) -> Vec<PathBuf> {
    let decls: Vec<ModDecl> = files
        .iter()
        .flat_map(|(path, text)| mod_decls(path, text))
        .collect();
    // A `#[path]`-loaded file owns its directory like `mod.rs` does.
    let path_loaded: BTreeSet<PathBuf> = decls
        .iter()
        .filter_map(|decl| Some(lexical(&decl.file.parent()?.join(decl.path_attr.as_ref()?))))
        .collect();
    let target = |decl: &ModDecl| {
        let dir = decl.file.parent().unwrap();
        let file = match &decl.path_attr {
            Some(path) => lexical(&dir.join(path)),
            None => {
                let owns_dir = path_loaded.contains(&decl.file)
                    || matches!(
                        decl.file.file_name().and_then(|name| name.to_str()),
                        Some("mod.rs" | "lib.rs" | "main.rs")
                    );
                let base = match owns_dir {
                    true => dir.to_path_buf(),
                    false => dir.join(decl.file.file_stem().unwrap()),
                };
                base.join(format!("{}.rs", decl.name))
            }
        };
        let children = match file.file_name().and_then(|name| name.to_str()) {
            Some("mod.rs") => file.parent().unwrap().to_path_buf(),
            _ => file.with_extension(""),
        };
        [file, children]
    };
    let mut test_only: Vec<PathBuf> = Vec::new();
    loop {
        let before = test_only.len();
        for decl in &decls {
            let inside = test_only.iter().any(|prefix| decl.file.starts_with(prefix));
            if decl.test_only || inside {
                for path in target(decl) {
                    if !test_only.contains(&path) {
                        test_only.push(path);
                    }
                }
            }
        }
        if test_only.len() == before {
            return test_only;
        }
    }
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
    let test_only = test_only_paths(&files);
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
