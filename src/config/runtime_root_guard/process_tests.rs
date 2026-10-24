use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

fn production_probe() -> &'static Path {
    static PROBE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &PROBE.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("probe.rs");
        let binary = dir.path().join(format!("probe{}", std::env::consts::EXE_SUFFIX));
        let module = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config/runtime_root_guard.rs");
        std::fs::write(&source, format!(
            "#[path = {module:?}] mod runtime_root_guard;\nfn main() {{ let root = runtime_root_guard::resolve(|| std::env::var_os(\"HOME\").map(std::path::PathBuf::from)); println!(\"{{}}\", root.unwrap().display()); }}\n"
        )).unwrap();
        let output = Command::new("rustc").args(["--edition=2024", "--crate-name", "runtime_root_probe"])
            .arg(&source).arg("-o").arg(&binary).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        (dir, binary)
    }).1
}

fn run_probe(home: &Path, root: Option<&str>, test_process: bool) -> Output {
    let mut command = Command::new(production_probe());
    command.env("HOME", home).env_remove("AGENTDESK_ROOT_DIR");
    if let Some(root) = root {
        command.env("AGENTDESK_ROOT_DIR", root);
    }
    if !test_process {
        command.env_remove("AGENTDESK_TEST_RUNTIME");
    }
    command.output().unwrap()
}

#[test]
fn ordinary_build_keeps_release_fallback_and_trimmed_override() {
    let home = tempfile::tempdir().unwrap();
    for root in [None, Some(""), Some(" \t ")] {
        let output = run_probe(home.path(), root, false);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            home.path().join(".adk").join("release").to_string_lossy()
        );
    }
    let explicit = home.path().join("explicit");
    let value = format!("  {}  ", explicit.display());
    let output = run_probe(home.path(), Some(&value), false);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        explicit.to_string_lossy()
    );
    assert!(!home.path().join(".adk").exists());
}

#[test]
fn integration_process_and_ordinary_child_require_an_explicit_root() {
    let home = tempfile::tempdir().unwrap();
    assert!(std::env::var_os("AGENTDESK_TEST_RUNTIME").is_some());
    for root in [None, Some(""), Some(" \t ")] {
        let output = run_probe(home.path(), root, true);
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("test runtime requires a non-empty AGENTDESK_ROOT_DIR")
        );
    }
    let explicit = home.path().join("child-runtime");
    let output = run_probe(home.path(), Some(explicit.to_str().unwrap()), true);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        explicit.to_string_lossy()
    );
    assert!(!home.path().join(".adk").exists());
}

#[test]
fn integration_child_using_the_normal_agentdesk_library_fails_before_config_audit() {
    let home = tempfile::tempdir().unwrap();
    let output = if let Some(binary) = option_env!("CARGO_BIN_EXE_agentdesk") {
        println!("normal child: agentdesk config audit --dry-run");
        Command::new(binary)
            .args(["config", "audit", "--dry-run"])
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env_remove("AGENTDESK_ROOT_DIR")
            .output()
            .unwrap()
    } else {
        println!("normal child: standalone production resolver");
        run_probe(home.path(), None, true)
    };
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("test runtime requires a non-empty AGENTDESK_ROOT_DIR"),
        "{output:?}"
    );
    assert!(!home.path().join(".adk").exists());
}
