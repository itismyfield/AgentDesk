use super::*;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU64, Ordering::SeqCst},
};

struct Fixture {
    root: tempfile::TempDir,
    now: Arc<AtomicU64>,
    fault: Arc<AtomicU8>,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
            now: Arc::new(AtomicU64::new(100_000)),
            fault: Arc::new(AtomicU8::new(0)),
        };
        fixture.configure(true, 2);
        fixture
    }

    fn configure(&self, enabled: bool, max_empty: usize) {
        std::fs::write(self.root.path().join("config.yaml"), format!(
            "data:\n  dir: {:?}\ncluster:\n  gateway_handback_breaker:\n    enabled: {enabled}\n    max_empty: {max_empty}\n",
            self.root.path().join("data")
        )).unwrap();
    }

    fn path(&self, token: &str) -> PathBuf {
        self.root
            .path()
            .join("gateway_handback_breaker")
            .join(format!("claude-{token}.json"))
    }

    fn owner(&self, token: &str) -> GatewayHandbackBreaker {
        let (now, fault) = (self.now.clone(), self.fault.clone());
        let path = self.path(token);
        let config = self.root.path().join("config.yaml");
        GatewayHandbackBreaker::with_sources(
            "claude",
            &format!("discord_{token}"),
            Some(self.root.path().into()),
            move || {
                match fault.swap(0, SeqCst) {
                    1 => std::fs::write(path.parent().unwrap(), "blocked").unwrap(),
                    2 => {
                        if path.exists() {
                            std::fs::rename(&path, path.with_extension("saved")).unwrap();
                        }
                        std::fs::create_dir_all(&path).unwrap();
                    }
                    _ => {}
                }
                now.load(SeqCst)
            },
            move || {
                serde_yaml::from_str::<crate::config::Config>(
                    &std::fs::read_to_string(&config).unwrap(),
                )
                .unwrap()
                .cluster
                .gateway_handback_breaker
            },
        )
    }

    fn advance(&self, seconds: u64) {
        self.now.fetch_add(seconds, SeqCst);
    }
}

fn empty(owner: &mut GatewayHandbackBreaker) {
    assert!(owner.record_yield());
    owner.observe(Ok(true));
}

fn capture(run: impl FnOnce()) -> String {
    let file = tempfile::tempfile().unwrap();
    let mut reader = file.try_clone().unwrap();
    crate::logging::test_capture::pin_callsite_interest();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || file.try_clone().unwrap())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    run();
    reader.seek(SeekFrom::Start(0)).unwrap();
    let mut output = String::new();
    reader.read_to_string(&mut output).unwrap();
    output
}

#[test]
fn dense_flapping_expires_and_day_old_history_does_not_retrigger_manual_hold() {
    let fixture = Fixture::new();
    let mut owner = fixture.owner("a");
    let logs = capture(|| {
        empty(&mut owner);
        assert!(!owner.suppressed());
        empty(&mut owner);
        assert!(owner.suppressed());
        assert!(!owner.record_yield());
        fixture.advance(1799);
        assert!(owner.suppressed());
        fixture.advance(1);
        assert!(!owner.suppressed());
    });
    assert_eq!(logs.matches("gateway_handback_suppressed").count(), 1);
    fixture.advance(DAY_SECS);
    empty(&mut owner);
    empty(&mut owner);
    assert!(owner.suppressed());
    fixture.advance(1800);
    assert!(!owner.suppressed());
}

#[test]
fn sparse_daily_budget_and_second_dense_activation_survive_restart_until_deleted() {
    for max_empty in [1, 2] {
        let fixture = Fixture::new();
        fixture.configure(true, max_empty);
        let mut owner = fixture.owner("a");
        let attempts = if max_empty == 1 { 2 } else { 4 };
        for _ in 0..attempts {
            empty(&mut owner);
            fixture.advance(if max_empty == 1 { 1800 } else { 660 });
        }
        assert!(owner.suppressed());
        assert!(!owner.record_yield());
        fixture.advance(DAY_SECS * 2);
        owner = fixture.owner("a");
        assert!(owner.suppressed());
        assert!(!fixture.owner("b").suppressed());
        std::fs::remove_file(fixture.path("a")).unwrap();
        assert!(!owner.suppressed());
        assert!(owner.record_yield());
    }
}

