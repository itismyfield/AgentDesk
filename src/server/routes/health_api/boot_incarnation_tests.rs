//! The per-process boot incarnation on both `health_response` arms.

use std::sync::Arc;

use super::super::tests::health_body;
use crate::services::discord::health::HealthRegistry;

#[tokio::test]
async fn detail_health_carries_one_boot_incarnation_per_process() {
    for registry in [Some(Arc::new(HealthRegistry::new())), None] {
        let first = health_body("/health/detail", registry.clone()).await;
        let again = health_body("/health/detail", registry.clone()).await;
        let incarnation = first["boot_incarnation"]
            .as_str()
            .unwrap_or_else(|| panic!("detail lacks boot_incarnation: {first}"));
        assert!(uuid::Uuid::parse_str(incarnation).is_ok(), "{incarnation}");
        assert_eq!(incarnation, super::boot_incarnation());
        assert_eq!(again["boot_incarnation"], first["boot_incarnation"]);

        let public = health_body("/health", registry.clone()).await;
        assert!(
            public.get("boot_incarnation").is_none(),
            "the public allowlist must not carry boot_incarnation: {public}"
        );
    }
}
