//! Settings PG round trip through the root CLI runtime-config payload, moved out of KRX services.

use crate::services::settings::tests::TestDatabase;
use crate::services::settings::{SettingsService, seed_runtime_config_defaults_pg};
use serde_json::json;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_pg_put_get_set_round_trip_survives_startup_seed() {
    let database = TestDatabase::create().await;
    let pool = database.connect().await;
    let mut config = crate::config::Config::default();
    config.runtime.max_entry_retries = Some(3);
    let service = SettingsService::new(Some(pool.clone()), Arc::new(config.clone()));
    let cli_put = crate::cli::client::runtime_config_payload(json!({
        "maxEntryRetries": 7
    }))
    .expect("normalize CLI PUT body");

    service
        .put_runtime_config(cli_put)
        .await
        .expect("write metadata-less CLI runtime config");
    let cli_get = serde_json::to_value(
        service
            .get_runtime_config()
            .await
            .expect("GET runtime config after CLI PUT"),
    )
    .expect("serialize runtime config response");
    let cli_round_trip = crate::cli::client::runtime_config_payload(cli_get)
        .expect("preserve explicit keys in CLI GET-to-SET payload");
    service
        .put_runtime_config(cli_round_trip)
        .await
        .expect("write CLI GET-to-SET runtime config");
    seed_runtime_config_defaults_pg(&pool, &config)
        .await
        .expect("run startup runtime-config seed");

    let seeded = service
        .get_runtime_config()
        .await
        .expect("GET runtime config after startup seed");
    assert_eq!(seeded.current.get("maxEntryRetries"), Some(&json!(7)));
    assert_eq!(seeded.explicit_keys, vec!["maxEntryRetries"]);

    drop(service);
    pool.close().await;
    database.drop().await;
}
