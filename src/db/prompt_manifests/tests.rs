use super::{
    PromptContentVisibility, PromptManifestBuilder, estimate_tokens_from_chars,
    fetch_prompt_manifest, save_prompt_manifest,
};

struct PromptManifestPgDatabase {
    _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
    admin_url: String,
    database_name: String,
    database_url: String,
}

impl PromptManifestPgDatabase {
    async fn create() -> Option<Self> {
        let base = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE")
            .ok()
            .filter(|value| !value.trim().is_empty())?;
        let lifecycle = crate::db::postgres::lock_test_lifecycle();
        let base = base.trim().trim_end_matches('/').to_string();
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        let admin_url = format!("{base}/{admin_db}");
        let database_name = format!(
            "agentdesk_prompt_manifest_{}",
            uuid::Uuid::new_v4().simple()
        );
        let database_url = format!("{base}/{database_name}");
        crate::db::postgres::create_test_database(&admin_url, &database_name, "prompt manifest pg")
            .await
            .expect("create prompt manifest postgres test db");
        Some(Self {
            _lifecycle: lifecycle,
            admin_url,
            database_name,
            database_url,
        })
    }

    async fn migrate(&self) -> sqlx::PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(&self.database_url, "prompt manifest pg")
            .await
            .expect("connect + migrate prompt manifest postgres test db")
    }

    async fn drop(self) {
        crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            "prompt manifest pg",
        )
        .await
        .expect("drop prompt manifest postgres test db");
    }
}

#[test]
fn prompt_manifest_token_estimate_is_chars_div_four() {
    assert_eq!(estimate_tokens_from_chars(0), 0);
    assert_eq!(estimate_tokens_from_chars(3), 0);
    assert_eq!(estimate_tokens_from_chars(4), 1);
    assert_eq!(estimate_tokens_from_chars(17), 4);
}

#[test]
fn prompt_manifest_builder_separates_content_visibility() {
    let manifest = PromptManifestBuilder::new(" turn-1 ", " channel-1 ")
        .dispatch_id(" dispatch-1 ")
        .profile(" project-agentdesk ")
        .content_layer(
            "system",
            true,
            Some("prompt_builder"),
            Some("base prompt"),
            PromptContentVisibility::AdkProvided,
            "abcd",
        )
        .content_layer(
            "user",
            true,
            Some("discord"),
            Some("user message"),
            PromptContentVisibility::UserDerived,
            "sensitive user content",
        )
        .build()
        .expect("build manifest");

    assert_eq!(manifest.turn_id, "turn-1");
    assert_eq!(manifest.channel_id, "channel-1");
    assert_eq!(manifest.dispatch_id.as_deref(), Some("dispatch-1"));
    assert_eq!(manifest.profile.as_deref(), Some("project-agentdesk"));
    assert_eq!(manifest.layer_count, 2);
    assert_eq!(manifest.total_input_tokens_est, 6);
    assert_eq!(
        manifest.layers[0].content_visibility,
        PromptContentVisibility::AdkProvided
    );
    assert_eq!(manifest.layers[0].full_content.as_deref(), Some("abcd"));
    assert!(manifest.layers[0].redacted_preview.is_none());
    assert_eq!(
        manifest.layers[1].content_visibility,
        PromptContentVisibility::UserDerived
    );
    assert!(manifest.layers[1].full_content.is_none());
    assert_eq!(
        manifest.layers[1].redacted_preview.as_deref(),
        Some("sensitive user content")
    );
}

