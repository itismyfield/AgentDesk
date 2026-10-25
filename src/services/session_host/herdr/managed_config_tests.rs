use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::path::PersonalPaths;
use super::*;
use crate::config::session_hosts::{BootSessionHosts, EndpointConfig, EndpointMode};
use crate::services::session_host::herdr::contract::{
    HerdrOutcome, HerdrTransport, ServerHello, ServerWitness, Witnessed,
};
use crate::services::session_host::herdr::model::{HerdrCall, HerdrEndpoint};
use crate::services::session_host::herdr::observe::{
    CANONICAL_CONFIG, ConfigRead, RestoreResume, RestoreUnverified, ServerProvenance,
    read_restore_resume_with,
};
use crate::services::session_host::herdr::provenance::{OsProvenance, ProcessStart, StartIdentity};

fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    personal: PersonalPaths,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("managed");
        let user = root.path().join("user");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&user).unwrap();
        Self {
            _root: root,
            home,
            personal: PersonalPaths {
                home: user,
                xdg: None,
            },
        }
    }

    fn boot(&self, node: Option<&str>, homes: &[(&Path, &str, &str)]) -> BootSessionHosts {
        let mut config = crate::config::Config::default();
        config.cluster.instance_id = node.map(str::to_string);
        config.session_hosts.herdr.endpoints = homes
            .iter()
            .enumerate()
            .map(|(i, (home, node, socket))| {
                (
                    format!("ep-{i}"),
                    EndpointConfig {
                        mode: EndpointMode::LegacyDedicated,
                        execution_node: node.to_string(),
                        socket_path: PathBuf::from(socket),
                        herdr_home: home.to_path_buf(),
                        herdr_session: "adk".into(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        BootSessionHosts::from_config(&config).unwrap()
    }

    fn target(&self) -> ManagedHome {
        let boot = self.boot(
            Some("local"),
            &[(&self.home, "local", "/tmp/isolated.sock")],
        );
        local(&boot, &self.personal).unwrap().remove(0)
    }

    fn config(&self, bytes: &[u8]) -> PathBuf {
        let path = self.home.join("config.toml");
        fs::write(&path, bytes).unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_modified(at(900))
            .unwrap();
        path
    }

    fn state(&self, text: &str) {
        fs::write(self.home.join(publication::STATE_FILE), text).unwrap();
    }

    fn reading(&self) -> RestoreResume {
        read_restore_resume_with(
            &Peer,
            &endpoint(&self.home),
            &FileProof {
                home: self.home.clone(),
            },
        )
    }
}

fn endpoint(home: &Path) -> HerdrEndpoint {
    HerdrEndpoint::new("local", "fixture", Path::new("/tmp/isolated.sock"), "adk")
        .unwrap()
        .with_herdr_home(home)
        .unwrap()
}

fn witness() -> ServerWitness {
    ServerWitness {
        socket: "/tmp/isolated.sock".into(),
        pid: 123,
        start: StartIdentity::LinuxTicks(100),
    }
}

struct Peer;
impl HerdrTransport for Peer {
    fn hello(&self) -> Result<ServerHello, RestoreUnverified> {
        Ok(ServerHello {
            witness: witness(),
            started: at(1000),
            connected_at: at(2000),
            version: "0.9.3".into(),
        })
    }
    fn server_witness(&self) -> Witnessed {
        Ok(witness())
    }
    fn call(&self, _: &HerdrCall) -> (HerdrOutcome, Witnessed) {
        panic!("E7 must not mutate")
    }
    fn call_with_witness(&self, _: &HerdrCall, _: &ServerWitness) -> HerdrOutcome {
        panic!("E7 must not mutate")
    }
}

struct FileProof {
    home: PathBuf,
}
impl ServerProvenance for FileProof {
    fn process_start(&self, _: u32) -> Result<ProcessStart, RestoreUnverified> {
        Ok(ProcessStart {
            identity: witness().start,
            wall_clock: at(1000),
        })
    }
    fn process_env(&self, _: u32, key: &str) -> Result<Vec<String>, RestoreUnverified> {
        Ok(vec![
            self.home
                .join(if key == "HERDR_CONFIG_PATH" {
                    "config.toml"
                } else {
                    "xdg"
                })
                .to_str()
                .unwrap()
                .into(),
        ])
    }
    fn read_config(&self, path: &Path) -> Result<ConfigRead, RestoreUnverified> {
        OsProvenance.read_config(path)
    }
    fn config_publication(&self, path: &Path) -> Result<Option<SystemTime>, RestoreUnverified> {
        OsProvenance.config_publication(path)
    }
}

type FileSnapshot = Vec<(PathBuf, Vec<u8>, u64, SystemTime, u32)>;

/// Every home, or the first refusal among them.
fn local(boot: &BootSessionHosts, personal: &PersonalPaths) -> io::Result<Vec<ManagedHome>> {
    ManagedHome::local(boot, personal)
        .into_iter()
        .map(|target| target.home)
        .collect()
}

#[test]
fn relative_personal_boundaries_cannot_authorize_a_managed_home() {
    let f = Fixture::new();
    let private = f.personal.home.join(".config/herdr");
    fs::create_dir_all(&private).unwrap();
    fs::write(private.join("config.toml"), b"personal").unwrap();
    let boot = f.boot(Some("local"), &[(&private, "local", "/tmp/isolated.sock")]);
    let relative_home = PersonalPaths {
        home: ".".into(),
        xdg: None,
    };
    assert!(local(&boot, &relative_home).is_err());
    let boot = f.boot(Some("local"), &[(&f.home, "local", "/tmp/isolated.sock")]);
    let relative_xdg = PersonalPaths {
        home: f.personal.home.clone(),
        xdg: Some(".".into()),
    };
    assert!(local(&boot, &relative_xdg).is_err());
    assert_eq!(fs::read(private.join("config.toml")).unwrap(), b"personal");
}

fn snapshot(home: &Path) -> FileSnapshot {
    let mut files: Vec<_> = fs::read_dir(home)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let meta = fs::metadata(&path).unwrap();
            (
                path.clone(),
                fs::read(path).unwrap(),
                meta.ino(),
                meta.modified().unwrap(),
                meta.mode(),
            )
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

#[test]
fn different_config_is_canonical_and_full_original_is_backed_up() {
    let f = Fixture::new();
    let original = b"[session]\nresume_agents_on_restore = true\n".repeat(200);
    let config = f.config(&original);
    let result = f.target().ensure().unwrap();
    assert!(
        fs::read(&config).unwrap() == CANONICAL_CONFIG,
        "writer left the unsafe config"
    );
    let EnsureResult::Prepared {
        backup: Some(backup),
    } = result
    else {
        panic!("changed config needs a backup: {result:?}");
    };
    assert_eq!(fs::read(backup).unwrap(), original);
}

#[test]
fn missing_config_is_prepared_without_backup() {
    let f = Fixture::new();
    let result = f.target().ensure().unwrap();
    assert_eq!(
        fs::read(f.home.join("config.toml")).unwrap(),
        CANONICAL_CONFIG
    );
    assert_eq!(result, EnsureResult::Prepared { backup: None });
}

#[test]
fn canonical_without_record_is_a_complete_noop_and_stays_off() {
    let f = Fixture::new();
    f.config(CANONICAL_CONFIG);
    let before = snapshot(&f.home);
    assert!(matches!(f.reading(), RestoreResume::Off { .. }));
    for _ in 0..3 {
        assert_eq!(f.target().ensure().unwrap(), EnsureResult::Unchanged);
    }
    assert_eq!(snapshot(&f.home), before);
    assert!(matches!(f.reading(), RestoreResume::Off { .. }));
}

#[test]
fn canonical_with_committed_record_preserves_config_and_barrier() {
    let f = Fixture::new();
    f.config(CANONICAL_CONFIG);
    f.state("v1 committed 900 0\n");
    let before = snapshot(&f.home);
    for _ in 0..3 {
        assert_eq!(f.target().ensure().unwrap(), EnsureResult::Unchanged);
    }
    assert_eq!(snapshot(&f.home), before);
    assert!(matches!(f.reading(), RestoreResume::Off { .. }));
}

#[test]
fn old_tmp_mtime_never_proves_a_prepublication_server_off() {
    let f = Fixture::new();
    f.config(b"old unsafe config");
    let tmp = f.home.join("stalled-tmp");
    fs::write(&tmp, CANONICAL_CONFIG).unwrap();
    fs::File::open(&tmp).unwrap().set_modified(at(900)).unwrap();
    f.state("v1 preparing\n");
    fs::rename(tmp, f.home.join("config.toml")).unwrap();
    assert_eq!(
        f.reading(),
        RestoreResume::Unverified(RestoreUnverified::ConfigChanged)
    );
    f.state("v1 committed 1200 0\n");
    assert_eq!(
        f.reading(),
        RestoreResume::Unverified(RestoreUnverified::ConfigChangedSinceStart)
    );
}

#[test]
fn remote_and_missing_node_do_not_even_resolve_a_home() {
    let f = Fixture::new();
    let unreachable = f.home.join("does-not-exist/child");
    for (node, owner) in [(None, "local"), (Some("local"), "remote")] {
        let boot = f.boot(node, &[(&unreachable, owner, "/tmp/isolated.sock")]);
        assert!(local(&boot, &f.personal).unwrap().is_empty());
        assert!(!unreachable.parent().unwrap().exists());
    }
    assert!(fs::read_dir(&f.home).unwrap().next().is_none());
}

#[test]
fn personal_and_binary_paths_are_rejected_before_any_file_changes() {
    let f = Fixture::new();
    let private = f.personal.home.join(".config/herdr");
    fs::create_dir_all(&private).unwrap();
    fs::write(private.join("config.toml"), b"personal config").unwrap();
    let bin = f.personal.home.join(".local/bin/herdr");
    fs::create_dir_all(bin.parent().unwrap()).unwrap();
    fs::write(&bin, b"personal binary").unwrap();
    let before = snapshot(&private);
    for path in [&private, &bin, Path::new("/opt/homebrew/bin/herdr")] {
        let boot = f.boot(Some("local"), &[(path, "local", "/tmp/isolated.sock")]);
        let result = local(&boot, &f.personal);
        assert!(result.is_err(), "private target was admitted: {path:?}");
    }
    assert_eq!(snapshot(&private), before);
    assert_eq!(fs::read(bin).unwrap(), b"personal binary");
}

/// A personal herdr directory that is itself a link to the managed home is that home.
#[test]
fn a_personal_herdr_link_to_the_managed_home_refuses_it() {
    let f = Fixture::new();
    f.config(b"[session]\nresume_agents_on_restore = true\n");
    let config = f.personal.home.join(".config");
    fs::create_dir_all(&config).unwrap();
    std::os::unix::fs::symlink(&f.home, config.join("herdr")).unwrap();
    let before = snapshot(&f.home);
    let boot = f.boot(Some("local"), &[(&f.home, "local", "/tmp/isolated.sock")]);
    let admitted =
        local(&boot, &f.personal).map(|homes| homes.iter().map(ManagedHome::ensure).count());
    assert!(
        admitted.is_err(),
        "the aliased home was admitted: {admitted:?}"
    );
    assert_eq!(snapshot(&f.home), before);
}

#[test]
fn same_boot_home_with_two_sockets_is_rejected_before_writing() {
    let f = Fixture::new();
    f.config(b"unsafe");
    let before = snapshot(&f.home);
    let boot = f.boot(
        Some("local"),
        &[
            (&f.home, "local", "/tmp/one.sock"),
            (&f.home, "local", "/tmp/two.sock"),
        ],
    );
    assert!(local(&boot, &f.personal).is_err());
    assert_eq!(snapshot(&f.home), before);
}

#[test]
fn malformed_and_unreadable_records_never_fall_back_to_legacy() {
    let f = Fixture::new();
    f.config(CANONICAL_CONFIG);
    for text in [
        "",
        "v2 preparing\n",
        "v1 committed 900\n",
        "v1 committed 900 1000000000\n",
        "v1 committed 18446744073709551615 0\n",
        "v1 committed 900 0 extra\n",
    ] {
        f.state(text);
        let before = snapshot(&f.home);
        assert_eq!(
            f.reading(),
            RestoreResume::Unverified(RestoreUnverified::ConfigUnreadable)
        );
        assert!(f.target().ensure().is_err());
        assert_eq!(snapshot(&f.home), before);
    }
}

#[test]
fn preparing_recovery_preserves_canonical_mtime_and_rejects_the_running_server() {
    let f = Fixture::new();
    let config = f.config(CANONICAL_CONFIG);
    f.state("v1 preparing\n");
    let before = fs::metadata(&config).unwrap();
    assert_eq!(f.target().ensure().unwrap(), EnsureResult::Recovered);
    let after = fs::metadata(&config).unwrap();
    assert_eq!(
        (before.ino(), before.modified().unwrap()),
        (after.ino(), after.modified().unwrap())
    );
    assert_eq!(
        f.reading(),
        RestoreResume::Unverified(RestoreUnverified::ConfigChangedSinceStart)
    );
    assert_eq!(f.target().ensure().unwrap(), EnsureResult::Unchanged);
}

#[test]
fn publication_margin_is_strict_at_one_second() {
    let f = Fixture::new();
    f.config(CANONICAL_CONFIG);
    for (state, off) in [
        ("v1 committed 998 999999999\n", true),
        ("v1 committed 999 0\n", false),
        ("v1 committed 999 1\n", false),
    ] {
        f.state(state);
        assert_eq!(
            matches!(f.reading(), RestoreResume::Off { .. }),
            off,
            "{state}"
        );
    }
}

#[test]
fn symlinks_hardlinks_and_writable_homes_cannot_be_repaired() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let f = Fixture::new();
    let config = f.config(b"private bytes");
    let alias = f.home.with_file_name("alias");
    symlink(&f.home, &alias).unwrap();
    let boot = f.boot(Some("local"), &[(&alias, "local", "/tmp/isolated.sock")]);
    assert!(local(&boot, &f.personal).is_err());
    let linked = f.home.with_file_name("linked");
    fs::hard_link(&config, &linked).unwrap();
    assert!(f.target().ensure().is_err());
    assert_eq!(fs::read(&linked).unwrap(), b"private bytes");
    fs::remove_file(&linked).unwrap();
    fs::remove_file(&config).unwrap();
    symlink(&linked, &config).unwrap();
    assert!(f.target().ensure().is_err());
    assert!(!linked.exists());
    fs::set_permissions(&f.home, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(
        local(
            &f.boot(Some("local"), &[(&f.home, "local", "/tmp/isolated.sock")]),
            &f.personal
        )
        .is_err()
    );
}

#[test]
fn custom_xdg_and_parent_escape_are_rejected_and_duplicate_targets_are_folded() {
    let mut f = Fixture::new();
    f.personal.xdg = Some(f._root.path().join("custom-xdg"));
    fs::create_dir(f.personal.xdg.as_ref().unwrap()).unwrap();
    let xdg = f.personal.xdg.as_ref().unwrap().join("herdr");
    let escape = f.home.join("../user/.config/herdr");
    for path in [&xdg, &escape] {
        let boot = f.boot(Some("local"), &[(path, "local", "/tmp/isolated.sock")]);
        assert!(local(&boot, &f.personal).is_err());
    }
    let boot = f.boot(
        Some("local"),
        &[
            (&f.home, "local", "/tmp/one.sock"),
            (&f.home, "local", "/tmp/one.sock"),
        ],
    );
    assert_eq!(local(&boot, &f.personal).unwrap().len(), 1);
}

#[test]
fn preparing_is_durable_before_replacement_and_failed_preparation_keeps_original() {
    let f = Fixture::new();
    f.config(b"original");
    let result = f.target().ensure_with(|step| {
        if step == Step::Preparing {
            assert_eq!(
                publication::state(&f.home.join("config.toml")).unwrap(),
                Some(publication::State::Preparing)
            );
            assert_eq!(fs::read(f.home.join("config.toml")).unwrap(), b"original");
            return Err(io::Error::other("stop after durable Preparing"));
        }
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(fs::read(f.home.join("config.toml")).unwrap(), b"original");
}

#[test]
fn committed_is_not_published_before_config_sync_and_backup() {
    let f = Fixture::new();
    f.config(b"original");
    f.target()
        .ensure_with(|step| {
            if matches!(
                step,
                Step::Backup
                    | Step::BeforeRename
                    | Step::Renamed
                    | Step::ConfigSynced
                    | Step::BeforeCommitted
            ) {
                assert_eq!(
                    publication::state(&f.home.join("config.toml")).unwrap(),
                    Some(publication::State::Preparing),
                    "{step:?}"
                );
            }
            if step == Step::BeforeRename {
                let backups: Vec<_> = fs::read_dir(&f.home)
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .starts_with("config.toml.bak.")
                    })
                    .collect();
                assert_eq!(backups.len(), 1, "original must be saved before rename");
                assert_eq!(fs::read(backups[0].path()).unwrap(), b"original");
            }
            Ok(())
        })
        .unwrap();
}

const CHILD: &str = "services::session_host::herdr::managed_config::tests::writer_child";

fn child(f: &Fixture, mode: &str) -> std::process::Command {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CHILD, "--nocapture"])
        .env("ADK_CONFIG_TEST_HOME", &f.home)
        .env("ADK_CONFIG_TEST_USER", &f.personal.home)
        .env("ADK_CONFIG_TEST_MODE", mode)
        // The child's own libtest lines stay out of this run's output.
        .stdout(std::process::Stdio::piped());
    command
}

fn wait_file(path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "no marker: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn writer_child() {
    let Some(home) = std::env::var_os("ADK_CONFIG_TEST_HOME") else {
        return;
    };
    let user = PathBuf::from(std::env::var_os("ADK_CONFIG_TEST_USER").unwrap());
    let mode = std::env::var("ADK_CONFIG_TEST_MODE").unwrap();
    let mut config = crate::config::Config::default();
    config.cluster.instance_id = Some("local".into());
    config.session_hosts.herdr.endpoints.insert(
        "local".into(),
        EndpointConfig {
            mode: EndpointMode::LegacyDedicated,
            execution_node: "local".into(),
            socket_path: "/tmp/isolated.sock".into(),
            herdr_home: PathBuf::from(home),
            herdr_session: "adk".into(),
        },
    );
    let boot = BootSessionHosts::from_config(&config).unwrap();
    let home = local(
        &boot,
        &PersonalPaths {
            home: user,
            xdg: None,
        },
    )
    .unwrap()
    .remove(0);
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let result = home.ensure_with(|step| {
            if mode == format!("{step:?}") {
                std::process::exit(73);
            }
            if mode == "hold" && step == Step::Preparing {
                let ready = PathBuf::from(std::env::var_os("ADK_CONFIG_TEST_READY").unwrap());
                fs::write(ready, b"ready").unwrap();
                wait_file(Path::new(
                    &std::env::var_os("ADK_CONFIG_TEST_RELEASE").unwrap(),
                ));
            }
            Ok(())
        });
        match result {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if let Some(path) = std::env::var_os("ADK_CONFIG_TEST_BUSY") {
                    fs::write(path, b"busy").unwrap();
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            result => {
                let result = result.unwrap();
                if let Some(path) = std::env::var_os("ADK_CONFIG_TEST_RESULT") {
                    fs::write(path, format!("{result:?}")).unwrap();
                }
                break;
            }
        }
    }
}

#[test]
fn process_exit_at_every_publish_boundary_preserves_original_and_fails_closed() {
    for phase in [
        "BeforePreparing",
        "Preparing",
        "Backup",
        "BeforeRename",
        "Renamed",
        "ConfigSynced",
        "BeforeCommitted",
        "Committed",
    ] {
        let f = Fixture::new();
        let config = f.config(b"original unsafe config");
        let output = child(&f, phase).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(73),
            "phase {phase}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!matches!(f.reading(), RestoreResume::Off { .. }), "{phase}");
        let bytes = fs::read(&config).unwrap();
        if bytes == CANONICAL_CONFIG {
            assert!(
                fs::read_dir(&f.home)
                    .unwrap()
                    .filter_map(Result::ok)
                    .any(|e| e
                        .file_name()
                        .to_string_lossy()
                        .starts_with("config.toml.bak.")
                        && fs::read(e.path()).unwrap() == b"original unsafe config"),
                "no original backup at {phase}"
            );
            let meta = fs::metadata(&config).unwrap();
            f.target().ensure().unwrap();
            let after = fs::metadata(&config).unwrap();
            assert_eq!(
                (meta.ino(), meta.modified().unwrap()),
                (after.ino(), after.modified().unwrap())
            );
        } else {
            assert_eq!(bytes, b"original unsafe config", "{phase}");
            f.target().ensure().unwrap();
        }
        assert_eq!(fs::read(&config).unwrap(), CANONICAL_CONFIG);
        assert!(!matches!(f.reading(), RestoreResume::Off { .. }), "{phase}");
    }
}

#[test]
fn two_process_writers_replace_once_and_the_waiting_writer_is_a_noop() {
    let f = Fixture::new();
    f.config(b"one original");
    let ready = f._root.path().join("ready");
    let release = f._root.path().join("release");
    let busy = f._root.path().join("busy");
    let result_a = f._root.path().join("a");
    let result_b = f._root.path().join("b");
    let a = child(&f, "hold")
        .env("ADK_CONFIG_TEST_READY", &ready)
        .env("ADK_CONFIG_TEST_RELEASE", &release)
        .env("ADK_CONFIG_TEST_RESULT", &result_a)
        .spawn()
        .unwrap();
    wait_file(&ready);
    let b = child(&f, "write")
        .env("ADK_CONFIG_TEST_BUSY", &busy)
        .env("ADK_CONFIG_TEST_RESULT", &result_b)
        .spawn()
        .unwrap();
    wait_file(&busy);
    fs::write(release, b"go").unwrap();
    assert!(a.wait_with_output().unwrap().status.success());
    assert!(b.wait_with_output().unwrap().status.success());
    assert!(
        fs::read_to_string(result_a)
            .unwrap()
            .starts_with("Prepared")
    );
    assert_eq!(fs::read_to_string(result_b).unwrap(), "Unchanged");
    let backups: Vec<_> = fs::read_dir(&f.home)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("config.toml.bak.")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read(backups[0].path()).unwrap(), b"one original");
}

struct PausingProof {
    inner: FileProof,
    read: std::sync::mpsc::Sender<()>,
    resume: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
impl ServerProvenance for PausingProof {
    fn process_start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        self.inner.process_start(pid)
    }
    fn process_env(&self, pid: u32, key: &str) -> Result<Vec<String>, RestoreUnverified> {
        self.inner.process_env(pid, key)
    }
    fn read_config(&self, path: &Path) -> Result<ConfigRead, RestoreUnverified> {
        let config = self.inner.read_config(path)?;
        self.read.send(()).unwrap();
        self.resume
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(15))
            .unwrap();
        Ok(config)
    }
    fn config_publication(&self, path: &Path) -> Result<Option<SystemTime>, RestoreUnverified> {
        self.inner.config_publication(path)
    }
}

#[test]
fn reader_paused_after_config_reads_the_newer_publication_veto() {
    let f = Fixture::new();
    f.config(CANONICAL_CONFIG);
    let (read_tx, read_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let proof = PausingProof {
        inner: FileProof {
            home: f.home.clone(),
        },
        read: read_tx,
        resume: std::sync::Mutex::new(resume_rx),
    };
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| read_restore_resume_with(&Peer, &endpoint(&f.home), &proof));
        read_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        fs::write(f.home.join("config.toml"), b"external drift").unwrap();
        f.target().ensure().unwrap();
        resume_tx.send(()).unwrap();
        assert_eq!(
            reader.join().unwrap(),
            RestoreResume::Unverified(RestoreUnverified::ConfigChangedSinceStart)
        );
    });
}

