#![cfg(unix)]

use super::*;

pub(crate) fn fixture_checkout(path: &Path) -> LandingTarget {
    fn git(path: &Path, args: &[&str]) -> String {
        let result = Command::new("git")
            .current_dir(path)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.com",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap().trim().into()
    }
    git(path, &["init", "-q", "--initial-branch=main"]);
    git(
        path,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/project.git",
        ],
    );
    git(path, &["commit", "-q", "--allow-empty", "-m", "initial"]);
    LandingTarget {
        repository: "owner/project".into(),
        head_sha: git(path, &["rev-parse", "HEAD"]),
        head_ref: "campaign/fixture".into(),
        base_ref: "main".into(),
        pin_hash: "pin".into(),
    }
}

pub(crate) fn tools(path: &Path, scan: &str) -> LandingChecks {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.join("cargo-shim")).unwrap();
    std::fs::write(
        path.join("prepush_gates.sh"),
        r#"set -eu
test "$PWD" = "$1"
test "$PREPUSH_BASE" = "main"
case "${2:-}" in
    --static-only) printf 'PREPUSH_STATIC_OK %s\n' "$(git rev-parse HEAD)" ;;
    '') printf 'PREPUSH_TREE_OK %s\nPREPUSH_OK\n' "$(git rev-parse HEAD)" ;;
    *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    std::fs::write(path.join("source_scan_readers.py"), format!(
        "import os, sys\nassert sys.argv[1] == os.getcwd()\nassert sys.argv[2:5] == ['--base', 'origin/main', '--run']\nprint({scan:?})\n"
    )).unwrap();
    let cargo = path.join("cargo-shim/cargo");
    std::fs::write(&cargo, "#!/bin/sh\ntest \"$*\" = 'check --lib --tests'\n").unwrap();
    std::fs::set_permissions(cargo, std::fs::Permissions::from_mode(0o700)).unwrap();
    LandingChecks::new(path).unwrap()
}

#[test]
fn m6_actual_preflight_commands_require_the_exact_head() {
    let checkout = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let target = fixture_checkout(checkout.path());
    let checks = tools(
        bin.path(),
        "SOURCE_SCAN modules=1 passed=1 failed=0 pg_not_run=0 ledger_known=0 cargo_rc=0",
    );
    let deadline = Utc::now() + chrono::TimeDelta::seconds(30);
    for mode in [PreflightMode::Full, PreflightMode::PreTlc] {
        let receipt = checks
            .run(checkout.path(), "host", &target, mode, deadline)
            .unwrap();
        assert_eq!(receipt.head_sha, target.head_sha);
        assert_eq!(receipt.mode, mode);
    }

    std::fs::write(
        bin.path().join("prepush_gates.sh"),
        "printf 'PREPUSH_STATIC_OK stale\\n'\n",
    )
    .unwrap();
    assert!(matches!(
        checks.run(
            checkout.path(),
            "host",
            &target,
            PreflightMode::PreTlc,
            deadline
        ),
        Err(PreflightError::Check("prepush receipt"))
    ));

    std::fs::write(checkout.path().join("drift"), "dirty").unwrap();
    assert!(matches!(
        checks.run(
            checkout.path(),
            "host",
            &target,
            PreflightMode::PreTlc,
            deadline
        ),
        Err(PreflightError::Checkout)
    ));
}

#[test]
fn m6_source_scan_cannot_pass_zero_selected_readers() {
    let checkout = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let target = fixture_checkout(checkout.path());
    let deadline = Utc::now() + chrono::TimeDelta::seconds(30);
    let checks = tools(
        bin.path(),
        "SOURCE_SCAN modules=1 passed=0 failed=0 pg_not_run=0 ledger_known=0 cargo_rc=0",
    );
    assert!(matches!(
        checks.run(
            checkout.path(),
            "host",
            &target,
            PreflightMode::PreTlc,
            deadline
        ),
        Err(PreflightError::Check("source scan results"))
    ));
}
