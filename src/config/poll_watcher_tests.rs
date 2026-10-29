use super::*;

#[derive(Clone, Default)]
struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn write_preserving_mtime(path: &Path, content: &str, modified: std::time::SystemTime) {
    std::fs::write(path, content).unwrap();
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert_eq!(
        std::fs::metadata(path).unwrap().modified().unwrap(),
        modified
    );
}

async fn wait_for_expiry(expected: u64) {
    let mut updates = subscribe();
    tokio::time::timeout(Duration::from_secs(15), async {
        while current().unwrap().cluster.gateway_lease_idle_expiry_secs != expected {
            updates.changed().await.unwrap();
        }
    })
    .await
    .expect("the real watcher must install the edited config");
}

#[test]
fn poll_watcher_handles_in_place_rename_rejection_and_same_content() {
    const CHILD: &str = "ADK_TEST_CONFIG_POLL_WRITES";
    if std::env::var_os(CHILD).is_none() {
        let thread = std::thread::current();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", thread.name().unwrap(), "--nocapture"])
            .env(CHILD, "1")
            .env_remove("AGENTDESK_CONFIG")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
        return;
    }

    assert!(current().is_none(), "the child must start without LIVE");
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().unwrap();
    let path = root.join("config.yaml");
    let _path = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &path,
    );
    let config = Config {
        data: crate::config::DataConfig {
            dir: root.join("data"),
            ..Default::default()
        },
        cluster: crate::config::ClusterConfig {
            gateway_lease_idle_expiry_secs: 41,
            ..Default::default()
        },
        ..Default::default()
    };
    std::fs::write(&path, serde_yaml::to_string(&config).unwrap()).unwrap();
    install(crate::config::load_from_path(&path).unwrap());
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let logs = LogBuffer::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(logs.clone())
            .finish(),
    )
    .unwrap();
    let _guard = start(path.clone(), true).expect("the fixture directory can be watched");

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut edited = (*current().unwrap()).clone();
            edited.cluster.gateway_lease_idle_expiry_secs = 73;
            write_preserving_mtime(&path, &serde_yaml::to_string(&edited).unwrap(), modified);
            wait_for_expiry(73).await;

            edited.cluster.gateway_lease_idle_expiry_secs = 97;
            let replacement = root.join("replacement.tmp");
            write_preserving_mtime(
                &replacement,
                &serde_yaml::to_string(&edited).unwrap(),
                modified,
            );
            std::fs::rename(&replacement, &path).unwrap();
            wait_for_expiry(97).await;

            let validated = current().unwrap();
            write_preserving_mtime(&path, "server: [broken", modified);
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    if String::from_utf8_lossy(&logs.0.lock().unwrap())
                        .contains("config hot-reload rejected invalid config")
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the real watcher must attempt and reject the invalid edit");
            assert!(Arc::ptr_eq(&validated, &current().unwrap()));

            edited.cluster.gateway_lease_idle_expiry_secs = 113;
            write_preserving_mtime(&path, &serde_yaml::to_string(&edited).unwrap(), modified);
            wait_for_expiry(113).await;

            let before = current().unwrap();
            let mut updates = subscribe();
            std::fs::write(root.join("sibling.yaml"), "server: [also broken").unwrap();
            assert!(
                tokio::time::timeout(Duration::from_secs(5), updates.changed())
                    .await
                    .is_err(),
                "a sibling edit must not reinstall the target config"
            );
            assert!(Arc::ptr_eq(&before, &current().unwrap()));

            let same_content = serde_yaml::to_string(&*before).unwrap();
            write_preserving_mtime(&path, &same_content, modified + Duration::from_secs(10));
            tokio::time::timeout(Duration::from_secs(15), updates.changed())
                .await
                .expect("a newer mtime must trigger the same-content reload")
                .unwrap();
            let after = current().unwrap();
            assert_eq!(
                serde_json::to_value(&*after).unwrap(),
                serde_json::to_value(&*before).unwrap()
            );
            assert!(restart_required_changes(&before, &after).is_empty());
        });
}
