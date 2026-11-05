use super::*;

#[test]
fn default_reads_follow_each_host_without_touching_runtime_roots() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for host in ["First-Mac", "second-mac"] {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        super::super::shell::with_hostname_for_test(host, || {
            assert_eq!(session_host_label(), host);
            assert_eq!(session_host_label(), host);
        });
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

#[test]
fn explicit_path_scope_restores_default_after_unwind() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session_host_label");
    fs::write(&path, "MacBook-Pro-14\n").unwrap();
    super::super::shell::with_hostname_for_test("macbook-pro-14", || {
        let panic = std::panic::catch_unwind(|| {
            with_session_host_label_path_for_test(&path, || {
                assert_eq!(session_host_label(), "MacBook-Pro-14");
                panic!("unwind the explicit fixture scope");
            });
        });
        assert!(panic.is_err());
        assert_eq!(session_host_label(), "macbook-pro-14");
    });
}

#[test]
fn spelling_is_preserved_but_real_rename_and_corruption_are_replaced() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session_host_label");
    assert_eq!(
        load_label(&path, "MacBook-Pro-14").unwrap(),
        "MacBook-Pro-14"
    );
    assert_eq!(
        load_label(&path, "macbook-pro-14").unwrap(),
        "MacBook-Pro-14"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "MacBook-Pro-14\n");
    assert_eq!(load_label(&path, "Other-Mac").unwrap(), "Other-Mac");
    assert_eq!(fs::read_to_string(&path).unwrap(), "Other-Mac\n");
    for bytes in [
        b"".as_slice(),
        b"wrong:session",
        b"../elsewhere",
        b"a\nb",
        b"\xff",
    ] {
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            load_label(&path, "MacBook-Pro-14").unwrap(),
            "MacBook-Pro-14"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "MacBook-Pro-14\n");
    }
    assert_eq!(
        normalized("macbook-pro-14.tail9b57d2.ts.net"),
        normalized("MacBook-Pro-14")
    );
}

#[test]
fn concurrent_processes_first_label_agree() {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    const NAME: &str =
        "services::platform::session_host_label::tests::concurrent_processes_first_label_agree";
    if std::env::var_os("ADK_LABEL_RACE_CHILD").is_some() {
        std::io::stdin().read_exact(&mut [0]).unwrap();
        let path = std::path::PathBuf::from(std::env::var_os("ADK_LABEL_RACE_PATH").unwrap());
        let label = load_label(&path, "MacBook-Pro-14").unwrap();
        assert_eq!(label, "MacBook-Pro-14");
        // Every reader names the fixture file; ordinary test calls never persist a label.
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || load_label(&path, "macbook-pro-14").unwrap())
            })
            .collect();
        for reader in readers {
            assert_eq!(reader.join().unwrap(), label);
        }
        println!("SESSION_HOST_LABEL={label}");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut children: Vec<_> = (0..2)
        .map(|_| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env("ADK_LABEL_RACE_CHILD", "1")
                .env(
                    "ADK_LABEL_RACE_PATH",
                    root.path().join("runtime/session_host_label"),
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in &mut children {
        child.stdin.take().unwrap().write_all(b"x").unwrap();
    }
    for child in children {
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("1 passed; 0 failed"));
        assert!(stdout.contains("SESSION_HOST_LABEL=MacBook-Pro-14"));
    }
    assert_eq!(
        fs::read_to_string(root.path().join("runtime/session_host_label")).unwrap(),
        "MacBook-Pro-14\n"
    );
}
