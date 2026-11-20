use std::sync::Arc;

use serde_json::json;

use super::{
    SettingsService,
    tests::{TestDatabase, upsert_test_kv},
};

#[tokio::test]
async fn dashboard_v3_config_catalog_exposes_metadata_separately_from_values_pg() {
    let database = TestDatabase::create().await;
    let pool = database.connect().await;
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some("catalog-test-secret".into());
    let service = SettingsService::new(Some(pool.clone()), Arc::new(config));
    upsert_test_kv(&pool, "server.auth_token", "catalog-test-secret").await;
    upsert_test_kv(
        &pool,
        "runtime-config",
        r#"{"maxRetries":7,"legacyKey":true,"__runtimeConfigExplicitKeys":["maxRetries"]}"#,
    )
    .await;

    let config = serde_json::to_value(service.get_config_entries().await.unwrap()).unwrap();
    let runtime = serde_json::to_value(service.get_runtime_config().await.unwrap()).unwrap();
    database.drop().await;

    for entry in config["entries"].as_array().unwrap() {
        assert!(entry["category"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(
            entry["description_ko"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
    }
    assert!(!config.to_string().contains("catalog-test-secret"));
    assert!(!runtime.to_string().contains("catalog-test-secret"));
    assert_eq!(runtime["current"]["maxRetries"], 7);
    assert_eq!(runtime["defaults"]["maxRetries"], 3);
    assert_eq!(runtime["current"]["legacyKey"], true);
    assert_eq!(runtime["explicit_keys"], json!(["maxRetries"]));
    let metadata = runtime["key_metadata"].as_object().unwrap();
    let defaults = runtime["defaults"].as_object().unwrap();
    assert_eq!(metadata.len(), defaults.len());
    assert!(defaults.keys().all(|key| metadata.contains_key(key)));
    assert!(
        metadata
            .values()
            .all(|item| item["label_ko"].as_str().is_some_and(|s| !s.is_empty()))
    );
}
