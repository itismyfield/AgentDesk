use super::*;
use crate::services::cluster::attachment_transfer::{self as assets, uploads::Upload};

fn records_bytes(records: &[String]) -> Vec<Vec<u8>> {
    records
        .iter()
        .map(|record| {
            let path = record
                .split_once(" → ")
                .unwrap()
                .1
                .rsplit_once(" (")
                .unwrap()
                .0;
            std::fs::read(path)
                .expect("retained files must remain readable after preparation returns")
        })
        .collect()
}

#[tokio::test]
async fn normal_and_busy_preparation_preserve_attachment_bytes_and_retention_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "shared_payload_files",
        "real attachment preparation",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    crate::services::discord::turn_bridge::recovery_text::store_turn_end_wip_warning(
        Some(&pool),
        &ProviderKind::Claude,
        727373,
        "dirty",
    )
    .unwrap();
    let raw_reply = "raw quote\r\n_ WIP literal";
    let (selected, rendered) = selected_reply(
        &shared,
        &ProviderKind::Claude,
        ChannelId::new(727373),
        RetryPromptContext::raw(Some(raw_reply.into())),
    )
    .await;
    assert_eq!(selected.raw.as_deref(), Some(raw_reply));
    assert_eq!(selected.selected.wip.as_deref(), Some("dirty"));
    assert_eq!(
        rendered.as_deref(),
        Some(
            "[WIP 리마인더 — 이전 턴 종료 시 커밋되지 않은 변경사항 감지]\n\ndirty\n\nraw quote\r\n_ WIP literal"
        )
    );
    assert_eq!(
        crate::services::discord::router::response_format::retry_prompt_context::store::peek(
            Some(&pool),
            &crate::services::discord::router::response_format::retry_prompt_context::store::key(
                "turn_end_wip_warning",
                &ProviderKind::Claude,
                727373
            )
        )
        .as_deref(),
        Some("dirty")
    );
    let identity = assets::AttachmentMessageIdentity {
        provider: "claude".into(),
        channel_id: "727373".into(),
        user_msg_id: "100".into(),
    };
    let bytes = vec![
        b"first attachment\r\n".to_vec(),
        "두 번째 첨부".as_bytes().to_vec(),
    ];
    let bundle = assets::validate_attachment_bundle_v1(
        assets::AttachmentBundleV1 {
            version: 1,
            identity: identity.clone(),
            source_attachment_count: 2,
            entries: bytes
                .iter()
                .enumerate()
                .map(|(i, b)| assets::AttachmentEntryV1 {
                    filename: format!("{i}.txt"),
                    sha256: assets::attachment_sha256_hex(b),
                    bytes: b.clone(),
                })
                .collect(),
        },
        &identity,
    )
    .unwrap();
    let reference = assets::store::put(&pool, &bundle).await.unwrap();
    let uploads = vec![Upload::Bundle(reference)];
    let prepared = prepare_uploads(&uploads, Some(&pool)).await.unwrap();
    assert_eq!(records_bytes(&prepared.records), bytes);
    let expected = bytes.clone();
    tokio::task::spawn_blocking(move || {
        let provider_lifetime = prepared;
        assert_eq!(
            records_bytes(&provider_lifetime.records),
            expected,
            "provider invocation owns temporary files after preparation returns"
        );
    })
    .await
    .unwrap();
    let retained = prepare_busy_uploads(ChannelId::new(727_373), &uploads, Some(&pool))
        .await
        .unwrap();
    assert_eq!(
        records_bytes(&retained),
        bytes,
        "busy return cannot release retained attachment bytes"
    );
    assert_eq!(
        records_bytes(&retained),
        bundle
            .as_bundle()
            .entries
            .iter()
            .map(|e| e.bytes.clone())
            .collect::<Vec<_>>()
    );
    pool.close().await;
    db.drop().await;
}
