use super::tests::fixture;
use super::*;
use crate::services::discord::turn_bridge::recovery_text;

#[tokio::test]
async fn production_started_at_receipt_settles_borrowed_wip_pg() {
    let (root, mut state) = fixture("exact provider prompt");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "production_receipt",
        "production timestamp prelude settlement",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let provider = ProviderKind::Claude;
    recovery_text::store_turn_end_wip_warning(Some(&pool), &provider, state.channel_id, "borrowed")
        .unwrap();
    let mut context = state.retry_prompt_snapshot().unwrap();
    context.selected.wip = Some("borrowed".into());
    state.followup_reply_context = context.encode();
    let record = serde_json::json!({"type":"user", "timestamp":chrono::Utc::now().to_rfc3339(), "message":{"content":"exact provider prompt"}});
    std::fs::write(state.output_path.as_ref().unwrap(), format!("{record}\n")).unwrap();
    assert_eq!(
        input_receipt(&state),
        Ok(true),
        "PRODUCTION_STARTED_AT_RECEIPT"
    );
    assert!(
        settle(Some(&pool), &provider, &state, "confirmed-turn").await,
        "PRODUCTION_STARTED_AT_RECEIPT"
    );
    assert_eq!(
        peek(&pool, &provider, state.channel_id).await,
        None,
        "PRODUCTION_STARTED_AT_RECEIPT"
    );
    pool.close().await;
    db.drop().await;
}

async fn peek(pool: &sqlx::PgPool, provider: &ProviderKind, channel: u64) -> Option<String> {
    sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1")
        .bind(format!(
            "turn_end_wip_warning:{}:{channel}",
            provider.as_str()
        ))
        .fetch_optional(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn indeterminate_settlement_consumes_but_negative_receipt_retains_wip_pg() {
    let (root, state) = fixture("exact provider prompt");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "receipt_fallback",
        "indeterminate and negative settlement differ",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let provider = ProviderKind::Claude;
    for mut changed in super::receipt_fallback_tests::indeterminate_cases(&state) {
        recovery_text::store_turn_end_wip_warning(
            Some(&pool),
            &provider,
            state.channel_id,
            "borrowed",
        )
        .unwrap();
        let mut context = changed.retry_prompt_snapshot().unwrap();
        context.selected.wip = Some("borrowed".into());
        changed.followup_reply_context = context.encode();
        assert!(
            settle(Some(&pool), &provider, &changed, "unknown-receipt").await,
            "INDETERMINATE_SETTLEMENT_USES_NORMAL_COMPLETION"
        );
        assert_eq!(
            peek(&pool, &provider, state.channel_id).await,
            None,
            "INDETERMINATE_SETTLEMENT_CONSUMES_BORROWED_WIP"
        );
    }
    let mut negative = state.clone();
    recovery_text::store_turn_end_wip_warning(Some(&pool), &provider, state.channel_id, "borrowed")
        .unwrap();
    let mut context = negative.retry_prompt_snapshot().unwrap();
    context.selected.wip = Some("borrowed".into());
    negative.followup_reply_context = context.encode();
    let wrong = serde_json::json!({"type":"user", "timestamp":chrono::Utc::now().to_rfc3339(), "message":{"content":"different input"}});
    std::fs::write(negative.output_path.as_ref().unwrap(), format!("{wrong}\n")).unwrap();
    assert!(
        !settle(Some(&pool), &provider, &negative, "negative-receipt").await,
        "NEGATIVE_RECEIPT_RETAINS_BORROWED_WIP"
    );
    assert_eq!(
        peek(&pool, &provider, state.channel_id).await.as_deref(),
        Some("borrowed"),
        "NEGATIVE_RECEIPT_RETAINS_BORROWED_WIP"
    );
    negative.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
    let normalized = serde_json::json!({"type":"assistant", "message":{"content":[{"type":"text", "text":"done"}]}});
    std::fs::write(
        negative.output_path.as_ref().unwrap(),
        format!("{normalized}\n"),
    )
    .unwrap();
    assert!(
        settle(Some(&pool), &provider, &negative, "normalized-completion").await,
        "CODEX_NORMALIZED_RELAY_COMPLETION_FALLBACK"
    );
    assert_eq!(
        peek(&pool, &provider, state.channel_id).await,
        None,
        "CODEX_NORMALIZED_RELAY_CONSUMES_BORROWED_WIP"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn snapshotless_tui_normal_completion_allows_next_reminder_generation_pg() {
    let (root, mut state) = fixture("direct or headless input");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "snapshotless_tui",
        "normal completion preserves reminder generation",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let provider = ProviderKind::Claude;
    state.followup_reply_context = None;
    for runtime in [RuntimeHandoffKind::ClaudeTui, RuntimeHandoffKind::CodexTui] {
        state.runtime_kind = Some(runtime);
        assert!(
            settle(Some(&pool), &provider, &state, "snapshotless-completion").await,
            "SNAPSHOTLESS_TUI_REMINDER_GATE_OPEN"
        );
    }
    pool.close().await;
    db.drop().await;
}
