use std::sync::Arc;

use super::super::tests::health_body;
use crate::services::discord::health::HealthRegistry;

#[tokio::test]
async fn test_instance_normal_registry_and_standalone_keep_the_public_health_contract() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for registry in [None, Some(Arc::new(HealthRegistry::new()))] {
        let public = health_body("/health", registry.clone()).await;
        let detail = health_body("/health/detail", registry).await;
        assert!(public.get("test_instance").is_none(), "{public}");
        assert!(detail.get("test_instance").is_none(), "{detail}");
        assert!(public.get("boot_incarnation").is_none(), "{public}");
        assert!(public.get("legacy_body_census").is_none(), "{public}");
        assert!(public.get("status").is_some());
        assert!(detail.get("status").is_some());
    }
}

#[test]
fn test_instance_response_discards_unverified_blocks_and_refuses_a_failed_scope() {
    let normal = serde_json::json!({"status":"healthy","ok":true,"fully_recovered":true,
        "degraded_reasons":[],"test_instance":{"token":"synthetic-secret"}});
    let mut legacy = normal.clone();
    assert!(super::attach_test_instance(&mut legacy, Ok(None)));
    assert!(legacy.get("test_instance").is_none());
    assert_eq!(legacy["status"], "healthy");
    let mut refused = normal;
    assert!(!super::attach_test_instance(
        &mut refused,
        Err("synthetic-secret".into())
    ));
    assert!(refused.get("test_instance").is_none());
    assert_eq!(refused["status"], "unhealthy");
    assert_eq!(refused["ok"], false);
    assert_eq!(refused["fully_recovered"], false);
    assert_eq!(
        refused["degraded_reasons"],
        serde_json::json!(["test_instance_unverified"])
    );
    assert!(!refused.to_string().contains("synthetic-secret"));
}
