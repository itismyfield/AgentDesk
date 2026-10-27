use super::*;

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
