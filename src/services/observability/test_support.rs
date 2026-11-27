use std::sync::{Mutex, MutexGuard, OnceLock};

pub fn test_runtime_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub struct EnvRuntimeGuard {
    _runtime: MutexGuard<'static, ()>,
    _env: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

// Dual-lock fixtures acquire environment first and release runtime first.
pub fn lock_env_then_runtime() -> EnvRuntimeGuard {
    let env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let runtime = test_runtime_lock();
    EnvRuntimeGuard {
        _runtime: runtime,
        _env: env,
    }
}
