use super::*;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

fn instance() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp
        .path()
        .canonicalize()
        .unwrap()
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(root.join("credential")).unwrap();
    fs::set_permissions(root.join("credential"), fs::Permissions::from_mode(0o700)).unwrap();
    (temp, root)
}

#[test]
fn stdin_credentials_use_fresh_private_files_without_secret_diagnostics() {
    let (_temp, root) = instance();
    credential_write(&root, "claude_bot_token", b"fake-sentinel\n".as_slice()).unwrap();
    let path = root.join("credential/claude_bot_token");
    assert_eq!(fs::read(&path).unwrap(), b"fake-sentinel");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let error = credential_write(&root, "claude_bot_token", b"replacement".as_slice()).unwrap_err();
    assert_eq!(error, "credential write rejected");
    assert_eq!(fs::read(&path).unwrap(), b"fake-sentinel");
    for input in [
        b"".as_slice(),
        b"fake-sentinel\nsecond",
        b" fake-sentinel",
        b"fake-sentinel\r",
    ] {
        assert_eq!(
            credential_write(&root, "driver_bot_token", input).unwrap_err(),
            error
        );
    }
    assert_eq!(
        credential_write(&root, "fake-sentinel", b"fake-sentinel".as_slice()).unwrap_err(),
        error
    );
    assert_eq!(
        credential_write(&root, "driver_bot_token", vec![b'x'; 4097].as_slice()).unwrap_err(),
        error
    );
    assert!(!root.join("credential/driver_bot_token").exists());
}

#[test]
fn stdin_boundary_rejects_release_aliases_and_link_targets_without_writes() {
    let (_temp, root) = instance();
    let marker = root.join("marker");
    fs::write(&marker, b"unchanged").unwrap();
    symlink(&marker, root.join("credential/driver_bot_token")).unwrap();
    assert!(credential_write(&root, "driver_bot_token", b"fake-sentinel".as_slice()).is_err());
    assert_eq!(fs::read(&marker).unwrap(), b"unchanged");
    let release = root
        .join(".ADK/ReLeAsE")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(release.join("credential")).unwrap();
    for directory in [&release, &release.join("credential")] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert!(credential_write(&release, "driver_bot_token", b"fake-sentinel".as_slice()).is_err());
    assert!(!release.join("credential/driver_bot_token").exists());
}
