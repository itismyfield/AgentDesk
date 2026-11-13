use super::*;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MODE: &str = "ADK_TEST_DECISION_BOOT_MODE";
const TEST: &str = "cli::dcserver::decision_boot::tests::startup_lookup_logs_once_with_fixed_availability_and_sigterm_restore";
static PREVIOUS_SIGTERM: AtomicBool = AtomicBool::new(false);
extern "C" fn previous_sigterm(_: libc::c_int) {
    PREVIOUS_SIGTERM.store(true, Ordering::SeqCst);
}

fn probe_child(mode: &str) {
    if mode == "cancel" || mode == "restore" {
        unsafe {
            libc::signal(libc::SIGTERM, previous_sigterm as *const () as usize);
        }
    }
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(std::io::stdout)
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let boot = runtime.block_on(initialize(None));
    println!("boot={boot}");
    if mode == "restore" {
        unsafe {
            libc::raise(libc::SIGTERM);
        }
    }
    println!("restored={}", PREVIOUS_SIGTERM.load(Ordering::SeqCst));
    if let Some(gate) = decision_gate::installed() {
        let scope = decision_gate::Scope {
            feature: decision_gate::Feature::IssueDuplicate,
            repository: Some("ADK".into()),
        };
        println!("availability={:?}", gate.availability(&scope));
    }
}

fn probe(mode: &str, path: &std::ffi::OsStr) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.env_clear().env(MODE, mode).env("PATH", path).args([
        TEST,
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ]);
    command
}

#[test]
fn startup_lookup_logs_once_with_fixed_availability_and_sigterm_restore() {
    if let Ok(mode) = std::env::var(MODE) {
        probe_child(&mode);
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("agent-op");
    let marker = dir.path().join("calls");
    let path = std::env::join_paths([
        dir.path(),
        std::path::Path::new("/usr/bin"),
        std::path::Path::new("/bin"),
    ])
    .unwrap();
    let write_stub = |body: &str| {
        std::fs::write(
            &executable,
            format!("#!/bin/sh\nprintf x >> '{}'\n{body}\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    };
    for (mode, body, expected) in [
        ("lookup", "printf fixture-startup-sentinel", "On"),
        ("lookup", "exit 0", "Off(no_key)"),
        (
            "lookup",
            "echo fixture-startup-sentinel >&2; exit 9",
            "Off(auth_failed)",
        ),
        ("lookup", "exec /bin/sleep 30", "Off(lookup_timeout)"),
        ("restore", "exit 0", "Off(no_key)"),
    ] {
        write_stub(body);
        let output = probe(mode, &path).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-startup-sentinel"));
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(!output.contains("fixture-startup-sentinel"));
        assert!(output.contains(expected), "{output}");
        assert!(output.contains("boot=true"), "{output}");
        if expected.starts_with("Off") {
            assert!(output.contains("availability=Err(MissingKey)"), "{output}");
        }
        assert_eq!(
            output.matches("Decision gate availability").count(),
            1,
            "{output}"
        );
        if mode == "restore" {
            assert!(output.contains("restored=true"), "{output}");
        }
    }
    assert_eq!(std::fs::read(&marker).unwrap(), b"xxxxx");
    let owned_pid = dir.path().join("owned-pid");
    write_stub(&format!(
        "echo $$ > '{}'\nexec /bin/sleep 30",
        owned_pid.display()
    ));
    let mut unrelated = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let mut child = probe("cancel", &path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !owned_pid.exists() && Instant::now() < deadline {
        assert!(
            child.try_wait().unwrap().is_none(),
            "probe exited before lookup started"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(owned_pid.exists());
    let pid: i32 = std::fs::read_to_string(owned_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-startup-sentinel"));
    let output = String::from_utf8(output.stdout).unwrap();
    assert!(!output.contains("fixture-startup-sentinel"));
    assert!(output.contains("boot=false"), "{output}");
    assert!(output.contains("restored=true"), "{output}");
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "lookup child must be reaped before startup returns"
    );
    assert!(
        unrelated.try_wait().unwrap().is_none(),
        "unowned child must survive"
    );
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
}
