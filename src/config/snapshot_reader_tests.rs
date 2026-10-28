use super::*;
use std::sync::{Arc, Mutex};

pub(crate) fn isolated(body: impl FnOnce()) {
    const CHILD: &str = "ADK_TEST_SNAPSHOT_READER_CHILD";
    let thread = std::thread::current();
    let name = thread.name().expect("named libtest worker");
    if std::env::var(CHILD).ok().as_deref() == Some(name) {
        assert!(
            snapshot().is_none(),
            "each reader starts in a fresh process"
        );
        body();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{name}: {stdout}\n{stderr}");
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
    println!("snapshot child executed: {name}");
}

pub(crate) struct Fixture {
    _root_env: crate::config::TestEnvVarGuard,
    root: tempfile::TempDir,
    pub(crate) path: PathBuf,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let root_env = crate::config::set_agentdesk_root_for_test(root.path());
        let path = root.path().join("config/agentdesk.yaml");
        std::env::set_current_dir(root.path()).unwrap();
        // Each fixture runs in a fresh single-test child, before its runtime starts.
        unsafe {
            std::env::set_var("AGENTDESK_CONFIG", &path);
            std::env::remove_var("DATABASE_URL");
        }
        Self {
            _root_env: root_env,
            root,
            path,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        self.root.path()
    }

    pub(crate) fn write_at(&self, path: &Path, yaml: &str) {
        let mut value: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let map = value.as_mapping_mut().unwrap();
        map.entry("server".into())
            .or_insert_with(|| serde_yaml::Value::Mapping(Default::default()));
        map.entry("data".into()).or_insert_with(|| {
            serde_yaml::to_value(serde_json::json!({"dir": self.root().join("data")})).unwrap()
        });
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_yaml::to_string(&value).unwrap()).unwrap();
    }

    pub(crate) fn write(&self, yaml: &str) {
        self.write_at(&self.path, yaml);
    }

    pub(crate) fn install(&self, initial: &str, disk: &str) {
        self.write(initial);
        crate::config_live_reload::install(load_from_path(&self.path).unwrap());
        self.write(disk);
    }

    pub(crate) fn reload(&self) {
        assert!(matches!(
            crate::config_live_reload::reload_from_path(&self.path),
            crate::config_live_reload::ReloadOutcome::Applied { .. }
        ));
    }
}

pub(crate) fn capture_identity<T>(body: impl FnOnce() -> T) -> (T, Vec<bool>) {
    use tracing_subscriber::layer::SubscriberExt;
    struct Capture(Arc<Mutex<Vec<bool>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != crate::api_caller_observability::LOG_TARGET {
                return;
            }
            struct Visitor<'a>(&'a Mutex<Vec<bool>>);
            impl tracing::field::Visit for Visitor<'_> {
                fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
                    if field.name() == "manager_channel_check_relied_on_claimed_header" {
                        self.0.lock().unwrap().push(value);
                    }
                }
                fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
            }
            event.record(&mut Visitor(&self.0));
        }
    }
    let values = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Capture(values.clone()));
    let result = tracing::subscriber::with_default(subscriber, body);
    let collected = values.lock().unwrap().clone();
    (result, collected)
}

#[test]
fn snapshot_facades_keep_published_value_and_preserve_boot_fallback() {
    isolated(|| {
        let fixture = Fixture::new();
        assert!(matches!(
            crate::config::shared_test_env_lock().try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        fixture.write("server: {port: 41}\nmeeting: {channel_name: fixture, available_agents: []}");
        for config in [
            runtime_config(),
            runtime_config_strict().unwrap(),
            runtime_config_optional().unwrap(),
        ] {
            assert_eq!(config.server.port, 41);
            let source = config.runtime_source.unwrap();
            assert_eq!(source.path, fixture.path);
            assert!(source.meeting_available_agents_explicitly_empty);
        }
        fixture.install("server: {port: 41}", "server: {port: 73}");
        assert_eq!(runtime_config().server.port, 41);
        assert_eq!(runtime_config_strict().unwrap().server.port, 41);
        assert_eq!(runtime_config_optional().unwrap().server.port, 41);
        fixture.reload();
        assert_eq!(runtime_config().server.port, 73);
        assert!(
            !runtime_config()
                .runtime_source
                .unwrap()
                .meeting_available_agents_explicitly_empty
        );
    });
}

#[cfg(unix)]
#[test]
fn snapshot_facades_do_not_touch_disk_after_publication() {
    use std::os::unix::fs::PermissionsExt;
    isolated(|| {
        let fixture = Fixture::new();
        let unread_data = fixture.root().join("unread-config-data");
        let disk = serde_yaml::to_string(&serde_json::json!({
            "server": {"port": 73, "auth_token": "synthetic-disk-secret"},
            "data": {"dir": unread_data},
        }))
        .unwrap();
        fixture.install("server: {port: 41}", &disk);
        std::fs::set_permissions(&fixture.path, std::fs::Permissions::from_mode(0o644)).unwrap();
        for config in [
            runtime_config(),
            runtime_config_strict().unwrap(),
            runtime_config_optional().unwrap(),
        ] {
            assert_eq!(config.server.port, 41);
        }
        assert!(
            !unread_data.exists(),
            "an eager discarded load must not create the disk config's data directory"
        );
        assert_eq!(
            std::fs::metadata(&fixture.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644,
            "an eager discarded optional load must not audit the unread secret file"
        );
    });
}
