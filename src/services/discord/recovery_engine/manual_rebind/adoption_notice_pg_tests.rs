use super::*;
use crate::services::provider::ProviderKind;

/// A fence-forward takes custody and then announces it: the operator notice must land as one
/// outbox row, not stop at the source registry with only a WARN.
#[tokio::test]
async fn fence_forward_notice_lands_one_outbox_row_pg() {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_adopt_fence_notice",
        "adopt fence-forward notice outbox",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let channel_id = 6_304_000_001_u64;
    let tmp = tempfile::tempdir().expect("tempdir");
    let transcript = tmp
        .path()
        .join("61590000-0000-4000-8000-000000000000.jsonl");
    let mut bytes = vec![b'x'; 4_095];
    bytes.push(b'\n');
    bytes.extend_from_slice(b"{\"type\":\"assistant\",\"partial\":\"UNREAD_6304\"}");
    std::fs::write(&transcript, &bytes).expect("write transcript");
    let path = transcript.to_str().expect("utf8 path");
    let tmux = "AgentDesk-claude-6304-cc";
    let mut row = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        channel_id,
        None,
        crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID,
        6_304_000_002,
        6_304_000_003,
        "tui prompt".to_string(),
        None,
        Some(tmux.to_string()),
        Some(path.to_string()),
        None,
        4_096,
    );
    row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    row.turn_source = inflight::TurnSource::ExternalInput;
    row.turn_start_offset = Some(4_096);
    let facts = AdoptFenceForward {
        cause: AdoptFenceForwardCause::TurnIdentityUnknown,
        existing: &row,
        tmux_session_name: tmux,
        output_path: path,
        initial_offset: bytes.len() as u64,
        latest_lease_turn_id: None,
    };
    let custody = take_adopt_fence_forward_custody(Some(&pool), channel_id, &facts)
        .await
        .expect("custody");
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));

    announce_adopt_fence_forward(&shared, &ProviderKind::Claude, channel_id, tmux, custody);

    // The enqueue runs on a spawned task; wait for it instead of racing it.
    let target = format!("channel:{channel_id}");
    let mut rows = Vec::new();
    for _ in 0..100 {
        rows = sqlx::query_as::<_, (String, String, Option<String>, String)>(
            "SELECT source, bot, reason_code, content FROM message_outbox WHERE target = $1",
        )
        .bind(&target)
        .fetch_all(&pool)
        .await
        .expect("read outbox");
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(rows.len(), 1, "fence-forward notice must reach the outbox");
    let (source, bot, reason_code, content) = &rows[0];
    assert_eq!(source, "adopt_fence_forward_notice");
    assert_eq!(
        bot,
        crate::services::discord::bot_role::UtilityBotRole::Notify.alias()
    );
    assert_eq!(reason_code.as_deref(), Some("adopt.fence_forward"));
    assert!(content.contains("응답 이어받기 실패"), "{content}");
    pool.close().await;
    db.drop().await;
}
