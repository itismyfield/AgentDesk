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
fn m9_detail_health_reports_the_compiled_source_on_both_paths() {
    if super::super::db_deadline_tests::resource_regression_tests::rerun_alone(concat!(
        module_path!(),
        "::m9_detail_health_reports_the_compiled_source_on_both_paths"
    )) {
        return;
    }
    let expected = std::env::var("ADK_TEST_RUNNING_SOURCE_SHA")
        .ok()
        .or_else(|| {
            option_env!("ADK_RUNNING_SOURCE_SHA")
                .filter(|sha| !sha.is_empty())
                .map(str::to_owned)
        });
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for registry in [Some(Arc::new(HealthRegistry::new())), None] {
                let detail = health_body("/health/detail", registry.clone()).await;
                let public = health_body("/health", registry).await;
                assert!(public.get("running_binary").is_none());
                if let Some(sha) = &expected {
                    let witness = &detail["running_binary"];
                    assert_eq!(witness["source_sha"].as_str(), Some(sha.as_str()));
                    assert!(witness["target"].as_str().is_some_and(|s| !s.is_empty()));
                    assert_eq!(
                        witness["boot_started_at_ms"],
                        crate::services::process_boot::initialize().started_at_ms
                    );
                    assert!(witness["observed_at"].as_str().is_some());
                } else {
                    assert!(detail.get("running_binary").is_none());
                }
            }
        });
}
