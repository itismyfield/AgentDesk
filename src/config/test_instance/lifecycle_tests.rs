use super::{admit_dcserver, legacy_cleanup_if_unscoped};
use crate::config::test_instance::{self as instance, tests as fixture};
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const MODE: &str = "I3A_LEASE_TEST_MODE";
const LIMIT: Duration = Duration::from_secs(10);

struct LeaseFixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    control: PathBuf,
    donor: Option<File>,
    witness: Value,
}

impl LeaseFixture {
    fn new(locked: bool) -> Self {
        let (temporary, root) = fixture::fixture();
        let parent = root.parent().unwrap();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(std::fs::metadata(parent).unwrap().mode() & 0o7777, 0o700);
        let control = parent.join(".gateway.lease");
        let donor = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&control)
            .unwrap();
        if locked {
            assert_eq!(
                unsafe { libc::flock(donor.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
        }
        let metadata = donor.metadata().unwrap();
        let witness = json!({"schema_version":1,"fd":donor.as_raw_fd(),"device":metadata.dev(),
            "inode":metadata.ino(),"instance_id":root.file_name().unwrap().to_str().unwrap(),
            "root":root,"nonce":uuid::Uuid::new_v4().to_string()});
        let mut result = Self {
            _temporary: temporary,
            root,
            control,
            donor: Some(donor),
            witness,
        };
        result.write_record();
        std::fs::write(
            result.root.join("foreign-sentinel"),
            b"untouched foreign sentinel\n",
        )
        .unwrap();
        result
    }

    fn write_record(&mut self) {
        let donor = self.donor.as_mut().unwrap();
        donor.set_len(0).unwrap();
        donor.seek(SeekFrom::Start(0)).unwrap();
        donor
            .write_all(&serde_json::to_vec(&self.witness).unwrap())
            .unwrap();
        donor.sync_all().unwrap();
        donor.seek(SeekFrom::Start(7)).unwrap();
    }

    fn spawn(&self, name: &str, mode: &str, witness: Option<&Value>, inherit: bool) -> Child {
        let mut command = fixture::command(&self.root);
        command
            .args(["--exact", selector(name), "--nocapture"])
            .env(MODE, mode)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(witness) = witness {
            command.env("AGENTDESK_TEST_LEASE", witness.to_string());
        }
        if inherit {
            let fd = self.donor.as_ref().unwrap().as_raw_fd();
            // Only this known donor descriptor crosses the supervisor-to-owner exec.
            unsafe {
                command.pre_exec(move || {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        command.spawn().unwrap()
    }

    fn assert_sentinel(&self) {
        assert_eq!(
            std::fs::read(self.root.join("foreign-sentinel")).unwrap(),
            b"untouched foreign sentinel\n"
        );
    }
}

fn selector(name: &str) -> &str {
    name.strip_prefix("agentdesk::").unwrap_or(name)
}

fn initialized_scope() -> crate::config::test_env_lock::SharedTestEnvLockGuard {
    let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    instance::initialize().unwrap();
    lock
}

fn finish(child: Child) -> Output {
    crate::services::process::wait_with_output_timeout(child, LIMIT, "instance lease test child")
        .unwrap()
}

fn wait_file(path: &Path) -> bool {
    let deadline = Instant::now() + LIMIT;
    while Instant::now() < deadline {
        if path.is_file() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn wait_ready(child: &mut Child, path: &Path) -> bool {
    let deadline = Instant::now() + LIMIT;
    while Instant::now() < deadline {
        if path.is_file() {
            return true;
        }
        if child.try_wait().unwrap().is_some() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn independent_lock(path: &Path, busy: bool) -> File {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if busy {
        assert_ne!(
            result, 0,
            "independent descriptor unexpectedly acquired the lease"
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
    } else {
        assert_eq!(result, 0, "lease remained held after its last owner exited");
    }
    file
}

struct ReleaseOnDrop(PathBuf);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"release");
    }
}

#[test]
fn lease_admission_holds_the_inherited_ofd_and_closes_without_unlocking() {
    let name = concat!(
        module_path!(),
        "::lease_admission_holds_the_inherited_ofd_and_closes_without_unlocking"
    );
    if std::env::var_os(MODE).is_some() {
        let _env = initialized_scope();
        let witness: Value =
            serde_json::from_str(&std::env::var("AGENTDESK_TEST_LEASE").unwrap()).unwrap();
        let fd = witness["fd"].as_i64().unwrap() as i32;
        let held = admit_dcserver()
            .unwrap()
            .expect("a real inherited lease must be admitted");
        assert!(std::env::var_os("AGENTDESK_TEST_LEASE").is_none());
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) }, 7);
        let root = &instance::current().unwrap().unwrap().root;
        std::fs::write(root.join("owner.ready"), b"ready").unwrap();
        assert!(wait_file(&root.join("owner.release")));
        drop(held);
        return;
    }
    for keep_parent in [true, false] {
        let mut fixture = LeaseFixture::new(true);
        let _release = ReleaseOnDrop(fixture.root.join("owner.release"));
        let mut child = fixture.spawn(name, "owner", Some(&fixture.witness), true);
        if !wait_ready(&mut child, &fixture.root.join("owner.ready")) {
            fixture::assert_single_test_passed(&finish(child), selector(name));
            panic!("owner never became ready");
        }
        if !keep_parent {
            drop(fixture.donor.take());
        }
        drop(independent_lock(&fixture.control, true));
        std::fs::write(fixture.root.join("owner.release"), b"release").unwrap();
        fixture::assert_single_test_passed(&finish(child), selector(name));
        if keep_parent {
            drop(independent_lock(&fixture.control, true));
            assert_eq!(
                fixture.donor.as_mut().unwrap().stream_position().unwrap(),
                7
            );
            drop(fixture.donor.take());
        }
        drop(independent_lock(&fixture.control, false));
        fixture.assert_sentinel();
    }
}

#[test]
fn lease_admission_rejects_missing_unlocked_wrong_ofd_and_unsafe_fds() {
    let name = concat!(
        module_path!(),
        "::lease_admission_rejects_missing_unlocked_wrong_ofd_and_unsafe_fds"
    );
    if std::env::var_os(MODE).is_some() {
        let _env = initialized_scope();
        assert!(admit_dcserver().is_err());
        for fd in 0..=2 {
            assert!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0);
        }
        return;
    }
    for case in [
        "missing",
        "unlocked",
        "wrong-ofd",
        "low-fd",
        "closed-fd",
        "foreign-fd",
    ] {
        let mut fixture = LeaseFixture::new(case != "unlocked");
        let mut holder = None;
        if matches!(case, "wrong-ofd" | "foreign-fd") {
            holder = fixture.donor.take();
            let path = if case == "foreign-fd" {
                fixture.root.join("foreign-lease")
            } else {
                fixture.control.clone()
            };
            let donor = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .mode(0o600)
                .open(path)
                .unwrap();
            let metadata = donor.metadata().unwrap();
            fixture.witness["fd"] = json!(donor.as_raw_fd());
            fixture.witness["device"] = json!(metadata.dev());
            fixture.witness["inode"] = json!(metadata.ino());
            fixture.donor = Some(donor);
            fixture.write_record();
        }
        let mut witness = fixture.witness.clone();
        if case == "low-fd" {
            witness["fd"] = json!(2);
        }
        let before = std::fs::read(&fixture.control).unwrap();
        let foreign = fixture.root.join("foreign-lease");
        let foreign_before = foreign.is_file().then(|| std::fs::read(&foreign).unwrap());
        let child = fixture.spawn(
            name,
            case,
            (case != "missing").then_some(&witness),
            case != "closed-fd",
        );
        fixture::assert_single_test_passed(&finish(child), selector(name));
        assert_eq!(std::fs::read(&fixture.control).unwrap(), before, "{case}");
        if let Some(before) = foreign_before {
            assert_eq!(std::fs::read(foreign).unwrap(), before, "{case}");
        }
        fixture.assert_sentinel();
        drop(holder);
    }
}

#[test]
fn lease_admission_binds_nonce_identity_and_safe_fixed_path() {
    let name = concat!(
        module_path!(),
        "::lease_admission_binds_nonce_identity_and_safe_fixed_path"
    );
    if std::env::var_os(MODE).is_some() {
        let _env = initialized_scope();
        assert!(admit_dcserver().is_err());
        return;
    }
    for case in [
        "foreign-nonce",
        "nil-nonce",
        "malformed-nonce",
        "foreign-root",
        "foreign-instance",
        "wrong-device",
        "wrong-inode",
        "unknown-field",
        "record-nonce",
        "oversized-record",
        "symlink",
        "hardlink",
        "replaced-inode",
        "unsafe-parent",
        "unsafe-file",
    ] {
        let mut fixture = LeaseFixture::new(true);
        let mut witness = fixture.witness.clone();
        match case {
            "foreign-nonce" => witness["nonce"] = json!(uuid::Uuid::new_v4().to_string()),
            "nil-nonce" => witness["nonce"] = json!(uuid::Uuid::nil().to_string()),
            "malformed-nonce" => witness["nonce"] = json!("not-a-nonce"),
            "foreign-root" => witness["root"] = json!(fixture.root.join("foreign-root")),
            "foreign-instance" => witness["instance_id"] = json!(uuid::Uuid::new_v4().to_string()),
            "wrong-device" => witness["device"] = json!(witness["device"].as_u64().unwrap() + 1),
            "wrong-inode" => witness["inode"] = json!(witness["inode"].as_u64().unwrap() + 1),
            "unknown-field" => witness["foreign"] = json!(true),
            "record-nonce" => {
                fixture.witness["nonce"] = json!(uuid::Uuid::new_v4().to_string());
                fixture.write_record();
            }
            "oversized-record" => {
                fixture.donor.as_mut().unwrap().set_len(5000).unwrap();
            }
            "symlink" => {
                let target = fixture.root.join("foreign-lease");
                std::fs::rename(&fixture.control, &target).unwrap();
                std::os::unix::fs::symlink(target, &fixture.control).unwrap();
            }
            "hardlink" => {
                std::fs::hard_link(&fixture.control, fixture.root.join("foreign-lease")).unwrap()
            }
            "replaced-inode" => {
                std::fs::rename(&fixture.control, fixture.root.join("foreign-lease")).unwrap();
                std::fs::write(
                    &fixture.control,
                    serde_json::to_vec(&fixture.witness).unwrap(),
                )
                .unwrap();
                std::fs::set_permissions(&fixture.control, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
            "unsafe-parent" => std::fs::set_permissions(
                fixture.root.parent().unwrap(),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap(),
            "unsafe-file" => {
                std::fs::set_permissions(&fixture.control, std::fs::Permissions::from_mode(0o644))
                    .unwrap()
            }
            _ => unreachable!(),
        }
        let before = std::fs::read(&fixture.control).unwrap();
        let foreign = fixture.root.join("foreign-lease");
        let foreign_before = foreign.is_file().then(|| std::fs::read(&foreign).unwrap());
        let child = fixture.spawn(name, case, Some(&witness), true);
        fixture::assert_single_test_passed(&finish(child), selector(name));
        assert_eq!(std::fs::read(&fixture.control).unwrap(), before, "{case}");
        if let Some(before) = foreign_before {
            assert_eq!(std::fs::read(foreign).unwrap(), before, "{case}");
        }
        fixture.assert_sentinel();
    }
}

#[test]
fn lease_admission_provider_exec_outlives_owner_without_holding_lease() {
    let name = concat!(
        module_path!(),
        "::lease_admission_provider_exec_outlives_owner_without_holding_lease"
    );
    let mode = std::env::var(MODE).ok();
    if mode.as_deref() == Some("provider") {
        let root = PathBuf::from(std::env::var("I3A_LEASE_ROOT").unwrap());
        let witness: Value =
            serde_json::from_str(&std::env::var("I3A_EXPECT_WITNESS").unwrap()).unwrap();
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        let result = unsafe {
            libc::fstat(
                witness["fd"].as_i64().unwrap() as i32,
                metadata.as_mut_ptr(),
            )
        };
        if result == 0 {
            let metadata = unsafe { metadata.assume_init() };
            assert!(
                i128::from(metadata.st_dev) != i128::from(witness["device"].as_u64().unwrap())
                    || i128::from(metadata.st_ino)
                        != i128::from(witness["inode"].as_u64().unwrap())
            );
        }
        std::fs::write(root.join("provider.ready"), b"ready").unwrap();
        assert!(wait_file(&root.join("provider.release")));
        std::fs::write(root.join("provider.done"), b"done").unwrap();
        return;
    }
    if mode.as_deref() == Some("owner") {
        let _env = initialized_scope();
        let encoded = std::env::var("AGENTDESK_TEST_LEASE").unwrap();
        let _held = admit_dcserver()
            .unwrap()
            .expect("owner must hold the inherited lease");
        assert!(std::env::var_os("AGENTDESK_TEST_LEASE").is_none());
        let root = &instance::current().unwrap().unwrap().root;
        let mut provider = Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .args(["--exact", selector(name), "--nocapture"])
            .env(MODE, "provider")
            .env("I3A_LEASE_ROOT", root)
            .env("I3A_EXPECT_WITNESS", encoded)
            .stdin(Stdio::null())
            .stdout(File::create(root.join("provider.stdout")).unwrap())
            .stderr(File::create(root.join("provider.stderr")).unwrap())
            .spawn()
            .unwrap();
        std::fs::write(root.join("provider.pid"), provider.id().to_string()).unwrap();
        if !wait_ready(&mut provider, &root.join("provider.ready")) {
            let _ = std::fs::write(root.join("provider.release"), b"release");
            let _ = finish(provider);
            panic!(
                "provider exec never became ready: {}",
                std::fs::read_to_string(root.join("provider.stderr")).unwrap()
            );
        }
        std::fs::write(root.join("owner.ready"), b"ready").unwrap();
        // The synthetic provider intentionally survives this owner's normal process exit.
        return;
    }
    let mut fixture = LeaseFixture::new(true);
    let _release = ReleaseOnDrop(fixture.root.join("provider.release"));
    let mut owner = fixture.spawn(name, "owner", Some(&fixture.witness), true);
    if !wait_ready(&mut owner, &fixture.root.join("owner.ready")) {
        fixture::assert_single_test_passed(&finish(owner), selector(name));
        panic!("owner never became ready");
    }
    drop(fixture.donor.take());
    fixture::assert_single_test_passed(&finish(owner), selector(name));
    let pid: i32 = std::fs::read_to_string(fixture.root.join("provider.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "owned provider must outlive its owner"
    );
    drop(independent_lock(&fixture.control, false));
    std::fs::write(fixture.root.join("provider.release"), b"release").unwrap();
    let deadline = Instant::now() + LIMIT;
    while Instant::now() < deadline {
        if unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(unsafe { libc::kill(pid, 0) } != 0);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    assert!(fixture.root.join("provider.done").is_file());
    let stdout = std::fs::read_to_string(fixture.root.join("provider.stdout")).unwrap();
    assert!(
        stdout
            .lines()
            .any(|line| line == format!("test {} ... ok", selector(name)))
    );
    assert_eq!(
        stdout
            .lines()
            .filter(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"))
            .count(),
        1
    );
    fixture.assert_sentinel();
}

#[test]
fn lease_admission_requires_initialized_scope_and_preserves_normal_mode() {
    let name = concat!(
        module_path!(),
        "::lease_admission_requires_initialized_scope_and_preserves_normal_mode"
    );
    match std::env::var(MODE).as_deref() {
        Ok("uninitialized") => {
            assert!(admit_dcserver().is_err());
            return;
        }
        Ok("normal") => {
            let _env = initialized_scope();
            assert!(admit_dcserver().unwrap().is_none());
            return;
        }
        _ => {}
    }
    let fixture = LeaseFixture::new(true);
    fixture::assert_single_test_passed(
        &finish(fixture.spawn(name, "uninitialized", Some(&fixture.witness), true)),
        selector(name),
    );
    let child = Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .args(["--exact", selector(name), "--nocapture"])
        .env(MODE, "normal")
        .env(
            "AGENTDESK_TEST_LEASE",
            "malformed witness must be ignored outside test mode",
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    fixture::assert_single_test_passed(&finish(child), selector(name));
    fixture.assert_sentinel();
}

#[test]
fn dcserver_rejects_missing_lease_before_first_runtime_effect() {
    let name = concat!(
        module_path!(),
        "::dcserver_rejects_missing_lease_before_first_runtime_effect"
    );
    if std::env::var_os(MODE).is_some() {
        let _env = initialized_scope();
        let root = &instance::current().unwrap().unwrap().root;
        std::fs::remove_dir_all(root.join("runtime")).unwrap();
        let limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        crate::cli::dcserver::handle_dcserver(None);
        panic!("missing lease must refuse the real dcserver entry point");
    }
    let fixture = LeaseFixture::new(true);
    let output = finish(fixture.spawn(name, "real-entry", None, false));
    assert!(
        !fixture.root.join("runtime").exists(),
        "dcserver performed its first runtime directory effect"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("test instance lease admission rejected")
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("test {} ...", selector(name)))
    );
    fixture.assert_sentinel();
}

#[test]
fn legacy_cleanup_runs_only_without_requested_instance() {
    let name = concat!(
        module_path!(),
        "::legacy_cleanup_runs_only_without_requested_instance"
    );
    if let Ok(mode) = std::env::var(MODE) {
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        if mode == "invalid" {
            assert!(instance::requested());
            assert!(instance::current().is_err());
        } else {
            instance::initialize().unwrap();
        }
        let root = PathBuf::from(std::env::var("I3A_LEASE_ROOT").unwrap());
        let mut calls = 0;
        legacy_cleanup_if_unscoped(|| {
            calls += 1;
            std::fs::write(root.join("cleanup.effect"), b"fake cleanup executed").unwrap();
        });
        let expected = usize::from(mode == "normal");
        assert_eq!(calls, expected);
        assert_eq!(root.join("cleanup.effect").exists(), expected == 1);
        assert_eq!(
            std::fs::read(root.join("foreign-sentinel")).unwrap(),
            b"untouched foreign sentinel\n"
        );
        return;
    }
    for mode in ["scoped", "invalid", "normal"] {
        let fixture = LeaseFixture::new(true);
        let child = if mode == "normal" {
            Command::new(std::env::current_exe().unwrap())
                .env_clear()
                .args(["--exact", selector(name), "--nocapture"])
                .env(MODE, mode)
                .env("I3A_LEASE_ROOT", &fixture.root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        } else {
            let mut command = fixture::command(&fixture.root);
            command
                .args(["--exact", selector(name), "--nocapture"])
                .env(MODE, mode)
                .env("I3A_LEASE_ROOT", &fixture.root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        };
        fixture::assert_single_test_passed(&finish(child), selector(name));
        fixture.assert_sentinel();
    }
}
