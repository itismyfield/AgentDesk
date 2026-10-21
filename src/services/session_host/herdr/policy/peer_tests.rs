use super::super::file;
use super::*;
use std::time::{Duration, UNIX_EPOCH};

#[test]
fn policy_socket_parent_resolves_final_symlink() {
    let root = tempfile::tempdir().unwrap();
    let config_dir = root.path().join("config");
    let live_dir = root.path().join("live");
    std::fs::create_dir(&config_dir).unwrap();
    std::fs::create_dir(&live_dir).unwrap();
    let live_socket = live_dir.join("server.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&live_socket).unwrap();
    let logical_socket = config_dir.join("server.sock");
    std::os::unix::fs::symlink(&live_socket, &logical_socket).unwrap();
    let observed = socket_directory(&logical_socket).unwrap();
    assert_eq!(
        observed,
        (
            std::fs::canonicalize(&live_socket).unwrap(),
            file::directory_identity(&live_dir).unwrap()
        ),
        "socket_parent_resolves_final_symlink_contract"
    );
    assert_ne!(observed.1, file::directory_identity(&config_dir).unwrap());
}

#[cfg(unix)]
struct Fixture {
    root: tempfile::TempDir,
    checker: PathBuf,
}

#[cfg(unix)]
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let checker = root.path().join("herdr");
        std::fs::write(&checker, b"fixture executable image").unwrap();
        Self { root, checker }
    }

    fn observation(&self) -> PeerObservation {
        let witness = ServerWitness::for_test(23);
        let started = UNIX_EPOCH + Duration::from_secs(1000);
        let token = file::image(&self.checker).unwrap();
        PeerObservation {
            expected_socket: witness.socket.clone(),
            socket_dir: file::directory_identity(self.root.path()).unwrap(),
            hello: ServerHello {
                witness: witness.clone(),
                started,
                version: "0.9.3".into(),
                connected_at: started + Duration::from_secs(5),
            },
            current: witness.clone(),
            process: ProcessStart {
                identity: witness.start,
                wall_clock: started,
            },
            protocol: 22,
            environment: PeerEnvironment {
                home: vec![self.root.path().to_str().unwrap().into()],
                ..PeerEnvironment::default()
            },
            executable: ExecutableImage {
                checker: self.checker.clone(),
                checker_token: token.clone(),
                loaded: Some(loaded_image::fixture(token.dev_ino())),
            },
        }
    }
}

#[cfg(unix)]
#[test]
fn policy_peer_accepts_only_the_peers_default_config() {
    let fixture = Fixture::new();
    for explicit in [false, true] {
        let mut observed = fixture.observation();
        if explicit {
            observed.environment.xdg_config_home =
                vec![fixture.root.path().join(".config").to_str().unwrap().into()];
            observed.environment.herdr_config_path = vec![
                fixture
                    .root
                    .path()
                    .join(".config/herdr/config.toml")
                    .to_str()
                    .unwrap()
                    .into(),
            ];
        }
        let peer = validate(observed).unwrap();
        assert_eq!(peer.home, fixture.root.path());
        assert_eq!(
            peer.config,
            fixture.root.path().join(".config/herdr/config.toml")
        );
        assert_eq!(peer.checker, fixture.checker);
        peer.image.unchanged().unwrap();
    }
}

#[cfg(unix)]
#[test]
fn policy_peer_refuses_duplicate_empty_relative_and_missing_home() {
    let fixture = Fixture::new();
    for field in ["home", "xdg", "config"] {
        for values in [
            vec!["/one".into(), "/two".into()],
            vec![String::new()],
            vec!["   ".into()],
            vec!["relative".into()],
            vec!["$HOME/config".into()],
            vec!["~/config".into()],
        ] {
            let mut observed = fixture.observation();
            match field {
                "home" => observed.environment.home = values,
                "xdg" => observed.environment.xdg_config_home = values,
                _ => observed.environment.herdr_config_path = values,
            }
            assert!(
                matches!(validate(observed), Err(Unverified::Peer)),
                "{field}"
            );
        }
    }
    let mut observed = fixture.observation();
    observed.environment.home.clear();
    assert!(matches!(validate(observed), Err(Unverified::Peer)));
}

#[cfg(unix)]
#[test]
fn policy_peer_refuses_nondefault_xdg_even_with_default_override() {
    let fixture = Fixture::new();
    for xdg in [false, true] {
        let mut observed = fixture.observation();
        if xdg {
            observed.environment.xdg_config_home = vec!["/custom-config".into()];
            observed.environment.herdr_config_path = vec![
                fixture
                    .root
                    .path()
                    .join(".config/herdr/config.toml")
                    .to_str()
                    .unwrap()
                    .into(),
            ];
        } else {
            observed.environment.herdr_config_path = vec!["/custom-config/config.toml".into()];
        }
        assert!(matches!(validate(observed), Err(Unverified::Ambiguous)));
    }
}

