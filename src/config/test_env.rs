pub(crate) mod teardown_probe;

pub(crate) struct TestEnvVarGuard {
    _lock: Option<super::test_env_lock::SharedTestEnvLockGuard>,
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl TestEnvVarGuard {
    #[track_caller]
    pub(crate) fn set_path(key: &'static str, value: &std::path::Path) -> Self {
        let lock = super::test_env_lock::acquire_shared_test_env_lock();
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self {
            _lock: Some(lock),
            key,
            previous,
        }
    }

    pub(crate) fn set_path_after_shared_test_env_lock(
        key: &'static str,
        value: &std::path::Path,
    ) -> Self {
        Self::set_value_after_shared_test_env_lock(key, value.as_os_str())
    }

    pub(crate) fn remove_after_shared_test_env_lock(key: &'static str) -> Self {
        let guard = Self::capture_after_shared_test_env_lock(key);
        unsafe { std::env::remove_var(key) };
        guard
    }

    /// Set a non-path value while the caller owns the shared environment lock.
    pub(crate) fn set_value_after_shared_test_env_lock(
        key: &'static str,
        value: &std::ffi::OsStr,
    ) -> Self {
        let guard = Self::capture_after_shared_test_env_lock(key);
        unsafe { std::env::set_var(key, value) };
        guard
    }

    /// Put `dir` in front of `PATH` while the caller owns the shared env lock. The system
    /// dirs stay, because tests that spawn `sh`/`sleep`/`python3` by name run unlocked.
    #[cfg(unix)]
    pub(crate) fn prepend_path_after_shared_test_env_lock(dir: &std::path::Path) -> Self {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let paths = std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&inherited));
        let joined = std::env::join_paths(paths).expect("join PATH");
        Self::set_value_after_shared_test_env_lock("PATH", &joined)
    }

    pub(crate) fn capture_after_shared_test_env_lock(key: &'static str) -> Self {
        Self {
            _lock: None,
            key,
            previous: std::env::var_os(key),
        }
    }
}

impl Drop for TestEnvVarGuard {
    fn drop(&mut self) {
        teardown_probe::before_restore(self.key);
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[track_caller]
pub(crate) fn set_agentdesk_root_for_test(path: &std::path::Path) -> TestEnvVarGuard {
    TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", path)
}

/// A fresh TMPDIR for a fixture that moves it. Never removed: tests on other threads create
/// their temp dirs inside it meanwhile, and removing it would delete those under them.
pub(crate) fn kept_test_tmpdir(label: &str) -> std::path::PathBuf {
    tempfile::Builder::new()
        .prefix(&format!("adk-{label}-"))
        .tempdir()
        .expect("fixture TMPDIR")
        .keep()
}

pub(crate) struct TestRuntimeRootGuard {
    _config: Option<TestEnvVarGuard>,
    _env: TestEnvVarGuard,
    _root: tempfile::TempDir,
}

impl TestRuntimeRootGuard {
    pub(crate) fn path(&self) -> &std::path::Path {
        self._root.path()
    }

    pub(crate) fn with_isolated_config() -> Self {
        let mut guard = Self::new();
        // Restore the config override before the root guard releases the environment lock.
        guard._config = Some(TestEnvVarGuard::remove_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
        ));
        guard
    }

    #[track_caller]
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().expect("runtime root");
        let env = set_agentdesk_root_for_test(root.path());
        Self {
            _config: None,
            _env: env,
            _root: root,
        }
    }
}

pub(crate) fn assert_test_config_write_within_root(root: &std::path::Path, path: &std::path::Path) {
    assert!(
        !path
            .components()
            .any(|part| part == std::path::Component::ParentDir),
        "test config write must not traverse parent directories"
    );
    // Check existing ancestors before creating directories, including dangling file symlinks.
    let existing = path
        .ancestors()
        .find(|candidate| match std::fs::symlink_metadata(candidate) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => panic!("cannot inspect test config write target: {error}"),
        })
        .expect("test config write has an existing ancestor");
    let resolved = existing.canonicalize().expect("resolve test config target");
    let root = root.canonicalize().expect("resolve test root");
    assert!(
        resolved.starts_with(root),
        "test config write must stay inside its temporary root"
    );
}

fn probe_root_config_isolation(prior_present: bool, owning: bool) {
    crate::test_env_panic_probe::assert_restores_on_return_and_panic(
        &["AGENTDESK_ROOT_DIR", "AGENTDESK_CONFIG"],
        prior_present,
        || {
            let checkpoint = |path: &std::path::Path| {
                crate::test_env_panic_probe::checkpoint_values(&[
                    ("AGENTDESK_ROOT_DIR", Some(path.as_os_str())),
                    ("AGENTDESK_CONFIG", None),
                ]);
            };
            if owning {
                let root = TestRuntimeRootGuard::with_isolated_config();
                checkpoint(root.path());
            } else {
                let _lock = super::test_env_lock::acquire_shared_test_env_lock();
                let root = tempfile::tempdir().unwrap();
                let _env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
                    "AGENTDESK_ROOT_DIR",
                    root.path(),
                );
                let _config =
                    TestEnvVarGuard::remove_after_shared_test_env_lock("AGENTDESK_CONFIG");
                checkpoint(root.path());
            }
        },
    );
}

#[test]
fn root_guard_restores_present_config_on_return_and_panic() {
    probe_root_config_isolation(true, true);
}

#[test]
fn root_guard_restores_absent_config_on_return_and_panic() {
    probe_root_config_isolation(false, true);
}

#[test]
fn borrowed_root_guard_restores_present_config_on_return_and_panic() {
    probe_root_config_isolation(true, false);
}

#[test]
fn borrowed_root_guard_restores_absent_config_on_return_and_panic() {
    probe_root_config_isolation(false, false);
}
