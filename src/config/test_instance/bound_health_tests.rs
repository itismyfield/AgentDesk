use super::{current, initialize, lifecycle};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::time::Duration;

const MODE: &str = "I3A_B2A_NATIVE_CHILD";

fn descriptor(encoded: &str, field: Option<&str>) -> i32 {
    let value: Value = serde_json::from_str(encoded).expect("synthetic descriptor witness");
    let value = field.map_or(&value, |name| &value[name]);
    i32::try_from(value["fd"].as_i64().expect("declared descriptor")).unwrap()
}

fn cloexec(fd: i32) -> bool {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0, "the admitted descriptor must remain owned");
    flags & libc::FD_CLOEXEC != 0
}

fn capture_environment() -> BTreeMap<String, String> {
    let environment: BTreeMap<_, _> = std::env::vars().collect();
    for name in environment.keys() {
        let name = name.to_ascii_uppercase();
        assert!(
            !name.ends_with("_TOKEN")
                && !name.ends_with("_API_KEY")
                && !name.ends_with("_PASSWORD")
                && name != "AWS_SECRET_ACCESS_KEY",
            "credential environment name reached the native child: {name}"
        );
    }
    assert!(!environment.contains_key("AGENTDESK_TEST_RUN"));
    assert!(!environment.contains_key("AGENTDESK_TEST_LEASE"));
    environment
}

#[test]
fn b2a_native_bound_health_child() {
    let Some(mode) = std::env::var_os(MODE) else {
        let selector = concat!(module_path!(), "::b2a_native_bound_health_child");
        let selector = selector.strip_prefix("agentdesk::").unwrap_or(selector);
        let (_temporary, root) = super::tests::fixture();
        let output = super::tests::command(&root)
            .args(["--exact", selector, "--nocapture"])
            .env(MODE, "reject-unadmitted")
            .env_remove("AGENTDESK_TEST_LEASE")
            .env_remove("AGENTDESK_TEST_RUN")
            .output()
            .expect("isolated unadmitted native child");
        super::tests::assert_single_test_passed(&output, selector);
        return;
    };
    if mode == "reject-unadmitted" {
        let _environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
        initialize().expect("isolated instance boundary");
        assert!(std::env::var_os("AGENTDESK_TEST_LEASE").is_none());
        assert!(std::env::var_os("AGENTDESK_TEST_RUN").is_none());
        assert!(lifecycle::admit_dcserver().is_err());
        assert!(!lifecycle::has_admitted_run());
        return;
    }
    assert_eq!(mode, "1");
    let binary_fd = descriptor(
        &std::env::var("AGENTDESK_TEST_RUN").expect("explicit B2a run"),
        Some("binary"),
    );
    let lease_fd = descriptor(
        &std::env::var("AGENTDESK_TEST_LEASE").expect("inherited lease"),
        None,
    );
    initialize().expect("native child instance boundary");
    let _held = lifecycle::admit_dcserver()
        .expect("actual native lease and executable admission")
        .expect("requested native test instance");
    assert!(lifecycle::has_admitted_run());
    assert!(cloexec(binary_fd));
    assert!(cloexec(lease_fd));
    let instance = current().unwrap().unwrap();
    assert!(
        matches!(
            std::fs::symlink_metadata(&instance.tmux.socket),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ),
        "the native HTTP fixture must have no tmux sessions to enumerate"
    );
    let capture_path = instance.root.join("runtime/b2a-child-capture.json");
    let mut capture = json!({
        "pid": std::process::id(), "argv": std::env::args().collect::<Vec<_>>(),
        "env": capture_environment(), "binary_fd": binary_fd,
        "binary_fd_cloexec": true, "lease_fd": lease_fd, "lease_fd_cloexec": true,
    });

    let mut config = crate::config::load().expect("instance's explicit canonical config");
    assert_eq!(config.server.host, "127.0.0.1");
    assert_eq!(config.database.host, "127.0.0.1");
    assert!(config.database.enabled);
    // 파일의 선언은 그대로 두고 child 메모리의 외부 연동과 leader worker만 끈다.
    config.cluster.enabled = true;
    config.cluster.role = crate::config::ClusterRole::Runner;
    config.cluster.runtime_profile = crate::config::RuntimeProfile::Runner;
    config.cluster.intake_routing.enabled = true;
    config.cluster.intake_routing.mode = crate::config::ClusterIntakeRoutingMode::Observe;
    config.cluster.intake_routing.capacity_aware = false;
    config
        .cluster
        .intake_routing
        .owner_authority_channel_ids
        .clear();
    config.discord.bots.clear();
    config.tui_o = None;
    config.agents.clear();
    config.routines.enabled = false;
    config.github.sync_interval_minutes = 0;
    config.mcp.watch_credentials = false;
    config.config_hot_reload = false;
    config.session_hosts = Default::default();
    config.database.pool_max = 1;
    config.policies.dir = instance.root.join("runtime/native-empty-policies");
    config.policies.hot_reload = false;
    std::fs::create_dir_all(&config.policies.dir).unwrap();
    for provider in config.providers.values_mut() {
        provider.runtime = Some("pipe".into());
        provider.tui_hosting = Some(false);
    }
    crate::config::validate_config(&config).expect("isolated runner config");
    crate::config_live_reload::install(config.clone());
    let registry = std::env::var_os("I3A_B2A_NATIVE_REGISTRY")
        .map(|_| Arc::new(crate::services::discord::health::HealthRegistry::new()));
    capture["registry"] = json!(registry.is_some());

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let pool = crate::db::postgres::connect_and_migrate(&config)
                .await
                .expect("B1 keeper's private PostgreSQL")
                .expect("private PostgreSQL is mandatory");
            let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
                .fetch_one(&pool)
                .await
                .expect("fresh private session census");
            assert_eq!(sessions, 0, "no recovered process may enter this fixture");
            let engine = crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone()))
                .expect("empty native policy engine");
            tokio::spawn(async move {
                for _ in 0..1200 {
                    if let Ok(Some(witness)) = lifecycle::health_witness() {
                        capture["port"] = witness["port"].clone();
                        capture["bound_pid"] = witness["pid"].clone();
                        let mut file = std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .mode(0o600)
                            .open(&capture_path)
                            .expect("fresh private native capture");
                        std::io::Write::write_all(
                            &mut file,
                            &serde_json::to_vec(&capture).unwrap(),
                        )
                        .unwrap();
                        file.sync_all().unwrap();
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                panic!("production server never published its actual bound witness");
            });
            crate::server::run(config, engine, registry, Some(pool))
                .await
                .expect("production native HTTP server");
            panic!("native HTTP child returned before its original Popen was stopped");
        });
}