#[cfg(unix)]
#[test]
fn policy_peer_checks_version_protocol_start_and_current_witness() {
    let fixture = Fixture::new();
    type Edit = fn(&mut PeerObservation);
    let edits: [(Edit, Unverified); 8] = [
        (|peer| peer.protocol = 23, Unverified::Unsupported),
        (
            |peer| peer.hello.version = "0.9.4".into(),
            Unverified::Unsupported,
        ),
        (|peer| peer.current.pid += 1, Unverified::Peer),
        (
            |peer| peer.process.identity = StartIdentity::LinuxTicks(2),
            Unverified::Peer,
        ),
        (
            |peer| peer.process.wall_clock += Duration::from_secs(1),
            Unverified::Peer,
        ),
        (
            |peer| peer.hello.connected_at = UNIX_EPOCH,
            Unverified::Peer,
        ),
        (
            |peer| peer.expected_socket = "/other/herdr.sock".into(),
            Unverified::Peer,
        ),
        (
            |peer| peer.executable.loaded = Some(loaded_image::fixture((0, 0))),
            Unverified::Identity,
        ),
    ];
    for (edit, expected) in edits {
        let mut observed = fixture.observation();
        edit(&mut observed);
        assert_eq!(
            validate(observed),
            Err(expected),
            "peer observation source contract"
        );
    }
}

#[cfg(unix)]
#[test]
fn policy_peer_path_stat_alone_or_another_loaded_image_never_passes() {
    let fixture = Fixture::new();
    let other = fixture.root.path().join("another-image");
    std::fs::write(&other, b"fixture executable image").unwrap();
    for loaded in [None, Some(file::image(&other).unwrap())] {
        let mut observed = fixture.observation();
        observed.executable.loaded = loaded.map(|token| loaded_image::fixture(token.dev_ino()));
        assert!(
            matches!(validate(observed), Err(Unverified::Identity)),
            "loaded-image identity contract"
        );
    }
    let mut observed = fixture.observation();
    observed.executable.checker = other;
    assert!(matches!(validate(observed), Err(Unverified::Identity)));
}

#[cfg(unix)]
#[test]
fn policy_peer_checker_replacement_invalidates_its_observed_image() {
    let fixture = Fixture::new();
    let observed = fixture.observation();
    let replacement = fixture.root.path().join("replacement");
    std::fs::write(&replacement, b"fixture executable image").unwrap();
    std::fs::rename(replacement, &fixture.checker).unwrap();
    assert!(matches!(validate(observed), Err(Unverified::Changed)));
}

#[cfg(unix)]
#[test]
fn policy_peer_same_loaded_image_may_have_a_different_logical_path() {
    let fixture = Fixture::new();
    let alias = fixture.root.path().join("checker-alias");
    std::os::unix::fs::symlink(&fixture.checker, &alias).unwrap();
    let mut observed = fixture.observation();
    observed.executable.checker = alias.clone();
    observed.executable.checker_token = file::image(&alias).unwrap();
    assert_eq!(validate(observed).unwrap().checker, alias);
}

#[test]
fn policy_peer_mapped_inode_cannot_be_replaced_with_path_stat() {
    let fixture = Fixture::new();
    let mut observed = fixture.observation();
    let running = observed.executable.loaded.clone();
    let replacement = fixture.root.path().join("new-image");
    std::fs::write(&replacement, b"replacement executable image").unwrap();
    std::fs::rename(replacement, &fixture.checker).unwrap();
    observed.executable.checker_token = file::image(&fixture.checker).unwrap();
    observed.executable.loaded = running;
    assert_eq!(
        validate(observed),
        Err(Unverified::Identity),
        "loaded_image_vnode_identity_contract"
    );
}

#[test]
fn policy_loaded_image_construction_is_os_adapter_only() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/services/session_host/herdr/policy");
    for entry in std::fs::read_dir(&directory).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if path.extension().is_none_or(|ext| ext != "rs")
            || name.ends_with("_tests.rs")
            || name == "tests.rs"
            || name == "loaded_image.rs"
        {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("LoadedImage {"),
            "loaded_image_adapter_constructor_contract: {}",
            path.display()
        );
    }
    let adapter = std::fs::read_to_string(directory.join("loaded_image.rs")).unwrap();
    assert!(
        !adapter.contains("file::image("),
        "loaded_image_adapter_never_path_stats_contract"
    );
    let linux = adapter
        .split("#[cfg(target_os = \"linux\")]")
        .nth(1)
        .and_then(|body| body.split("#[cfg(not(target_os").next())
        .expect("Linux loaded-image adapter is present");
    assert!(
        linux.contains("\"/proc/{pid}/exe\""),
        "loaded_image_linux_procfs_adapter_contract"
    );
    assert!(
        !linux.contains("process_exec_path") && !linux.contains("canonicalize"),
        "loaded_image_linux_adapter_forbids_pathname_fallback_contract"
    );
}
