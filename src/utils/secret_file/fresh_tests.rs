use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

#[test]
fn fresh_policy_shares_private_modes_and_preserves_existing_targets() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().canonicalize().unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    let path = parent.join("token");
    write_secret_file_with_policy(&path, b"first", SecretFilePolicy::FreshPrivate).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"first");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        write_secret_file_with_policy(&path, b"second", SecretFilePolicy::FreshPrivate).is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), b"first");
    write_secret_file(&path, b"replacement").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"replacement");
    let link = parent.join("link");
    symlink(&path, &link).unwrap();
    assert!(
        write_secret_file_with_policy(&link, b"changed", SecretFilePolicy::FreshPrivate).is_err()
    );
    let alias = parent.join("alias");
    symlink(&parent, &alias).unwrap();
    assert!(
        write_secret_file_with_policy(
            &alias.join("outside"),
            b"changed",
            SecretFilePolicy::FreshPrivate
        )
        .is_err()
    );
    assert!(!parent.join("outside").exists());
    assert_eq!(fs::read(&path).unwrap(), b"replacement");
}
