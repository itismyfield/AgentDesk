use super::*;
use std::path::Path;

use crate::db::auto_queue::test_support::TestPostgresDb;

const CHILD: &str = "ADK_TEST_DAEMON_CONFIG_LIFETIME";
const BOOT_EXPIRY: u64 = 41;
const EDIT_EXPIRY: u64 = 73;

fn expiry() -> Option<u64> {
    current().map(|config| config.cluster.gateway_lease_idle_expiry_secs)
}

fn write_config(path: &Path, config: &Config) {
    std::fs::write(path, serde_yaml::to_string(config).unwrap()).unwrap();
}

async fn edit_and_observe(path: &Path, config: &Config) {
    let mut updates = subscribe();
    write_config(path, config);
    tokio::time::timeout(Duration::from_secs(10), async {
        while expiry() != Some(config.cluster.gateway_lease_idle_expiry_secs) {
            updates.changed().await.unwrap();
        }
    })
    .await
    .expect("the retained filesystem watcher must apply the edit");
}

fn assert_boot_snapshot() {
    assert_eq!(
        expiry(),
        Some(BOOT_EXPIRY),
        "the validated boot config must be installed before HTTP or policy engine startup"
    );
}

fn assert_policy_engine_failure(config: &Config, pool: &sqlx::PgPool) {
    let error = match crate::engine::PolicyEngine::new_with_pg(config, Some(pool.clone())) {
        Ok(_) => panic!("a policies path that names a file must fail engine startup"),
        Err(error) => error,
    };
    assert!(
        error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotADirectory)
        }),
        "{error:#}"
    );
}

async fn assert_http_startup_failure(config: &Config) {
    let engine = crate::engine::PolicyEngine::new_with_pg(config, None).unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        crate::server::run(config.clone(), engine, None, None),
    )
    .await
    .expect("the HTTP startup future must return")
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "PostgreSQL is required for AgentDesk server runtime"
    );
}

async fn dcserver_scenario(scenario: &str, config: &Config, path: &Path, pool: &sqlx::PgPool) {
    let _guard = crate::bootstrap::prepare_dcserver_boot(config, pool).await;
    assert_boot_snapshot();
    let mut edited = config.clone();
    edited.cluster.gateway_lease_idle_expiry_secs = EDIT_EXPIRY;

    if scenario == "engine-init-failure" {
        assert_policy_engine_failure(config, pool);
        assert_boot_snapshot();
    } else {
        edit_and_observe(path, &edited).await;
        assert_http_startup_failure(config).await;
        assert_eq!(
            expiry(),
            Some(EDIT_EXPIRY),
            "the HTTP startup future must not reinstall its old boot config"
        );
        tokio::time::sleep(DEBOUNCE + Duration::from_millis(50)).await;
        edited.cluster.gateway_lease_idle_expiry_secs = 97;
    }

    edit_and_observe(path, &edited).await;
    assert_eq!(
        expiry(),
        Some(edited.cluster.gateway_lease_idle_expiry_secs)
    );
}

async fn standalone_scenario(config: &Config, path: &Path, pool: &sqlx::PgPool) {
    crate::launch::after_database_ready(config, Some(pool), || async {
        assert_boot_snapshot();
        let mut edited = config.clone();
        edited.cluster.gateway_lease_idle_expiry_secs = EDIT_EXPIRY;
        edit_and_observe(path, &edited).await;
        Ok(())
    })
    .await
    .unwrap();

    let mut updates = subscribe();
    let mut after_exit = config.clone();
    after_exit.cluster.gateway_lease_idle_expiry_secs = 97;
    write_config(path, &after_exit);
    assert!(
        tokio::time::timeout(Duration::from_millis(1200), updates.changed())
            .await
            .is_err(),
        "returning from the standalone server must drop its watcher"
    );
    assert_eq!(expiry(), Some(EDIT_EXPIRY));
}

async fn producer_scenario(scenario: &str, config: &Config, path: &Path, pool: &sqlx::PgPool) {
    let _guard = crate::bootstrap::prepare_dcserver_boot(config, pool).await;
    assert_boot_snapshot();
    if scenario == "boot-producer" {
        assert_policy_engine_failure(config, pool);
        assert_boot_snapshot();
        return;
    }
    let mut edited = config.clone();
    edited.cluster.gateway_lease_idle_expiry_secs = EDIT_EXPIRY;
    write_config(path, &edited);
    assert!(matches!(
        reload_from_path(path),
        ReloadOutcome::Applied { .. }
    ));
    assert_eq!(expiry(), Some(EDIT_EXPIRY));
    assert_http_startup_failure(config).await;
    assert_eq!(
        expiry(),
        Some(EDIT_EXPIRY),
        "the HTTP startup future must not reinstall its old boot config"
    );
}

fn run_child(scenario: &str) {
    let test = std::thread::current();
    let test_id = test.name().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_id, "--nocapture"])
        .env(CHILD, scenario)
        .env_remove("AGENTDESK_CONFIG")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{scenario}: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
}

fn run_scenario(scenario: &str) {
    assert!(
        current().is_none(),
        "each child starts without a live snapshot"
    );
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
        config_hot_reload: !matches!(scenario, "boot-producer" | "http-producer"),
        policies: crate::config::PoliciesConfig {
            dir: root.join("policies"),
            hot_reload: false,
            ..Default::default()
        },
        cluster: crate::config::ClusterConfig {
            gateway_lease_idle_expiry_secs: BOOT_EXPIRY,
            ..Default::default()
        },
        ..Default::default()
    };
    if matches!(scenario, "engine-init-failure" | "boot-producer") {
        std::fs::write(
            &config.policies.dir,
            "a file cannot be a policies directory",
        )
        .unwrap();
    } else {
        std::fs::create_dir(&config.policies.dir).unwrap();
    }
    let mut disk = config.clone();
    disk.cluster.gateway_lease_idle_expiry_secs = 59;
    write_config(&path, &disk);

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            match scenario {
                "engine-init-failure" | "http-future-exit" => {
                    dcserver_scenario(scenario, &config, &path, &pool).await;
                }
                "standalone-watch-scope" => standalone_scenario(&config, &path, &pool).await,
                "boot-producer" | "http-producer" => {
                    producer_scenario(scenario, &config, &path, &pool).await;
                }
                other => panic!("unknown child scenario: {other}"),
            }
            pool.close().await;
            db.drop().await;
        });
}

fn isolated_producer(scenario: &str) {
    if let Ok(child) = std::env::var(CHILD) {
        assert_eq!(child, scenario);
        run_scenario(scenario);
    } else {
        run_child(scenario);
    }
}

#[test]
fn daemon_boot_snapshot_precedes_policy_engine_failure_pg() {
    isolated_producer("boot-producer");
}

#[test]
fn daemon_http_start_preserves_validated_reload_pg() {
    isolated_producer("http-producer");
}

#[test]
fn daemon_snapshot_and_watcher_outlive_http_pg() {
    if let Ok(scenario) = std::env::var(CHILD) {
        run_scenario(&scenario);
    } else {
        for scenario in [
            "engine-init-failure",
            "http-future-exit",
            "standalone-watch-scope",
        ] {
            run_child(scenario);
        }
    }
}
