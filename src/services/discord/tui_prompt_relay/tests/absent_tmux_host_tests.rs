//! A host where every tmux session is absent, so the real dead verdict cannot be flipped
//! by a concurrent fixture whose fake `tmux` on PATH answers "live" for every name.

use crate::config::TestEnvVarGuard;
use crate::config::test_env_lock::{SharedTestEnvLockGuard, acquire_shared_test_env_lock};

/// Env lock, then a `tmux` reporting every session missing, then the dedupe lock (the
/// canonical order); fields drop in reverse of that.
pub(super) struct AbsentTmuxHost {
    _dedupe: std::sync::MutexGuard<'static, ()>,
    _path: TestEnvVarGuard,
    _bin: tempfile::TempDir,
    _env: SharedTestEnvLockGuard,
}

impl AbsentTmuxHost {
    pub(super) fn isolate() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let env = acquire_shared_test_env_lock();
        let bin = tempfile::tempdir().expect("absent tmux dir");
        let tmux = bin.path().join("tmux");
        std::fs::write(
            &tmux,
            "#!/bin/sh\necho \"can't find session: $*\" >&2\nexit 1\n",
        )
        .expect("absent tmux");
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700))
            .expect("executable absent tmux");
        let path = TestEnvVarGuard::prepend_path_after_shared_test_env_lock(bin.path());
        let dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Self {
            _dedupe: dedupe,
            _path: path,
            _bin: bin,
            _env: env,
        }
    }
}
