use super::*;

#[cfg(unix)]
#[test]
fn policy_file_read_is_bounded_while_image_hashing_streams() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    let bytes = vec![b'x'; LIMIT as usize];
    std::fs::write(&path, &bytes).unwrap();
    let snapshot = read(&path).unwrap();
    assert_eq!(snapshot.bytes, bytes);
    assert_eq!(
        snapshot.token.hash,
        <[u8; 32]>::from(Sha256::digest(&bytes))
    );
    snapshot.token.unchanged().unwrap();
    std::fs::write(&path, vec![b'x'; LIMIT as usize + 1]).unwrap();
    assert!(matches!(read(&path), Err(Unverified::File)));
    let executable = image(&path).unwrap();
    executable.unchanged().unwrap();
    assert!(executable.same_image(&executable.clone()));
}

#[cfg(unix)]
#[test]
fn policy_file_same_descriptor_rejects_in_place_and_path_replacement() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    for replace in [false, true] {
        std::fs::write(&path, b"old").unwrap();
        let result = snapshot(&path, Some(LIMIT), true, || {
            if replace {
                let replacement = root.path().join("replacement");
                std::fs::write(&replacement, b"new").unwrap();
                std::fs::rename(replacement, &path).unwrap();
            } else {
                std::fs::write(&path, b"longer").unwrap();
            }
        });
        assert!(matches!(result, Err(Unverified::Changed)), "{result:?}");
    }
}

#[cfg(unix)]
#[test]
fn policy_file_token_detects_changed_bytes_with_preserved_mtime() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    std::fs::write(&path, b"false").unwrap();
    let token = read(&path).unwrap().token;
    std::fs::write(&path, b"other").unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(token.identity.modified))
        .unwrap();
    assert!(matches!(token.unchanged(), Err(Unverified::Changed)));
}

#[cfg(unix)]
#[test]
fn policy_file_keeps_logical_path_and_every_relative_symlink() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("actual");
    std::fs::create_dir(&directory).unwrap();
    let target = directory.join("config.toml");
    std::fs::write(&target, b"policy").unwrap();
    symlink("actual", root.path().join("directory")).unwrap();
    symlink("directory/config.toml", root.path().join("config.toml")).unwrap();
    let logical = root.path().join("config.toml");
    let token = read(&logical).unwrap().token;
    assert_eq!(token.logical_path(), logical);
    assert_eq!(token.resolved_path, target.canonicalize().unwrap());
    for name in ["directory", "config.toml"] {
        assert!(
            token
                .symlink_chain
                .iter()
                .any(|link| link.path.ends_with(name))
        );
    }
    token.unchanged().unwrap();
    std::fs::write(root.path().join("other.toml"), b"policy").unwrap();
    std::fs::remove_file(&logical).unwrap();
    symlink("other.toml", &logical).unwrap();
    assert!(matches!(token.unchanged(), Err(Unverified::Changed)));
}

#[cfg(unix)]
#[test]
fn policy_file_startup_margin_checks_ctime_mtime_and_symlink_times() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("target"), b"policy").unwrap();
    let logical = root.path().join("link");
    symlink("target", &logical).unwrap();
    let mut token = read(&logical).unwrap().token;
    let old = UNIX_EPOCH + Duration::from_secs(10);
    token.identity.modified = old;
    token.identity.changed = old;
    for link in &mut token.symlink_chain {
        link.identity.modified = old;
        link.identity.changed = old;
    }
    let boundary = old + START_MARGIN;
    assert!(!token.predates(boundary));
    let started = boundary + Duration::from_nanos(1);
    assert!(token.predates(started));
    for (link, ctime) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut changed = token.clone();
        let identity = if link {
            &mut changed.symlink_chain.last_mut().unwrap().identity
        } else {
            &mut changed.identity
        };
        if ctime {
            identity.changed = started;
        } else {
            identity.modified = started;
        }
        assert!(
            !changed.predates(started),
            "startup age contract: link={link}, ctime={ctime}"
        );
    }
}

#[cfg(unix)]
#[test]
fn policy_file_refuses_relative_missing_dangling_cycle_and_directory() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    symlink("missing", root.path().join("dangling")).unwrap();
    symlink("cycle", root.path().join("cycle")).unwrap();
    for path in [
        Path::new("relative"),
        root.path(),
        &root.path().join("missing"),
        &root.path().join("dangling"),
        &root.path().join("cycle"),
    ] {
        assert!(read(path).is_err(), "{}", path.display());
    }
}

#[cfg(unix)]
#[test]
fn policy_file_same_target_symlink_replacement_invalidates_token_and_read() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("target"), b"policy").unwrap();
    let logical = root.path().join("logical");
    symlink("target", &logical).unwrap();
    let replace = |relative: &str| {
        let replacement = root.path().join("replacement");
        symlink(relative, &replacement).unwrap();
        std::fs::rename(replacement, &logical).unwrap();
    };
    let only_chain_changed = |before: &FileToken, after: &FileToken| {
        assert_eq!(before.logical_path, after.logical_path);
        assert_eq!(before.resolved_path, after.resolved_path);
        assert_eq!(before.identity, after.identity);
        assert_eq!(before.hash, after.hash);
        assert_eq!(before.limit, after.limit);
        assert_ne!(before.symlink_chain, after.symlink_chain);
    };

    let token = read(&logical).unwrap().token;
    replace("./target");
    only_chain_changed(&token, &read(&logical).unwrap().token);
    assert!(
        matches!(token.unchanged(), Err(Unverified::Changed)),
        "symlink chain token contract"
    );

    replace("target");
    let before_read = read(&logical).unwrap().token;
    let result = snapshot(&logical, Some(LIMIT), true, || replace("./target"));
    only_chain_changed(&before_read, &read(&logical).unwrap().token);
    assert!(
        matches!(result, Err(Unverified::Changed)),
        "symlink chain read contract"
    );
}