#[test]
fn hot_kill_switch_bypasses_timed_and_manual_holds_without_touching_state() {
    let _environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for manual in [false, true] {
        let fixture = Fixture::new();
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            fixture.root.path(),
        );
        let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &fixture.root.path().join("config.yaml"),
        );
        let now = fixture.now.clone();
        let mut owner = GatewayHandbackBreaker::with_sources(
            "claude",
            "discord_a",
            Some(fixture.root.path().into()),
            move || now.load(SeqCst),
            || {
                crate::config::load_graceful()
                    .cluster
                    .gateway_handback_breaker
            },
        );
        for _ in 0..if manual { 4 } else { 2 } {
            empty(&mut owner);
            if manual {
                fixture.advance(660);
            }
        }
        let before = std::fs::read(fixture.path("a")).unwrap();
        fixture.configure(false, 2);
        for _ in 0..5 {
            assert!(!owner.suppressed());
            empty(&mut owner);
        }
        assert_eq!(std::fs::read(fixture.path("a")).unwrap(), before);
        fixture.configure(true, 2);
        assert!(owner.suppressed());
    }
}

#[test]
fn pending_survives_disable_errors_and_restarts_and_is_settled_only_once() {
    let fixture = Fixture::new();
    let mut owner = fixture.owner("a");
    assert!(owner.record_yield());
    fixture.configure(false, 2);
    owner.observe(Ok(true));
    fixture.configure(true, 2);
    assert!(!owner.record_yield());
    owner.observe(Err(()));
    assert!(!owner.record_yield());
    owner = fixture.owner("a");
    owner.observe(Ok(true));
    owner = fixture.owner("a");
    owner.observe(Ok(true));
    assert!(!owner.suppressed());
    for _ in 0..3 {
        assert!(owner.record_yield());
        owner.observe(Ok(false));
        assert!(!owner.suppressed());
    }
    empty(&mut owner);
    assert!(owner.suppressed());
}

#[test]
fn malformed_and_unreadable_state_suppress_without_overwrite_and_disable_ignores_them() {
    let fixture = Fixture::new();
    let mut owner = fixture.owner("a");
    std::fs::create_dir_all(fixture.path("a").parent().unwrap()).unwrap();
    for directory in [false, true] {
        if directory {
            std::fs::create_dir(fixture.path("a")).unwrap();
        } else {
            std::fs::write(fixture.path("a"), "broken").unwrap();
        }
        let logs = capture(|| {
            assert!(owner.suppressed());
            assert!(!owner.record_yield());
            owner.observe(Ok(true));
        });
        assert_eq!(
            logs.matches("gateway_handback_breaker_state_error").count(),
            3
        );
        fixture.configure(false, 2);
        assert!(
            capture(|| {
                assert!(!owner.suppressed());
                empty(&mut owner);
            })
            .is_empty()
        );
        fixture.configure(true, 2);
        if directory {
            std::fs::remove_dir(fixture.path("a")).unwrap();
        } else {
            assert_eq!(std::fs::read(fixture.path("a")).unwrap(), b"broken");
            std::fs::remove_file(fixture.path("a")).unwrap();
        }
        assert!(!owner.suppressed());
    }
}

#[test]
fn pending_create_and_rename_failures_refuse_yield_and_settlement_retries_original_observation() {
    let fixture = Fixture::new();
    let mut owner = fixture.owner("a");
    let path = fixture.path("a");
    for fault in [1, 2] {
        fixture.fault.store(fault, SeqCst);
        let logs = capture(|| assert!(!owner.record_yield()));
        assert!(logs.contains("gateway_handback_breaker_state_error"));
        if fault == 1 {
            std::fs::remove_file(path.parent().unwrap()).unwrap();
        } else {
            std::fs::remove_dir(&path).unwrap();
        }
    }
    for retry in 0..3 {
        let fixture = Fixture::new();
        let mut owner = fixture.owner("a");
        let path = fixture.path("a");
        empty(&mut owner);
        assert!(owner.record_yield());
        fixture.fault.store(2, SeqCst);
        let logs = capture(|| owner.observe(Ok(true)));
        assert!(logs.contains("gateway_handback_breaker_state_error"));
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(path.with_extension("saved"), &path).unwrap();
        assert!(!owner.record_yield());
        if retry == 2 {
            std::fs::remove_file(&path).unwrap();
            assert!(!owner.suppressed());
            assert!(owner.record_yield());
            continue;
        }
        if retry == 0 {
            owner.observe(Ok(false));
        } else {
            assert!(owner.suppressed());
        }
        assert!(owner.suppressed());
        owner = fixture.owner("a");
        assert!(owner.suppressed());
        owner.observe(Ok(true));
        fixture.advance(1800);
        assert!(!owner.suppressed());
    }
}
