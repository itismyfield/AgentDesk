use super::*;
use std::{fs, os::unix::io::AsRawFd};

#[test]
fn p5_singleton_requires_exclusive_ownership_across_processes() {
    const ROOT: &str = "ADK_TEST_REPLAY_LOCK_PROBE";
    if let Ok(root) = std::env::var(ROOT) {
        assert!(
            acquire_for_tests(Path::new(&root)).is_err(),
            "an unowned flock cannot mint recovery authority"
        );
        println!("SINGLETON_OVERLAP_REFUSED");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("runtime")).unwrap();
    let owner = acquire_for_tests(root.path()).unwrap();
    let name = concat!(
        module_path!(),
        "::p5_singleton_requires_exclusive_ownership_across_processes"
    )
    .strip_prefix("agentdesk::")
    .unwrap();
    let peer = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(ROOT, root.path())
        .output()
        .unwrap();
    assert!(
        peer.status.success(),
        "{} {}",
        String::from_utf8_lossy(&peer.stdout),
        String::from_utf8_lossy(&peer.stderr)
    );
    assert!(String::from_utf8_lossy(&peer.stdout).contains("SINGLETON_OVERLAP_REFUSED"));
    assert!(owner.covers(root.path()));
    let retained = owner.clone();
    drop(owner);
    assert!(
        acquire_for_tests(root.path()).is_err(),
        "a retained recovery context keeps the lock alive"
    );
    drop(retained);
    assert!(
        acquire_for_tests(root.path()).is_ok(),
        "the same root can restart after its owner exits"
    );
}

#[test]
fn lock_pid_write_removes_stale_suffix() {
    let lock = tempfile::NamedTempFile::new().expect("temporary lock file");
    let current_pid = std::process::id().to_string();
    fs::write(lock.path(), format!("{current_pid}999")).expect("seed longer PID");
    let file = fs::OpenOptions::new()
        .write(true)
        .truncate(false)
        .open(lock.path())
        .expect("open existing lock file");
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "acquire single-instance lock before PID write"
    );

    write_dcserver_lock_pid(&file).expect("write current PID");

    assert_eq!(
        fs::read(lock.path()).expect("read PID"),
        current_pid.as_bytes()
    );
}
