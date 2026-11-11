use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use poise::serenity_prelude::ChannelId;
use std::path::PathBuf;

fn failure() -> OwnedRecoveryFailure {
    OwnedRecoveryFailure {
        provider: "codex".into(),
        channel: ChannelId::new(8_057_301),
        expected_path: PathBuf::from("/fixture/codex/account/8057301.json"),
        namespace: "codex/account".into(),
        stage: "catalog_unreadable",
        error: "read denied".into(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_pg_is_storage_hold_instead_of_successful_notice() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pass = RecoveryPass::new();
    let failed = failure();
    assert!(catalog(None, &pass, &failed).await.is_err());
    assert!(
        cleanup(
            None,
            ("codex", 8_057_301, "codex/account"),
            ("cleanup", "intent")
        )
        .await
        .is_err()
    );
    assert_eq!(failed.error, "read denied");
}

#[tokio::test(flavor = "current_thread")]
async fn catalog_pass_and_cleanup_stage_notices_are_persistently_deduplicated_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let pass = RecoveryPass::new();
    let mut failed = failure();
    let first = catalog(Some(&pool), &pass, &failed).await.unwrap();
    failed.error = "invalid UTF-8".into();
    assert_eq!(catalog(Some(&pool), &pass, &failed).await.unwrap(), first);
    assert_ne!(
        catalog(Some(&pool), &RecoveryPass::new(), &failed)
            .await
            .unwrap(),
        first
    );
    let at = ("codex", failed.channel.get(), "codex/account");
    let cleanup_first = cleanup(Some(&pool), at, ("cleanup-1", "receipt"))
        .await
        .unwrap();
    assert_eq!(
        cleanup(Some(&pool), at, ("cleanup-1", "receipt"))
            .await
            .unwrap(),
        cleanup_first
    );
    assert_ne!(
        cleanup(Some(&pool), at, ("cleanup-1", "outbox"))
            .await
            .unwrap(),
        cleanup_first
    );
    let messages: Vec<(String, String)> = sqlx::query_as(
        "SELECT content, session_key FROM message_outbox WHERE target = $1 ORDER BY id",
    )
    .bind(format!("channel:{}", failed.channel))
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(messages.len(), 4);
    assert!(messages.iter().all(|(text, _)| {
        crate::services::discord::dispatch_policy::has_non_turn_provenance(text)
    }));
    assert!(
        messages
            .iter()
            .all(|(text, _)| !text.contains("invalid UTF-8"))
    );
    assert!(
        messages
            .iter()
            .any(|(_, key)| key.starts_with("boot-queue-io:v1:"))
    );
    assert!(messages.iter().any(|(_, key)| key.starts_with("A-io:v1:")));
}
