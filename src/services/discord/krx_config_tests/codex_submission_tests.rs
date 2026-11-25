//! Codex draft-notice PG test that checks discord dispatch provenance, moved out of KRX codex_tui.

use crate::services::codex_tui::input::{
    draft_notice_event_for_test as draft_notice_event,
    enqueue_draft_notice_for_test as enqueue_draft_notice,
};

#[tokio::test]
async fn codex_draft_notice_is_one_non_turn_outbox_event_pg() {
    crate::db::postgres::postgres_test_database_url_base()
        .expect("POSTGRES_TEST_DATABASE_URL_BASE required for the codex draft notice PG test"); // agentdesk-audit: allow-unwrap — test-only fixture: a missing or unreachable PostgreSQL fails the test instead of passing silently
    const CHANNEL: u64 = 7_676_150;
    const RESTART_DB: &str = "ADK_CODEX_DRAFT_NOTICE_TEST_DB";
    let event = |primary: u64, nonce: &str| {
        #[cfg(unix)]
        {
            use crate::services::claude_tui::owner_input;
            use std::sync::Arc;
            let target = serde_json::json!({"host":"herdr","execution_nonce":nonce,
                "_attempt":{"identity":{"primary":primary,"queued_generation":7,
                    "sources":[{"message_id":primary,"queued_generation":7}]},
                    "attempt":{"nonce":nonce}}});
            owner_input::with_scope(
                "codex".into(),
                target,
                CHANNEL,
                Arc::new(|_| Ok(())),
                Arc::new(|_, _| Ok(())),
                Arc::new(|_| Ok(())),
                || draft_notice_event(CHANNEL, owner_input::original_identity()).unwrap(),
            )
        }
        #[cfg(not(unix))]
        {
            let _ = nonce;
            draft_notice_event(CHANNEL, Some((primary, 7))).unwrap()
        }
    };
    assert!(crate::services::claude_tui::owner_input::original_identity().is_none());
    assert!(draft_notice_event(CHANNEL, None).is_none());
    if let Ok(url) = std::env::var(RESTART_DB) {
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        for original in [1001, 1002] {
            enqueue_draft_notice(&pool, CHANNEL, &event(original, "after-restart"))
                .await
                .unwrap();
        }
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM message_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count, 2,
            "a fresh process must reuse both original notice keys"
        );
        pool.close().await;
        return;
    }
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let first = enqueue_draft_notice(&pool, CHANNEL, &event(1001, "first-attempt"))
        .await
        .unwrap();
    let again = enqueue_draft_notice(&pool, CHANNEL, &event(1001, "next-attempt"))
        .await
        .unwrap();
    assert_eq!(
        first, again,
        "one original across changed attempts owes one notice"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let second = enqueue_draft_notice(&pool, CHANNEL, &event(1002, "next-attempt"))
        .await
        .unwrap();
    assert_ne!(
        first, second,
        "identical notice text for different originals stays distinct"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
    let text: String = sqlx::query_scalar("SELECT content FROM message_outbox WHERE id = $1")
        .bind(first)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(text.contains("초안의 보존 여부는 확인하지 못했습니다"));
    assert!(crate::services::discord::dispatch_policy::has_non_turn_provenance(&text));
    assert!(crate::services::turn_orchestrator::ChannelMailboxHandle::v2_notice_is_non_turn(&text));
    pool.close().await;
    let test = format!(
        "{}::codex_draft_notice_is_one_non_turn_outbox_event_pg",
        module_path!().split_once("::").unwrap().1
    );
    let restarted = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &test, "--nocapture"])
        .env(RESTART_DB, &db.database_url)
        .output()
        .unwrap();
    assert!(
        restarted.status.success()
            && String::from_utf8_lossy(&restarted.stdout).contains("1 passed"),
        "{}\n{}",
        String::from_utf8_lossy(&restarted.stdout),
        String::from_utf8_lossy(&restarted.stderr)
    );
    let pool = db.connect_and_migrate().await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
    pool.close().await;
    db.drop().await;
}
