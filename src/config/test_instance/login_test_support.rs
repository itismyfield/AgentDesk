//! Claude login instance fixtures shared by KRX test_instance tests and the root-hosted
//! environment tests; they stay in KRX.

use super::*;

pub(crate) fn claude_login_fixture(quoted: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (temp, previous) = tests::fixture();
    let home = temp.path().canonicalize().unwrap();
    let home = if quoted {
        home.join("login home 'active'")
    } else {
        home
    };
    let root = home
        .join(".adk/dev/instances")
        .join(previous.file_name().unwrap());
    std::fs::create_dir_all(root.parent().unwrap()).unwrap();
    std::fs::rename(&previous, &root).unwrap();
    for name in [
        "manifest.json",
        "receipt.json",
        "config/agentdesk.yaml",
        "config/org.yaml",
    ] {
        let path = root.join(name);
        let body = std::fs::read_to_string(&path)
            .unwrap()
            .replace(previous.to_str().unwrap(), root.to_str().unwrap());
        std::fs::write(path, body).unwrap();
    }
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    for binary in [
        home.join(".local/bin/claude"),
        root.join("fixture-worktree/target/debug/claude"),
    ] {
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "#!/bin/sh\nprintf '2.1.325\\n'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    std::fs::write(home.join("active-slot"), "3").unwrap();
    let cswap = home.join(".local/bin/cswap");
    std::fs::write(&cswap, format!("#!/bin/sh\nvalue=$(/bin/cat {})\n[ \"$value\" != unavailable ] || exit 1\nprintf '{{\"activeAccountNumber\":%s}}\\n' \"$value\"\n", crate::services::process::shell_escape(&home.join("active-slot").to_string_lossy()))).unwrap();
    let metadata = home.join("metadata-probe");
    std::fs::write(&metadata, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    for path in [cswap, metadata] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    (temp, root, home)
}

pub(crate) fn bind_claude_login(root: &Path, home: &Path) {
    let login = serde_json::json!({"mode":"default_active","account_slot":3,
        "login_home":home,"config_dir":root.join("home/.adk/profiles/claude/test-claude"),"cli":home.join(".local/bin/claude")});
    for name in ["manifest.json", "receipt.json"] {
        let path = root.join(name);
        let mut value = read_json(&path).unwrap();
        value["claude_login"] = login.clone();
        std::fs::write(path, value.to_string()).unwrap();
    }
    let path = root.join("config/org.yaml");
    let mut org = read_json(&path).unwrap();
    org["provider_auth_profiles"]["test-claude"]["home"] =
        serde_json::json!(root.join("home/.adk/profiles/claude/test-claude"));
    std::fs::write(path, org.to_string()).unwrap();
}

pub(crate) fn review_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (temp, root, home) = claude_login_fixture(true);
    bind_claude_login(&root, &home);
    let sentinel = root.join("runtime/provider-started");
    let body = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then printf '2.1.325\\n'; exit 0; fi\ntouch {}\nprintf 'fixture-ai\\n'\n",
        crate::services::process::shell_escape(&sentinel.to_string_lossy())
    );
    std::fs::write(home.join(".local/bin/claude"), body).unwrap();
    let slot = home.join("active-slot");
    std::fs::write(&slot, "3").unwrap();
    let cswap = home.join(".local/bin/cswap");
    std::fs::write(&cswap, format!("#!/bin/sh\nvalue=$(/bin/cat {})\n[ \"$value\" != unavailable ] || exit 1\nprintf '{{\"activeAccountNumber\":%s}}\\n' \"$value\"\n",crate::services::process::shell_escape(&slot.to_string_lossy()))).unwrap();
    let meta = home.join("metadata-probe");
    std::fs::write(&meta, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    for p in [cswap, meta] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    (temp, root, home)
}
