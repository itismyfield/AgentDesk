use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::cluster::attachment_transfer::{self as assets, uploads::Upload};

#[tokio::test]
async fn admission_failure_keeps_consumed_voice_assets_and_ack_custody_pg() {
    let _root = TestRuntimeRootGuard::new();
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "admission_custody",
        "actual mailbox admission",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(727_372));
    let path = fixture_path(727372);
    handle
        .v2_adopt(path.clone(), "admission-pg".into())
        .await
        .unwrap();
    let mut original = original(8_000_000_000_000_001_200, "voice original\n_ WIP");
    let voice: crate::voice::prompt::VoiceTranscriptAnnouncement = serde_json::from_value(json!({
        "transcript":"voice original", "user_id":"42", "utterance_id":"custody", "language":"ko-KR",
        "verbose_progress":true, "started_at":null, "completed_at":null, "samples_written":null,
        "control_channel_id":null, "stt_mode":null, "stt_latency_ms":null }))
    .unwrap();
    let message = poise::serenity_prelude::MessageId::new(original.message_id);
    crate::voice::announce_meta::persist_voice_announcement_reservation_durable(
        &pool,
        "custody-pending",
        ChannelId::new(727_372),
        "voice",
        &voice,
    )
    .await
    .unwrap();
    assert!(
        crate::voice::announce_meta::bind_voice_announcement_durable_message_id(
            &pool,
            "custody-pending",
            message
        )
        .await
        .unwrap()
    );
    assert!(
        crate::voice::announce_meta::mark_voice_announcement_durable_consumed(&pool, message)
            .await
            .unwrap()
    );
    let identity = assets::AttachmentMessageIdentity {
        provider: "claude".into(),
        channel_id: "727372".into(),
        user_msg_id: message.to_string(),
    };
    let bytes = b"attachment survives admission error".to_vec();
    let bundle = assets::validate_attachment_bundle_v1(
        assets::AttachmentBundleV1 {
            version: 1,
            identity: identity.clone(),
            source_attachment_count: 1,
            entries: vec![assets::AttachmentEntryV1 {
                filename: "custody.txt".into(),
                sha256: assets::attachment_sha256_hex(&bytes),
                bytes: bytes.clone(),
            }],
        },
        &identity,
    )
    .unwrap();
    let reference = assets::store::put(&pool, &bundle).await.unwrap();
    original.pending_uploads = vec![Upload::Bundle(reference.clone())];
    original.voice_announcement = Some(voice.clone());
    let saved = serde_json::to_value(&original).unwrap();
    let custody = Arc::new(Mutex::new(Custody::new(&original)));
    handle
        .v2_request(|s| {
            s.as_mut().unwrap().fault = Fault::Before;
            Ok(())
        })
        .await
        .unwrap();
    let mut accepted_ack = 0;
    let failed = handle.v2_admit(custody.clone()).await;
    accepted_ack += usize::from(failed == Ok(Admitted::Queued));
    assert!(matches!(failed, Err(AdmitError::NotCommitted(_))));
    assert_eq!(
        accepted_ack, 0,
        "failed admission must issue zero accepted acknowledgements"
    );
    handle
        .v2_request(|s| {
            s.as_mut().unwrap().fault = Fault::None;
            Ok(())
        })
        .await
        .unwrap();
    let watch = Watch::new(
        &path,
        Plan {
            fail: vec![Step::ParentSync],
            ..Default::default()
        },
    );
    let pending = handle.v2_admit(custody.clone()).await;
    accepted_ack += usize::from(pending == Ok(Admitted::Queued));
    assert_eq!(pending, Err(AdmitError::Pending));
    assert_eq!(
        accepted_ack, 0,
        "uncertain publication cannot advance the caller checkpoint"
    );
    assert_eq!(
        serde_json::to_value(&custody.lock().unwrap().original).unwrap(),
        saved
    );
    assert_eq!(
        assets::store::load(&pool, &reference)
            .await
            .unwrap()
            .as_bundle(),
        bundle.as_bundle()
    );
    assert_eq!(
        crate::voice::announce_meta::load_consumed_voice_announcement_durable(&pool, message)
            .await
            .unwrap()
            .unwrap()
            .utterance_id,
        voice.utterance_id
    );
    drop(watch);
    handle
        .v2_request(|s| s.as_mut().unwrap().recover())
        .await
        .unwrap();
    for _ in 0..2 {
        accepted_ack += usize::from(handle.v2_admit(custody.clone()).await == Ok(Admitted::Queued));
    }
    assert_eq!(accepted_ack, 1);
    assert_eq!(
        handle
            .v2_request(|s| Ok(s.as_ref().unwrap().snapshot.items.len()))
            .await
            .unwrap(),
        1,
        "recovered synthetic admission must not append the original again"
    );
    let disk: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(disk["items"][0], saved);
    let mixed = registry.handle(ChannelId::new(727_376));
    mixed
        .v2_adopt(fixture_path(727376), "mixed-pg".into())
        .await
        .unwrap();
    mixed.v2_request(|s| {
        let owner = s.as_mut().unwrap();
        owner.enqueue(&super::original(100, "known A"))?;
        let attempt: AttemptIdentity = serde_json::from_value(json!({
            "nonce":"known-A", "provider":"claude", "target":{"host":"tmux","owner":"mixed-pg","root":"/test","execution_nonce":"execution"},
            "source":{"kind":"existing","session_id":"session","transcript":"/test/transcript","file":[1,2],"offset":0}
        })).unwrap();
        let key = owner.reserve(attempt, PreparedPayload { frame:"known A".into(), digest:"A".into(), attribution:json!({}) })?;
        drop(owner.arm(&key, 1000)?.consume(&key)?);
        owner.settle(&key, true, 1001)
    }).await.unwrap();
    assert_eq!(
        mixed
            .v2_admit(Arc::new(Mutex::new(Custody::new(&super::original(
                100, "known A"
            )))))
            .await,
        Ok(Admitted::Duplicate)
    );
    let mut envelope = super::original(100, "known A\nfresh B\n_ literal");
    envelope.source_message_ids = vec![100, 200];
    envelope.source_text_segments = serde_json::from_value(json!([
        {"message_id":100,"text":"known A"},{"message_id":200,"text":"fresh B\n_ literal"}]))
    .unwrap();
    assert_eq!(
        mixed
            .v2_admit(Arc::new(Mutex::new(Custody::new(&envelope))))
            .await,
        Ok(Admitted::Queued)
    );
    mixed.v2_request(|s| {
        let owner = s.as_mut().unwrap();
        assert_eq!(owner.snapshot.items[0].message_id, 200);
        let attempt: AttemptIdentity = serde_json::from_value(json!({
            "nonce":"fresh-B", "provider":"claude", "target":{"host":"tmux","owner":"mixed-pg","root":"/test","execution_nonce":"execution"},
            "source":{"kind":"existing","session_id":"session","transcript":"/test/transcript","file":[1,2],"offset":0}
        })).unwrap();
        owner.reserve(attempt, PreparedPayload { frame:owner.snapshot.items[0].text.clone(), digest:"B".into(), attribution:json!({}) })?;
        Ok(())
    }).await.unwrap();
    let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = effects.clone();
    assert!(
        mixed
            .v2_effect(2000, move |frame| {
                assert_eq!(frame, "fresh B\n_ literal");
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(true)
            })
            .await
            .unwrap()
    );
    assert_eq!(
        effects.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "known A adds zero effects; fresh B is delivered once"
    );
    pool.close().await;
    db.drop().await;
}
