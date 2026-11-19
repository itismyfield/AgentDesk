//! The per-process boot incarnation on both `health_response` arms.

use std::sync::Arc;

use super::super::tests::health_body;
use crate::services::discord::health::HealthRegistry;

#[test]
fn detail_health_carries_one_boot_incarnation_per_process() {
    if super::super::db_deadline_tests::resource_regression_tests::rerun_alone(concat!(
        module_path!(),
        "::detail_health_carries_one_boot_incarnation_per_process"
    )) {
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for registry in [Some(Arc::new(HealthRegistry::new())), None] {
                let first = health_body("/health/detail", registry.clone()).await;
                let again = health_body("/health/detail", registry.clone()).await;
                let incarnation = first["boot_incarnation"]
                    .as_str()
                    .unwrap_or_else(|| panic!("detail lacks boot_incarnation: {first}"));
                assert!(uuid::Uuid::parse_str(incarnation).is_ok(), "{incarnation}");
                assert_eq!(incarnation, super::boot_incarnation());
                assert_eq!(
                    incarnation,
                    crate::services::cluster::readiness::boot_id(),
                    "readiness and health must identify the same process"
                );
                assert_eq!(again["boot_incarnation"], first["boot_incarnation"]);

                let public = health_body("/health", registry.clone()).await;
                assert!(
                    public.get("boot_incarnation").is_none(),
                    "the public allowlist must not carry boot_incarnation: {public}"
                );
                assert!(public.get("legacy_body_census").is_none());
                assert_eq!(first["legacy_body_census"]["boot_id"], incarnation);
                assert_eq!(first["legacy_body_census"]["status"], "INCONCLUSIVE");
            }
        });
}

#[test]
fn m9_unwitnessed_health_omits_running_binary_on_both_paths() {
    if super::super::db_deadline_tests::resource_regression_tests::rerun_alone(concat!(
        module_path!(),
        "::m9_unwitnessed_health_omits_running_binary_on_both_paths"
    )) {
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for registry in [Some(Arc::new(HealthRegistry::new())), None] {
                let detail = health_body("/health/detail", registry.clone()).await;
                let public = health_body("/health", registry).await;
                assert!(public.get("running_binary").is_none());
                assert!(detail.get("running_binary").is_none());
            }
        });
}

#[cfg(target_os = "linux")]
#[test]
fn m9_running_health_reports_a_verified_release_pair() {
    use std::{env, fs, process::Command};

    const CHILD: &str = "ADK_TEST_RELEASE_PAIR_CHILD";
    let source = "a".repeat(40);
    let target = "x86_64-unknown-linux-gnu";
    if env::var_os(CHILD).is_none() {
        let executable = env::current_exe().unwrap();
        let release = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let binary = release.path().join("agentdesk");
        fs::hard_link(&executable, &binary).unwrap();
        let image = crate::services::session_host::policy::file::image(&binary).unwrap();
        fs::write(
            release.path().join("agentdesk.source.json"),
            serde_json::json!({
                "schema_version": 1,
                "source_sha": source,
                "target": target,
                "sha256": hex::encode(image.digest()),
            })
            .to_string(),
        )
        .unwrap();
        let name = concat!(
            module_path!(),
            "::m9_running_health_reports_a_verified_release_pair"
        );
        let output = Command::new(binary)
            .args(["--exact", name.split_once("::").unwrap().1])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    crate::services::running_binary::initialize();
    // Publication is pinned at startup; the service never rereads the pathname.
    let exe = env::current_exe().unwrap();
    fs::remove_file(exe.with_file_name("agentdesk.source.json")).unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for registry in [Some(Arc::new(HealthRegistry::new())), None] {
                let detail = health_body("/health/detail", registry.clone()).await;
                let public = health_body("/health", registry).await;
                let record = &detail["running_binary"];
                assert_eq!(record["source_sha"], source);
                assert_eq!(record["target"], target);
                assert!(record["observed_at"].is_string());
                assert_eq!(
                    record["boot_started_at_ms"],
                    crate::services::process_boot::initialize().started_at_ms
                );
                assert!(public.get("running_binary").is_none());
            }
        });
}
