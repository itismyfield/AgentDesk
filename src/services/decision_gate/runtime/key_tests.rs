use super::*;
use std::os::unix::fs::PermissionsExt;

fn stub(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("agent-op");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[tokio::test]
async fn fake_lookup_success_missing_auth_and_timeout_are_fixed_safe_classes() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("calls");
    let arguments = dir.path().join("arguments");
    for (body, expected, has_key) in [
        ("printf fixture-lookup-sentinel", KeyLookupOutcome::On, true),
        ("exit 0", KeyLookupOutcome::NoKey, false),
        (
            "echo fixture-lookup-sentinel >&2; exit 9",
            KeyLookupOutcome::AuthFailed,
            false,
        ),
        ("exec /bin/sleep 30", KeyLookupOutcome::LookupTimeout, false),
    ] {
        let body = format!(
            "printf 'x' >> '{}'\nprintf '%s\\n' \"$@\" > '{}'\n{body}",
            marker.display(),
            arguments.display()
        );
        let executable = stub(dir.path(), &body);
        let mut command = Command::new(executable);
        let start = std::time::Instant::now();
        let (key, outcome) = load(None, &mut command, &CancellationToken::new()).await;
        assert_eq!(outcome, expected);
        assert_eq!(key.is_some(), has_key);
        assert!(!format!("{key:?}/{outcome:?}").contains("fixture-lookup-sentinel"));
        assert!(start.elapsed() < Duration::from_secs(10));
        let actual_arguments = std::fs::read_to_string(&arguments).unwrap();
        assert_eq!(
            actual_arguments,
            "read\nop://AgentDesk/Gemini API/credential\n"
        );
        assert!(!actual_arguments.contains("fixture-lookup-sentinel"));
    }
    assert_eq!(std::fs::read(marker).unwrap(), b"xxxx");
}

#[tokio::test]
async fn cancellation_waits_for_owned_child_and_captured_key_never_looks_up() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("child-pid");
    let executable = stub(
        dir.path(),
        &format!("echo $$ > '{}'\nexec /bin/sleep 30", marker.display()),
    );
    let mut command = Command::new(&executable);
    let cancel = CancellationToken::new();
    let lookup = load(None, &mut command, &cancel);
    tokio::pin!(lookup);
    loop {
        tokio::select! {
            result = &mut lookup => panic!("lookup ended before cancellation: {:?}", result.1),
            _ = tokio::time::sleep(Duration::from_millis(5)) => {
                if marker.exists() { break; }
            }
        }
    }
    let pid: i32 = std::fs::read_to_string(&marker)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    cancel.cancel();
    let (key, outcome) = lookup.await;
    assert!(key.is_none());
    assert_eq!(outcome, KeyLookupOutcome::Cancelled);
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "owned child must already be reaped"
    );
    std::fs::remove_file(&marker).unwrap();
    let mut command = Command::new(executable);
    let (key, outcome) = load(
        SecretString::new("fixture-captured-sentinel".into()),
        &mut command,
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(outcome, KeyLookupOutcome::On);
    assert!(key.is_some());
    assert!(!marker.exists());
}

fn build_startup_probe(dir: &std::path::Path) -> std::path::PathBuf {
    let source = include_str!("../../../../tests/fixtures/decision_gate/startup_probe.rs")
        .replace("__ROOT__", env!("CARGO_MANIFEST_DIR"));
    let source_path = dir.join("probe.rs");
    std::fs::write(&source_path, source).unwrap();
    let binary = dir.join("probe");
    let mut compiler = std::process::Command::new("rustc");
    compiler
        .args(["--edition=2024", "-C", "debuginfo=0"])
        .arg(&source_path)
        .arg("-o")
        .arg(&binary);
    let output = compiler.output().unwrap();
    assert!(
        output.status.success(),
        "startup probe failed to compile: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    binary
}

#[test]
fn isolated_startup_captures_key_before_runtime_and_builder_children() {
    let dir = tempfile::tempdir().unwrap();
    let binary = build_startup_probe(dir.path());
    let env_stub = dir.path().join("print-env");
    std::fs::write(
        &env_stub,
        "#!/bin/sh\nprintf '%s' \"${GEMINI_API_KEY-unset}\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&env_stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = std::process::Command::new(binary)
        .env_clear()
        .env("GEMINI_API_KEY", "fixture-probe-sentinel")
        .arg(env_stub)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"captured=true children=2 inherited=0\n");
}
