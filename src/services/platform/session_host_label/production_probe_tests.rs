use std::path::Path;
use std::process::Command;

fn run_probe(binary: &Path, root: &Path, host: &str, expected: &str, change_inputs: bool) {
    let mut command = Command::new(binary);
    command
        .env("AGENTDESK_ROOT_DIR", root)
        .env("ADK_LABEL_PROBE_HOST", host)
        .env("ADK_LABEL_PROBE_EXPECTED", expected)
        .env_remove("AGENTDESK_CONFIG");
    if change_inputs {
        command.arg("--change-inputs");
    }
    let output = command
        .output()
        .expect("execute ordinary production getter probe");
    assert!(
        output.status.success(),
        "{host} -> {expected}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "SESSION_HOST_LABEL_PROBE_OK"
    );
}

#[test]
fn ordinary_getter_restores_creates_and_caches_host_spelling() {
    let fixture = tempfile::tempdir().unwrap();
    let source = Path::new(crate::runtime_layout::REPO_ROOT)
        .join("tests/fixtures/session_host_label_probe.rs");
    let binary = fixture
        .path()
        .join(format!("probe{}", std::env::consts::EXE_SUFFIX));
    let output = Command::new("rustc")
        .args(["--edition=2024", "--crate-name", "session_host_label_probe"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("compile ordinary production getter probe");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let saved = fixture.path().join("saved");
    std::fs::create_dir_all(saved.join("runtime")).unwrap();
    std::fs::write(saved.join("runtime/session_host_label"), "MacBook-Pro-14\n").unwrap();
    run_probe(&binary, &saved, "macbook-pro-14", "MacBook-Pro-14", true);

    let fresh = fixture.path().join("fresh");
    assert!(!fresh.exists());
    run_probe(&binary, &fresh, "MacBook-Pro-14", "MacBook-Pro-14", false);
    assert_eq!(
        std::fs::read_to_string(fresh.join("runtime/session_host_label")).unwrap(),
        "MacBook-Pro-14\n"
    );
    run_probe(&binary, &fresh, "macbook-pro-14", "MacBook-Pro-14", true);
}
