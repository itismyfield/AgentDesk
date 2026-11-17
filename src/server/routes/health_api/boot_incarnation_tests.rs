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