#[test]
fn read_only_canonical_config_is_not_chmodded_or_replaced() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let path = f.config(CANONICAL_CONFIG);
    fs::set_permissions(path, fs::Permissions::from_mode(0o444)).unwrap();
    let before = snapshot(&f.home);
    assert_eq!(f.target().ensure().unwrap(), EnsureResult::Unchanged);
    assert_eq!(snapshot(&f.home), before);
}

#[test]
fn record_and_lock_aliases_do_not_authorize_reads_or_replacements() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let config = f.config(CANONICAL_CONFIG);
    let record = f.home.join(publication::STATE_FILE);
    fs::create_dir(&record).unwrap();
    assert_eq!(
        f.reading(),
        RestoreResume::Unverified(RestoreUnverified::ConfigUnreadable)
    );
    assert!(f.target().ensure().is_err());
    fs::remove_dir(&record).unwrap();
    let private = f._root.path().join("private-record");
    fs::write(&private, b"v1 committed 900 0\n").unwrap();
    symlink(&private, &record).unwrap();
    assert_eq!(
        f.reading(),
        RestoreResume::Unverified(RestoreUnverified::ConfigUnreadable)
    );
    assert!(f.target().ensure().is_err());
    assert_eq!(fs::read(&private).unwrap(), b"v1 committed 900 0\n");
    fs::remove_file(&record).unwrap();
    fs::write(&config, b"unsafe").unwrap();
    symlink(&private, f.home.join(".agentdesk-config.lock")).unwrap();
    assert!(f.target().ensure().is_err());
    assert_eq!(fs::read(config).unwrap(), b"unsafe");
    assert_eq!(fs::read(private).unwrap(), b"v1 committed 900 0\n");
}
