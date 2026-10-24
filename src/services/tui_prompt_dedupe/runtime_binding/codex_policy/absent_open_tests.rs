use super::*;
use serde_json::{Value, json};

fn owned() -> Value {
    json!({"codex_ownership": {
        "execution_nonce": "captured", "proof_seq": 7, "dev": 8, "ino": 9
    }})
}

#[test]
fn rollout_ownership_accepts_only_known_exact_nonce_with_complete_shape() {
    assert!(rollout_ownership_matches(&json!({}), "captured", false));
    assert!(rollout_ownership_matches(&owned(), "captured", true));
    assert!(!rollout_ownership_matches(&owned(), "captured", false));
    assert!(!rollout_ownership_matches(&owned(), "other", true));
    for value in [json!(null), json!([]), json!("marker"), json!(0)] {
        assert!(!rollout_ownership_matches(&value, "captured", true));
    }
    for value in [json!(null), json!([]), json!("ownership"), json!(0)] {
        assert!(!rollout_ownership_matches(
            &json!({"codex_ownership": value}),
            "captured",
            true
        ));
    }
    for field in ["execution_nonce", "proof_seq", "dev", "ino"] {
        let mut missing = owned();
        missing["codex_ownership"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(
            !rollout_ownership_matches(&missing, "captured", true),
            "{field}"
        );
    }
    for field in ["proof_seq", "dev", "ino"] {
        for value in [json!(null), json!("7"), json!(-1), json!(1.5)] {
            let mut wrong = owned();
            wrong["codex_ownership"][field] = value;
            assert!(
                !rollout_ownership_matches(&wrong, "captured", true),
                "{field}"
            );
        }
    }
    for value in [json!(null), json!(7), json!([]), json!({})] {
        let mut wrong = owned();
        wrong["codex_ownership"]["execution_nonce"] = value;
        assert!(!rollout_ownership_matches(&wrong, "captured", true));
    }
    let mut shape_only = owned();
    for field in ["proof_seq", "dev", "ino"] {
        shape_only["codex_ownership"][field] = json!(u64::MAX);
    }
    assert!(rollout_ownership_matches(&shape_only, "captured", true));
}

#[test]
fn nonce_observation_rejects_conflicts_blank_and_io_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    let paths = [root.path().join("canonical"), root.path().join("legacy")];
    assert_eq!(read_nonce_paths(paths.clone()), SpawnNonceMarker::Absent);
    assert!(!paths[0].exists() && !paths[1].exists());
    fs::write(&paths[0], " captured\n").unwrap();
    assert_eq!(
        read_nonce_paths(paths.clone()),
        SpawnNonceMarker::Known("captured".into())
    );
    fs::write(&paths[1], "captured").unwrap();
    assert_eq!(
        read_nonce_paths(paths.clone()),
        SpawnNonceMarker::Known("captured".into())
    );
    let before = paths.each_ref().map(|path| fs::read(path).unwrap());
    let _ = read_nonce_paths(paths.clone());
    assert_eq!(before, paths.each_ref().map(|path| fs::read(path).unwrap()));
    fs::write(&paths[1], "foreign").unwrap();
    assert_eq!(
        read_nonce_paths(paths.clone()),
        SpawnNonceMarker::Unreadable
    );
    fs::write(&paths[1], " \n").unwrap();
    assert_eq!(
        read_nonce_paths(paths.clone()),
        SpawnNonceMarker::Unreadable
    );
    let directory = root.path().join("directory");
    fs::create_dir(&directory).unwrap();
    assert_eq!(
        read_nonce_paths([paths[0].clone(), directory]),
        SpawnNonceMarker::Unreadable
    );
    fs::write(&paths[0], [0xff]).unwrap();
    fs::write(&paths[1], "captured").unwrap();
    assert_eq!(read_nonce_paths(paths), SpawnNonceMarker::Unreadable);
}

#[test]
fn nonce_observation_accepts_legacy_only_and_refuses_canonical_error() {
    let root = tempfile::tempdir().unwrap();
    let paths = [root.path().join("canonical"), root.path().join("legacy")];
    fs::write(&paths[1], "captured").unwrap();
    assert_eq!(
        read_nonce_paths(paths.clone()),
        SpawnNonceMarker::Known("captured".into())
    );
    fs::create_dir(&paths[0]).unwrap();
    assert_eq!(read_nonce_paths(paths), SpawnNonceMarker::Unreadable);
}

#[test]
fn marker_read_distinguishes_missing_malformed_oversize_and_io_without_writes() {
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("rollout");
    assert!(read_json(&marker).unwrap().is_none());
    assert!(!marker.exists());
    fs::write(&marker, serde_json::to_vec(&owned()).unwrap()).unwrap();
    let before = fs::read(&marker).unwrap();
    assert_eq!(read_json(&marker).unwrap(), Some(owned()));
    assert_eq!(fs::read(&marker).unwrap(), before);
    fs::write(&marker, b"{").unwrap();
    assert!(read_json(&marker).is_err());
    let mut boundary = serde_json::to_vec(&owned()).unwrap();
    boundary.resize(16 * 1024, b' ');
    fs::write(&marker, boundary).unwrap();
    assert_eq!(read_json(&marker).unwrap(), Some(owned()));
    fs::write(&marker, vec![b' '; 16 * 1024 + 1]).unwrap();
    assert!(read_json(&marker).is_err());
    assert!(read_json(root.path()).is_err());
}

#[cfg(unix)]
#[test]
fn readonly_marker_probes_never_create_sessions_or_normalize_its_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let tmux = format!("absent-readonly-{}", uuid::Uuid::new_v4());
    let [nonce_path, _] = marker_paths(&tmux, "spawn_nonce").unwrap();
    let sessions = nonce_path.parent().unwrap();
    assert_eq!(sessions, root.path().join("runtime/sessions"));
    assert_eq!(read_nonce(&tmux), SpawnNonceMarker::Absent);
    assert!(runtime_markers_match(&tmux));
    assert!(!root.path().join("runtime").exists());

    fs::create_dir_all(sessions).unwrap();
    fs::set_permissions(sessions, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(&nonce_path, "captured").unwrap();
    let before = fs::read(&nonce_path).unwrap();
    assert_eq!(
        read_nonce(&tmux),
        SpawnNonceMarker::Known("captured".into())
    );
    assert!(runtime_markers_match(&tmux));
    assert_eq!(fs::read(&nonce_path).unwrap(), before);
    assert_eq!(
        fs::metadata(sessions).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(fs::read_dir(sessions).unwrap().count(), 1);
}
