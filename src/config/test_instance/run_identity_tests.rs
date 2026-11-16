use super::*;
use crate::config::test_instance::lifecycle::{bound_listener, health_witness};
use sha2::{Digest, Sha256};

const RUN_MODE: &str = "I3A_RUN_IDENTITY_MODE";
const NAMES: [&str; 4] = [
    "claude_bot_token",
    "codex_bot_token",
    "announce_bot_token",
    "notify_bot_token",
];

struct RunFixture {
    lease: LeaseFixture,
    binary: File,
    executable: PathBuf,
    run: Value,
}

impl RunFixture {
    fn new(copy: bool) -> Self {
        let lease = LeaseFixture::new(true);
        let current = std::env::current_exe().unwrap();
        let executable = if copy {
            let target = lease.root.join("fixture-worktree/target/debug/agentdesk");
            std::fs::copy(&current, &target).unwrap();
            target
        } else {
            current
        };
        let binary = File::open(&executable).unwrap();
        let metadata = binary.metadata().unwrap();
        static DIGEST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let digest = DIGEST
            .get_or_init(|| format!("{:x}", Sha256::digest(std::fs::read(&executable).unwrap())))
            .clone();
        let directory = lease.root.join("credential");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let identities: Vec<_> = NAMES.iter().enumerate().map(|(index, name)| {
            let path = directory.join(name);
            std::fs::write(&path, format!("synthetic-credential-{index}")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let file = std::fs::symlink_metadata(path).unwrap();
            json!({"name":name,"application_id":format!("{}",index+100),"device":file.dev(),
                "inode":file.ino(),"size":file.len(),"mtime_ns":file.mtime() as u64*1_000_000_000+file.mtime_nsec() as u64,
                "ctime_ns":file.ctime() as u64*1_000_000_000+file.ctime_nsec() as u64})
        }).collect();
        let run = json!({"schema":1,"head":"a".repeat(40),"worktree":lease.root.join("fixture-worktree"),
            "binary":{"fd":binary.as_raw_fd(),"device":metadata.dev(),"inode":metadata.ino(),"sha256":digest},
            "identities":identities});
        Self {
            lease,
            binary,
            executable,
            run,
        }
    }

    fn execute(&self, name: &str, mode: &str, run: &Value) -> Output {
        let environment = fixture::command(&self.lease.root);
        let mut command = Command::new(&self.executable);
        command
            .env_clear()
            .envs(
                environment
                    .get_envs()
                    .filter_map(|(key, value)| value.map(|value| (key, value))),
            )
            .args(["--exact", selector(name), "--nocapture"])
            .env(RUN_MODE, mode)
            .env("AGENTDESK_TEST_RUN", run.to_string())
            .env("AGENTDESK_TEST_LEASE", self.lease.witness.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut descriptors = vec![
            self.binary.as_raw_fd(),
            self.lease.donor.as_ref().unwrap().as_raw_fd(),
        ];
        if let Some(fd) = run["binary"]["fd"].as_i64().filter(|fd| *fd >= 3) {
            let fd = fd as i32;
            if !descriptors.contains(&fd) {
                descriptors.push(fd);
            }
        }
        unsafe {
            command.pre_exec(move || {
                for &fd in &descriptors {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        crate::services::process::wait_with_output_timeout(
            command.spawn().unwrap(),
            Duration::from_secs(90),
            "large native run-identity child",
        )
        .unwrap()
    }
}

fn child_mode() -> Option<String> {
    std::env::var(RUN_MODE).ok()
}

#[test]
fn run_health_uses_actual_listener_pid_and_verified_four_identities() {
    let name = concat!(
        module_path!(),
        "::run_health_uses_actual_listener_pid_and_verified_four_identities"
    );
    if child_mode().as_deref() == Some("aux") {
        let fd = std::env::var("I3A_BINARY_FD")
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        let found = unsafe { libc::fstat(fd, &mut metadata) } == 0;
        let device = std::env::var("I3A_BINARY_DEV")
            .unwrap()
            .parse::<u64>()
            .unwrap();
        let inode = std::env::var("I3A_BINARY_INO")
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(
            !found || (metadata.st_dev as u64, metadata.st_ino) != (device, inode),
            "binary FD leaked through auxiliary exec"
        );
        assert!(std::env::var_os("AGENTDESK_TEST_RUN").is_none());
        return;
    }
    if child_mode().is_some() {
        let _env = initialized_scope();
        let expected: Value =
            serde_json::from_str(&std::env::var("AGENTDESK_TEST_RUN").unwrap()).unwrap();
        let lease: Value =
            serde_json::from_str(&std::env::var("AGENTDESK_TEST_LEASE").unwrap()).unwrap();
        let binary_fd = expected["binary"]["fd"].as_i64().unwrap() as i32;
        let held = admit_dcserver().unwrap().unwrap();
        assert!(std::env::var_os("AGENTDESK_TEST_RUN").is_none());
        assert_ne!(
            unsafe { libc::fcntl(binary_fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .args(["--exact", selector(name), "--nocapture"])
            .env(RUN_MODE, "aux")
            .env("I3A_BINARY_FD", binary_fd.to_string())
            .env("I3A_BINARY_DEV", expected["binary"]["device"].to_string())
            .env("I3A_BINARY_INO", expected["binary"]["inode"].to_string())
            .output()
            .unwrap();
        fixture::assert_single_test_passed(&output, selector(name));
        assert!(
            health_witness().is_err(),
            "unbound run must not produce health"
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        bound_listener(address).unwrap();
        let observed = health_witness().unwrap().unwrap();
        assert_eq!(observed["pid"], std::process::id());
        assert_eq!(observed["port"], address.port());
        assert_eq!(observed["head"], expected["head"]);
        assert_eq!(observed["binary_sha256"], expected["binary"]["sha256"]);
        assert_eq!(observed["nonce"], lease["nonce"]);
        assert_eq!(observed["identities"], expected["identities"]);
        assert_eq!(observed["live_ready"], false);
        assert!(bound_listener("127.0.0.1:1".parse().unwrap()).is_err());
        drop(held);
        assert!(
            health_witness().is_err(),
            "released admission must not produce health"
        );
        return;
    }
    let fixture = RunFixture::new(false);
    fixture::assert_single_test_passed(
        &fixture.execute(name, "positive", &fixture.run),
        selector(name),
    );
}

#[test]
fn run_admission_rejects_foreign_old_duplicate_and_unsupported_inputs() {
    let name = concat!(
        module_path!(),
        "::run_admission_rejects_foreign_old_duplicate_and_unsupported_inputs"
    );
    if child_mode().is_some() {
        let _env = initialized_scope();
        assert!(
            admit_dcserver().is_err(),
            "invalid run provenance was admitted"
        );
        return;
    }
    let fixture = RunFixture::new(false);
    for case in [
        "head",
        "worktree",
        "sha",
        "inode",
        "fd",
        "duplicate",
        "name",
        "zero-id",
        "unknown",
        "writable",
    ] {
        let mut run = fixture.run.clone();
        let writable;
        match case {
            "head" => run["head"] = json!("b".repeat(40)),
            "worktree" => run["worktree"] = json!(fixture.lease.root),
            "sha" => run["binary"]["sha256"] = json!("0".repeat(64)),
            "inode" => run["binary"]["inode"] = json!(0),
            "fd" => run["binary"]["fd"] = json!(2),
            "duplicate" => run["identities"][1] = run["identities"][0].clone(),
            "name" => run["identities"][0]["name"] = json!("driver_bot_token"),
            "zero-id" => run["identities"][0]["application_id"] = json!("0"),
            "unknown" => run["extra"] = json!(true),
            "writable" => {
                // The FD is valid and names a regular file, but grants writes.
                writable = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&fixture.lease.control)
                    .unwrap();
                run["binary"]["fd"] = json!(writable.as_raw_fd());
            }
            _ => unreachable!(),
        }
        fixture::assert_single_test_passed(&fixture.execute(name, case, &run), selector(name));
    }
}

#[test]
fn run_health_rejects_credential_inode_content_and_symlink_drift() {
    let name = concat!(
        module_path!(),
        "::run_health_rejects_credential_inode_content_and_symlink_drift"
    );
    if let Some(mode) = child_mode() {
        let _env = initialized_scope();
        let _held = admit_dcserver().unwrap().unwrap();
        bound_listener("127.0.0.1:18791".parse().unwrap()).unwrap();
        assert!(health_witness().unwrap().is_some());
        let root = &instance::current().unwrap().unwrap().root;
        let path = root.join("credential/claude_bot_token");
        match mode.as_str() {
            "inode" => {
                std::fs::rename(&path, path.with_extension("old")).unwrap();
                std::fs::write(&path, b"synthetic-replacement").unwrap();
            }
            "content" => std::fs::write(&path, b"synthetic-mutated-content").unwrap(),
            "symlink" => {
                std::fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink(root.join("foreign-sentinel"), &path).unwrap();
            }
            "late-extra" => {
                let extra = root.join("credential/driver_bot_token");
                std::fs::write(&extra, b"synthetic-legacy-extra").unwrap();
                std::fs::set_permissions(&extra, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            "directory-mode" => std::fs::set_permissions(
                root.join("credential"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap(),
            _ => unreachable!(),
        }
        assert!(
            health_witness().is_err(),
            "credential drift must invalidate run health"
        );
        assert_eq!(
            std::fs::read(root.join("foreign-sentinel")).unwrap(),
            b"untouched foreign sentinel\n"
        );
        return;
    }
    for mode in [
        "inode",
        "content",
        "symlink",
        "late-extra",
        "directory-mode",
    ] {
        let fixture = RunFixture::new(false);
        fixture::assert_single_test_passed(
            &fixture.execute(name, mode, &fixture.run),
            selector(name),
        );
    }
}

#[test]
fn run_health_rejects_replaced_current_executable_path() {
    let name = concat!(
        module_path!(),
        "::run_health_rejects_replaced_current_executable_path"
    );
    if child_mode().is_some() {
        let _env = initialized_scope();
        let _held = admit_dcserver().unwrap().unwrap();
        bound_listener("127.0.0.1:18791".parse().unwrap()).unwrap();
        let image = std::env::current_exe().unwrap();
        std::fs::rename(&image, image.with_extension("old")).unwrap();
        std::fs::write(image, b"synthetic-replacement-image").unwrap();
        assert!(
            health_witness().is_err(),
            "replaced image pathname must invalidate health"
        );
        return;
    }
    let fixture = RunFixture::new(true);
    fixture::assert_single_test_passed(
        &fixture.execute(name, "replacement", &fixture.run),
        selector(name),
    );
}

#[test]
fn run_admission_rejects_a_path_retargeted_after_exec() {
    let name = concat!(
        module_path!(),
        "::run_admission_rejects_a_path_retargeted_after_exec"
    );
    if child_mode().is_some() {
        let _env = initialized_scope();
        let mut expected: Value =
            serde_json::from_str(&std::env::var("AGENTDESK_TEST_RUN").unwrap()).unwrap();
        let path = std::env::current_exe().unwrap();
        let original = path.with_extension("mapped");
        std::fs::rename(&path, &original).unwrap();
        std::fs::copy(&original, &path).unwrap();
        let replacement = File::open(&path).unwrap();
        let metadata = replacement.metadata().unwrap();
        expected["binary"]["fd"] = json!(replacement.as_raw_fd());
        expected["binary"]["device"] = json!(metadata.dev());
        expected["binary"]["inode"] = json!(metadata.ino());
        std::mem::forget(replacement);
        // Path and inherited bytes agree; only the mapped-image proof distinguishes the run.
        unsafe {
            std::env::set_var("AGENTDESK_TEST_RUN", expected.to_string());
        }
        assert!(
            admit_dcserver().is_err(),
            "a pathname replacement was mistaken for the loaded image"
        );
        return;
    }
    let fixture = RunFixture::new(true);
    fixture::assert_single_test_passed(
        &fixture.execute(name, "mapped-old", &fixture.run),
        selector(name),
    );
}

#[test]
fn legacy_lease_without_run_keeps_health_block_absent() {
    let name = concat!(
        module_path!(),
        "::legacy_lease_without_run_keeps_health_block_absent"
    );
    if std::env::var_os(MODE).is_some() {
        let _env = initialized_scope();
        let _held = admit_dcserver().unwrap().unwrap();
        bound_listener("127.0.0.1:18791".parse().unwrap()).unwrap();
        assert!(health_witness().unwrap().is_none());
        return;
    }
    let fixture = LeaseFixture::new(true);
    fixture::assert_single_test_passed(
        &finish(fixture.spawn(name, "legacy", Some(&fixture.witness), true)),
        selector(name),
    );
}
