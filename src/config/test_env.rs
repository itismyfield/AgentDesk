pub(crate) mod teardown_probe;

pub(crate) struct TestEnvVarGuard {
    _lock: Option<super::test_env_lock::SharedTestEnvLockGuard>,
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl TestEnvVarGuard {
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

    /// Set a non-path value while the caller owns the shared environment lock.
    pub(crate) fn set_value_after_shared_test_env_lock(
        key: &'static str,
        value: &std::ffi::OsStr,
    ) -> Self {
        let guard = Self::capture_after_shared_test_env_lock(key);
        unsafe { std::env::set_var(key, value) };
        guard
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

pub(crate) fn set_agentdesk_root_for_test(path: &std::path::Path) -> TestEnvVarGuard {
    TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", path)
}

pub(crate) struct TestRuntimeRootGuard {
    _env: TestEnvVarGuard,
    _root: tempfile::TempDir,
}

impl TestRuntimeRootGuard {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().expect("runtime root");
        let env = set_agentdesk_root_for_test(root.path());
        Self {
            _env: env,
            _root: root,
        }
    }
}
