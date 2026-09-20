use std::sync::{Mutex, MutexGuard, OnceLock};

pub(crate) fn test_runtime_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) struct EnvRuntimeGuard {
    _runtime: MutexGuard<'static, ()>,
    _env: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

// Dual-lock fixtures acquire environment first and release runtime first.
pub(crate) fn lock_env_then_runtime() -> EnvRuntimeGuard {
    let env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let runtime = test_runtime_lock();
    EnvRuntimeGuard {
        _runtime: runtime,
        _env: env,
    }
}

#[cfg(unix)]
#[test]
fn env_runtime_fixtures_finish_in_parallel() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for round in 0..3 {
        let output = tempfile::tempfile().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "synthetic_terminal_ordering_tests::",
                "witnessless_reclaim_records_the_i20_violation_and_a_witnessed_one_does_not",
                "thread_follow_up_tmux_ready_claim_records_intended_classification_4984",
                "voice_intake_chime_",
                "voice_foreground_path_does_not_double_chime",
                "voice_pcm_harness_unattended_e2e",
                "--test-threads=8",
            ])
            .env_remove("ADK_VOICE_PCM_HARNESS_REPORT")
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output.try_clone().unwrap()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        use std::io::{Read, Seek, SeekFrom};
        let mut output = output;
        output.seek(SeekFrom::Start(0)).unwrap();
        let mut log = String::new();
        output.read_to_string(&mut log).unwrap();
        println!("parallel fixture round {round}:\n{log}");
        assert!(
            status.is_some_and(|status| status.success()),
            "round {round}: fixtures failed or deadlocked\n{log}"
        );
        assert!(
            log.contains("13 passed; 0 failed; 0 ignored"),
            "round {round}: incomplete fixture coverage\n{log}"
        );
    }
}
