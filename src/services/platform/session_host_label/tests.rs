use super::*;

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
        let label =
            super::super::shell::with_hostname_for_test("MacBook-Pro-14", session_host_label);
        assert_eq!(label, "MacBook-Pro-14");
        // The process cache is shared across readers, even when their OS observations differ.
        let readers: Vec<_> = (0..4)
            .map(|_| std::thread::spawn(session_host_label))
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
                .env("AGENTDESK_ROOT_DIR", root.path())
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