#[tokio::test]
async fn prompt_manifest_save_fetch_round_trip_pg() {
    let Some(test_db) = PromptManifestPgDatabase::create().await else {
        eprintln!("skipping prompt_manifest_save_fetch_round_trip_pg: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;

    let manifest = PromptManifestBuilder::new("turn-round-trip", "1499610614904131594")
        .dispatch_id("dispatch-1")
        .profile("project-agentdesk")
        .content_layer(
            "system",
            true,
            Some("prompt_builder"),
            Some("authoritative instructions"),
            PromptContentVisibility::AdkProvided,
            "system prompt content",
        )
        .content_layer(
            "user",
            true,
            Some("discord"),
            Some("latest user message"),
            PromptContentVisibility::UserDerived,
            "user supplied prompt content",
        )
        .build()
        .expect("build manifest");

    let manifest_id = save_prompt_manifest(Some(&pool), &manifest)
        .await
        .expect("save manifest")
        .expect("manifest id");
    let fetched = fetch_prompt_manifest(Some(&pool), "turn-round-trip")
        .await
        .expect("fetch manifest")
        .expect("manifest");

    assert_eq!(fetched.id, Some(manifest_id));
    assert_eq!(fetched.turn_id, "turn-round-trip");
    assert_eq!(fetched.channel_id, "1499610614904131594");
    assert_eq!(fetched.dispatch_id.as_deref(), Some("dispatch-1"));
    assert_eq!(fetched.profile.as_deref(), Some("project-agentdesk"));
    assert_eq!(fetched.layer_count, 2);
    assert_eq!(
        fetched.total_input_tokens_est,
        manifest.total_input_tokens_est
    );
    assert_eq!(fetched.layers.len(), 2);
    assert_eq!(
        fetched.layers[0].content_visibility,
        PromptContentVisibility::AdkProvided
    );
    assert!(fetched.layers[0].full_content.is_some());
    assert!(fetched.layers[0].redacted_preview.is_none());
    assert_eq!(
        fetched.layers[1].content_visibility,
        PromptContentVisibility::UserDerived
    );
    assert!(fetched.layers[1].full_content.is_none());
    assert!(fetched.layers[1].redacted_preview.is_some());

    crate::db::postgres::close_test_pool(pool, "prompt manifest pg")
        .await
        .expect("close test pool");
    test_db.drop().await;
}

#[tokio::test]
async fn manifest_storage_stats_counts_utf8_bytes_not_chars_pg() {
    let Some(test_db) = PromptManifestPgDatabase::create().await else {
        eprintln!(
            "skipping manifest_storage_stats_counts_utf8_bytes_not_chars_pg: postgres unavailable"
        );
        return;
    };
    let pool = test_db.migrate().await;

    let system_text = "한글🙂";
    let user_text = "요청🙂한글";
    let expected_stored_bytes = (system_text.len() + user_text.len()) as i64;
    let expected_chars = (system_text.chars().count() + user_text.chars().count()) as i64;
    assert!(
        expected_stored_bytes > expected_chars,
        "multibyte fixture must distinguish UTF-8 bytes from characters"
    );

    let manifest = PromptManifestBuilder::new("turn-utf8-storage-stats", "1499610614904131594")
        .content_layer(
            "system",
            true,
            Some("prompt_builder"),
            Some("utf8 system content"),
            PromptContentVisibility::AdkProvided,
            system_text,
        )
        .content_layer(
            "user",
            true,
            Some("discord"),
            Some("utf8 user content"),
            PromptContentVisibility::UserDerived,
            user_text,
        )
        .build()
        .expect("build manifest");

    save_prompt_manifest(Some(&pool), &manifest)
        .await
        .expect("save manifest");

    let cfg = crate::config::PromptManifestRetentionConfig {
        enabled: true,
        full_content_days: 30,
        per_layer_max_bytes_adk_provided: 0,
        per_layer_max_bytes_user_derived: 0,
    };
    let stats = super::manifest_storage_stats(&pool, &cfg)
        .await
        .expect("stats");

    assert_eq!(stats.layer_count, 2);
    assert_eq!(stats.total_stored_bytes, expected_stored_bytes);
    assert_eq!(stats.total_original_bytes, expected_stored_bytes);
    assert_ne!(stats.total_stored_bytes, expected_chars);

    crate::db::postgres::close_test_pool(pool, "prompt manifest pg")
        .await
        .expect("close test pool");
    test_db.drop().await;
}

#[test]
fn prompt_manifest_layer_truncates_adk_provided_at_byte_cap() {
    let cfg = crate::config::PromptManifestRetentionConfig {
        enabled: true,
        full_content_days: 30,
        // 64-byte cap for adk_provided.
        per_layer_max_bytes_adk_provided: 64,
        per_layer_max_bytes_user_derived: 0,
    };
    let body = "A".repeat(2_048);
    let layer = super::PromptManifestLayer::from_content_with_retention(
        "system",
        true,
        Some("prompt_builder"),
        Some("base"),
        PromptContentVisibility::AdkProvided,
        body.clone(),
        Some(&cfg),
    );

    assert!(layer.is_truncated, "layer should be flagged truncated");
    assert_eq!(
        layer.original_bytes,
        Some(body.len() as i64),
        "original_bytes must reflect the pre-truncation size"
    );
    let stored = layer.full_content.as_deref().unwrap();
    assert!(stored.len() <= 64, "stored body must fit byte cap");
    assert!(stored.ends_with("[truncated by retention policy]"));

    // Hash MUST match the original (full) content, never the truncated body.
    let expected_hash = super::sha256_hex(&body);
    assert_eq!(layer.content_sha256, expected_hash);
}

#[test]
fn prompt_manifest_layer_truncation_disabled_when_config_disabled() {
    let cfg = crate::config::PromptManifestRetentionConfig {
        enabled: false,
        full_content_days: 30,
        per_layer_max_bytes_adk_provided: 8,
        per_layer_max_bytes_user_derived: 8,
    };
    let body = "A".repeat(1_024);
    let layer = super::PromptManifestLayer::from_content_with_retention(
        "system",
        true,
        None::<&str>,
        None::<&str>,
        PromptContentVisibility::AdkProvided,
        body.clone(),
        Some(&cfg),
    );
    assert!(!layer.is_truncated);
    assert_eq!(layer.full_content.as_deref(), Some(body.as_str()));
}

#[test]
fn prompt_manifest_layer_zero_cap_disables_truncation_for_visibility() {
    let cfg = crate::config::PromptManifestRetentionConfig {
        enabled: true,
        full_content_days: 30,
        per_layer_max_bytes_adk_provided: 0, // disabled
        per_layer_max_bytes_user_derived: 0,
    };
    let body = "A".repeat(1_024);
    let layer = super::PromptManifestLayer::from_content_with_retention(
        "system",
        true,
        None::<&str>,
        None::<&str>,
        PromptContentVisibility::AdkProvided,
        body.clone(),
        Some(&cfg),
    );
    assert!(!layer.is_truncated);
    assert_eq!(layer.full_content.as_deref(), Some(body.as_str()));
}

#[test]
fn prompt_manifest_layer_truncation_handles_utf8_boundary() {
    let cfg = crate::config::PromptManifestRetentionConfig {
        enabled: true,
        full_content_days: 30,
        per_layer_max_bytes_adk_provided: 64,
        per_layer_max_bytes_user_derived: 0,
    };
    // Mix of multi-byte chars to ensure we never split a codepoint.
    let body: String = std::iter::repeat("한").take(200).collect();
    let layer = super::PromptManifestLayer::from_content_with_retention(
        "system",
        true,
        None::<&str>,
        None::<&str>,
        PromptContentVisibility::AdkProvided,
        body.clone(),
        Some(&cfg),
    );
    let stored = layer.full_content.as_deref().unwrap();
    // Must be valid UTF-8 (would panic on `as_str()` otherwise).
    let _: &str = stored;
    assert!(layer.is_truncated);
    assert!(stored.len() <= 64);
}

#[tokio::test]
async fn prompt_manifest_save_applies_write_time_cap_via_global_pg() {
    use sqlx::Row;
    let Some(test_db) = PromptManifestPgDatabase::create().await else {
        eprintln!(
            "skipping prompt_manifest_save_applies_write_time_cap_via_global_pg: postgres unavailable"
        );
        return;
    };
    let pool = test_db.migrate().await;

    // Install a tight global cap so the next save trips it.
    super::install_retention_config(crate::config::PromptManifestRetentionConfig {
        enabled: true,
        full_content_days: 30,
        per_layer_max_bytes_adk_provided: 64,
        per_layer_max_bytes_user_derived: 0,
    });

    let big_body = "B".repeat(8_192);
    let manifest = PromptManifestBuilder::new("turn-write-cap", "1499610614904131594")
        .content_layer(
            "system",
            true,
            Some("prompt_builder"),
            Some("base"),
            PromptContentVisibility::AdkProvided,
            big_body.clone(),
        )
        .build()
        .expect("build manifest");
    save_prompt_manifest(Some(&pool), &manifest)
        .await
        .expect("save manifest");

    let row = sqlx::query(
        "SELECT full_content, is_truncated, content_sha256, original_bytes \
         FROM prompt_manifest_layers LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("fetch saved layer");
    let stored: Option<String> = row.try_get("full_content").unwrap_or(None);
    let is_truncated: bool = row.try_get("is_truncated").unwrap_or(false);
    let content_sha256: String = row.try_get("content_sha256").unwrap_or_default();
    let original_bytes: Option<i64> = row.try_get("original_bytes").ok().flatten();

    let stored = stored.expect("body stored");
    assert!(stored.len() <= 64, "saved body must fit byte cap");
    assert!(is_truncated, "is_truncated must be set when capped");
    assert_eq!(content_sha256, super::sha256_hex(&big_body));
    assert_eq!(
        original_bytes,
        Some(big_body.len() as i64),
        "original_bytes must reflect the pre-truncation length"
    );

    crate::db::postgres::close_test_pool(pool, "prompt manifest pg")
        .await
        .expect("close test pool");
    test_db.drop().await;
}

#[tokio::test]
async fn prompt_manifest_apply_retention_trims_old_full_content_pg() {
    use sqlx::Row;
    let Some(test_db) = PromptManifestPgDatabase::create().await else {
        eprintln!(
            "skipping prompt_manifest_apply_retention_trims_old_full_content_pg: postgres unavailable"
        );
        return;
    };
    let pool = test_db.migrate().await;

    // Seed a manifest with full content, then back-date it past the horizon.
    let manifest = PromptManifestBuilder::new("turn-retention", "1499610614904131594")
        .content_layer(
            "system",
            true,
            Some("prompt_builder"),
            Some("base"),
            PromptContentVisibility::AdkProvided,
            "old-system-content",
        )
        .build()
        .expect("build manifest");
    save_prompt_manifest(Some(&pool), &manifest)
        .await
        .expect("save manifest");

    // Back-date by 60 days.
    sqlx::query("UPDATE prompt_manifests SET created_at = NOW() - INTERVAL '60 days'")
        .execute(&pool)
        .await
        .expect("backdate manifest");

    let cfg = crate::config::PromptManifestRetentionConfig {
        enabled: true,
        full_content_days: 30,
        per_layer_max_bytes_adk_provided: 0,
        per_layer_max_bytes_user_derived: 0,
    };
    let report = super::apply_retention_policy(&pool, &cfg, false)
        .await
        .expect("apply retention policy");
    assert_eq!(report.trimmed_full_content, 1);

    // Verify hash is preserved, full_content is NULL, is_truncated set.
    let row = sqlx::query(
        "SELECT full_content, is_truncated, content_sha256 \
         FROM prompt_manifest_layers LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("fetch trimmed layer");
    let full_content: Option<String> = row.try_get("full_content").unwrap_or(None);
    let is_truncated: bool = row.try_get("is_truncated").unwrap_or(false);
    let content_sha256: String = row.try_get("content_sha256").unwrap_or_default();
    assert!(full_content.is_none(), "full_content must be NULL");
    assert!(is_truncated, "is_truncated must be TRUE");
    assert_eq!(content_sha256, super::sha256_hex("old-system-content"));

    // Stats should report at least one layer + zero remaining full_content rows.
    let stats = super::manifest_storage_stats(&pool, &cfg)
        .await
        .expect("stats");
    assert!(stats.layer_count >= 1);
    assert_eq!(
        stats.oldest_full_content_at, None,
        "no rows should still have full_content"
    );
    assert_eq!(stats.retention_days, 30);
    assert!(stats.retention_horizon_at.is_some());
    assert!(stats.restart_required_for_config_changes);
    assert_eq!(stats.config_applied_at, "boot");
    assert_eq!(stats.config_source, "agentdesk.yaml boot snapshot");
    assert!(!stats.hot_reload);

    crate::db::postgres::close_test_pool(pool, "prompt manifest pg")
        .await
        .expect("close test pool");
    test_db.drop().await;
}
