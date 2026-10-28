use std::path::{Path, PathBuf};

use crate::config::Config;

// Each child starts with empty LIVE and LOCAL cells, independent of other lib tests.
pub(super) fn isolated() -> Option<PathBuf> {
    const CHILD: &str = "ADK_TEST_CLUSTER_SNAPSHOT_CHILD";
    const ROOT: &str = "ADK_TEST_CLUSTER_SNAPSHOT_ROOT";
    let test = std::thread::current().name().unwrap().to_string();
    if std::env::var(CHILD).ok().as_deref() == Some(test.as_str()) {
        return Some(PathBuf::from(std::env::var_os(ROOT).unwrap()));
    }
    let root = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &test, "--nocapture", "--test-threads=1"])
        .env(CHILD, &test)
        .env(ROOT, root.path())
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env("AGENTDESK_CONFIG", root.path().join("disk.yaml"))
        .env_remove("ADK_INTAKE_ROUTING_MODE")
        .env_remove("AGENTDESK_CLUSTER_API_BASE_URL")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{test}: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
    None
}

pub(super) fn write_disk_config(root: &Path, config: &Config) {
    let mut config = config.clone();
    config.data.dir = root.join("data");
    std::fs::write(
        root.join("disk.yaml"),
        serde_yaml::to_string(&config).unwrap(),
    )
    .unwrap();
}

#[test]
fn capacity_before_snapshot_is_not_enabled_by_disk() {
    let Some(root) = isolated() else { return };
    let mut disk = Config::default();
    disk.cluster.intake_routing.capacity_aware = true;
    write_disk_config(&root, &disk);
    assert!(crate::config_live_reload::current().is_none());
    assert!(!super::execution_capacity::automatic_enabled());
}

#[test]
fn routing_before_snapshot_keeps_unknown_owner_authority_fence() {
    use super::intake_routing_config::{IntakeRoutingMode, OwnerAuthorityChannelOptIn};

    let Some(root) = isolated() else { return };
    let mut disk = Config::default();
    disk.cluster.intake_routing.enabled = true;
    disk.cluster.intake_routing.mode = crate::config::ClusterIntakeRoutingMode::Enforce;
    disk.cluster.intake_routing.owner_authority_channel_ids = vec!["42".into()];
    write_disk_config(&root, &disk);
    assert!(crate::config_live_reload::current().is_none());
    let effective = super::intake_routing_config::effective_intake_routing_config();
    assert_eq!(effective.mode, IntakeRoutingMode::Disabled);
    assert!(!effective.owner_authority_config_known);
    assert_eq!(
        effective.owner_authority_channel_opt_in("42"),
        OwnerAuthorityChannelOptIn::Unknown
    );
}

#[test]
fn routing_and_capacity_follow_installed_snapshot_replacements() {
    use super::intake_routing_config::{IntakeRoutingMode, OwnerAuthorityChannelOptIn};

    let Some(root) = isolated() else { return };
    let mut disk = Config::default();
    disk.cluster.intake_routing.enabled = true;
    disk.cluster.intake_routing.mode = crate::config::ClusterIntakeRoutingMode::Enforce;
    disk.cluster.intake_routing.capacity_aware = true;
    disk.cluster.intake_routing.owner_authority_channel_ids = vec!["disk-only".into()];
    write_disk_config(&root, &disk);
    let mut installed = disk.clone();
    installed.cluster.intake_routing.owner_authority_channel_ids = vec!["42".into()];
    crate::config_live_reload::install(installed.clone());
    for (enabled, expected) in [
        (true, IntakeRoutingMode::Enforce),
        (false, IntakeRoutingMode::Disabled),
    ] {
        installed.cluster.intake_routing.enabled = enabled;
        installed.cluster.intake_routing.capacity_aware = enabled;
        crate::config_live_reload::install(installed.clone());
        let effective = super::intake_routing_config::effective_intake_routing_config();
        assert_eq!(effective.mode, expected);
        assert_eq!(super::execution_capacity::automatic_enabled(), enabled);
        assert_eq!(
            effective.owner_authority_channel_opt_in("42"),
            OwnerAuthorityChannelOptIn::OptedIn
        );
        assert_eq!(
            effective.owner_authority_channel_opt_in("disk-only"),
            OwnerAuthorityChannelOptIn::NotOptedIn
        );
    }
}

#[test]
fn discovery_names_follow_snapshot_without_reading_disk() {
    use super::session_discovery::build_yaml_channel_name_map;
    use crate::services::provider::ProviderKind;

    let Some(root) = isolated() else { return };
    let config = |name: &str| -> Config {
        serde_yaml::from_str(&format!(
            "server: {{}}\nagents:\n  - id: fixture\n    name: Fixture\n    channels:\n      claude: {{id: '42', name: '{name}'}}\n"
        ))
        .unwrap()
    };
    write_disk_config(&root, &config("disk-only"));
    assert!(build_yaml_channel_name_map().is_empty());
    let key = (
        "fixture".to_string(),
        ProviderKind::Claude,
        "42".to_string(),
    );
    for name in ["installed", "reloaded"] {
        crate::config_live_reload::install(config(name));
        let names = build_yaml_channel_name_map();
        assert_eq!(names.len(), 1);
        assert_eq!(names.get(&key).map(String::as_str), Some(name));
    }
}

#[tokio::test]
async fn runtime_capability_refresh_writes_installed_snapshot_to_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use serde_json::{Value, json};

    let Some(root) = isolated() else { return };
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    sqlx::query("INSERT INTO worker_nodes(instance_id, last_heartbeat_at, capabilities) VALUES ('snapshot-worker', NOW(), '{}'::jsonb)")
        .execute(&pool)
        .await
        .unwrap();
    let mut config = Config::default();
    config.cluster.api_base_url = Some("https://disk.example".into());
    config
        .cluster
        .capabilities
        .insert("config_source".into(), json!("disk-only"));
    write_disk_config(&root, &config);
    for source in ["installed", "reloaded"] {
        config.cluster.api_base_url = Some(format!("https://{source}.example"));
        config
            .cluster
            .capabilities
            .insert("config_source".into(), json!(source));
        crate::config_live_reload::install(config.clone());
        super::intake_worker_capabilities::refresh_worker_node_runtime_capabilities(
            &pool,
            "snapshot-worker",
        )
        .await
        .unwrap();
        let capabilities: Value = sqlx::query_scalar(
            "SELECT capabilities FROM worker_nodes WHERE instance_id = 'snapshot-worker'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(capabilities["config_source"], json!(source));
        assert_eq!(
            capabilities["agentdesk_api"]["base_url"],
            json!(format!("https://{source}.example"))
        );
    }
    pool.close().await;
    fixture.drop().await;
}
