use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn a2_missing_owner_nonce_probe_creates_no_runtime_storage_or_permission_changes() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().unwrap();
    let tmp = root.join("private-tmp");
    std::fs::create_dir(&tmp).unwrap();
    let _tmp = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("TMPDIR", &tmp);
    let name = format!("a2-missing-nonce-{}", uuid::Uuid::new_v4());
    let canonical = marker_path(&name, "spawn_nonce");
    let legacy =
        std::path::PathBuf::from(tmux_common::legacy_tmp_session_path(&name, "spawn_nonce"));
    assert!(canonical.starts_with(&root));
    assert!(legacy.starts_with(&tmp));
    let permissions = (
        std::fs::metadata(&root).unwrap().permissions().mode(),
        std::fs::metadata(&tmp).unwrap().permissions().mode(),
    );
    assert!(!canonical.parent().unwrap().exists());
    assert_eq!(nonce(&name), Ok(None));
    assert!(
        !canonical.parent().unwrap().exists(),
        "read-only nonce probe created the sessions directory"
    );
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
    assert_eq!(
        (
            std::fs::metadata(&root).unwrap().permissions().mode(),
            std::fs::metadata(&tmp).unwrap().permissions().mode()
        ),
        permissions
    );
}

#[test]
fn a2_owner_nonce_uses_legacy_only_for_missing_canonical_and_keeps_opaque_bytes() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().unwrap();
    let tmp = root.join("private-tmp");
    std::fs::create_dir(&tmp).unwrap();
    let _tmp = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("TMPDIR", &tmp);
    let name = format!("a2-present-nonce-{}", uuid::Uuid::new_v4());
    let canonical = marker_path(&name, "spawn_nonce");
    let legacy =
        std::path::PathBuf::from(tmux_common::legacy_tmp_session_path(&name, "spawn_nonce"));
    assert!(canonical.starts_with(&root));
    assert!(legacy.starts_with(&tmp));
    std::fs::write(&legacy, b"legacy-opaque").unwrap();
    assert_eq!(nonce(&name), Ok(Some(b"legacy-opaque".to_vec())));
    std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
    std::fs::set_permissions(
        canonical.parent().unwrap(),
        std::fs::Permissions::from_mode(0o750),
    )
    .unwrap();
    for bytes in [&b" \n"[..], &[0xff][..]] {
        std::fs::write(&canonical, bytes).unwrap();
        assert!(
            nonce(&name).is_err(),
            "bad canonical bytes fell through to a valid legacy marker"
        );
        assert_eq!(std::fs::read(&legacy).unwrap(), b"legacy-opaque");
    }
    std::fs::remove_file(&canonical).unwrap();
    std::fs::create_dir(&canonical).unwrap();
    assert!(
        nonce(&name).is_err(),
        "canonical read failure fell through to legacy"
    );
    std::fs::remove_dir(&canonical).unwrap();
    let opaque = b"old opaque nonce / value\n";
    std::fs::write(&canonical, opaque).unwrap();
    assert_eq!(nonce(&name), Ok(Some(opaque.to_vec())));
    assert_eq!(
        std::fs::metadata(canonical.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
    assert_eq!(std::fs::read(&legacy).unwrap(), b"legacy-opaque");
}
