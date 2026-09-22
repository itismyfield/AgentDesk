use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use uploads::{BundleRef, Upload};

fn bundle() -> ValidatedAttachmentBundle {
    let identity = AttachmentMessageIdentity {
        provider: "claude".into(),
        channel_id: "8120".into(),
        user_msg_id: "8121".into(),
    };
    validate_attachment_bundle_v1(
        AttachmentBundleV1 {
            version: ATTACHMENT_BUNDLE_V1,
            identity: identity.clone(),
            source_attachment_count: 2,
            entries: ["../../escape.png", "C:\\Users\\private\nfile.txt"]
                .into_iter()
                .map(|name| AttachmentEntryV1 {
                    filename: name.into(),
                    sha256: attachment_sha256_hex(b"verified bytes"),
                    bytes: b"verified bytes".to_vec(),
                })
                .collect(),
        },
        &identity,
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attachment_bundle_store_restart_identity_integrity_expiry_and_cleanup_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let validated = bundle();
    let reference = store::put(&pool, &validated).await.unwrap();
    assert_eq!(
        store::put(&pool, &validated).await.unwrap(),
        reference,
        "redelivery is idempotent"
    );
    let mut changed = validated.as_bundle().clone();
    changed.entries[0].filename = "changed".into();
    let changed = validate_attachment_bundle_v1(changed, &reference.identity).unwrap();
    assert!(
        store::put(&pool, &changed)
            .await
            .unwrap_err()
            .contains("different bytes")
    );

    // The durable queue survives a process restart using only typed references.
    let encoded = serde_json::to_vec(&vec![Upload::Bundle(reference.clone())]).unwrap();
    let restored: uploads::PendingUploads = serde_json::from_slice(&encoded).unwrap();
    let materialized = materialize::prepare(&restored, Some(&pool)).await.unwrap();
    assert_eq!(materialized.records.len(), 2);
    let first_path = materialized.records[0]
        .split(" → ")
        .nth(1)
        .unwrap()
        .split(" (14 bytes)")
        .next()
        .unwrap();
    let file = std::path::PathBuf::from(first_path);
    assert_eq!(std::fs::read(&file).unwrap(), b"verified bytes");
    assert_eq!(
        file.file_name().unwrap().to_string_lossy(),
        format!("0-{}.png", attachment_sha256_hex(b"verified bytes"))
    );
    assert!(
        !materialized.records[1].contains('\n'),
        "display filenames are escaped"
    );
    drop(materialized);
    assert!(!file.exists(), "provider lifetime owns cleanup");
    let prepared_again = materialize::prepare(&restored, Some(&pool)).await.unwrap();
    assert_eq!(prepared_again.records.len(), 2, "requeue materializes anew");
    drop(prepared_again);

    let mut foreign: BundleRef = reference.clone();
    foreign.identity.provider = "codex".into();
    assert!(store::load(&pool, &foreign).await.is_err());
    assert!(
        store::validate_refs(&pool, &[reference.clone()], "claude", "other-channel")
            .await
            .is_err()
    );
    let mut incomplete = reference.clone();
    incomplete.source_count = 1;
    assert!(store::load(&pool, &incomplete).await.is_err());
    sqlx::query("UPDATE intake_attachment_bundles SET payload=set_byte(payload,0,0) WHERE id=$1")
        .bind(reference.bundle_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        materialize::prepare(&restored, Some(&pool))
            .await
            .unwrap_err()
            .contains("HashMismatch")
    );
    sqlx::query(
        "UPDATE intake_attachment_bundles SET expires_at=NOW()-INTERVAL '1 second' WHERE id=$1",
    )
    .bind(reference.bundle_id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        store::load(&pool, &reference)
            .await
            .unwrap_err()
            .to_string()
            .contains("expired")
    );
    assert_eq!(store::cleanup(&pool).await.unwrap(), 1);
    assert!(materialize::prepare(&restored, Some(&pool)).await.is_err());
    pool.close().await;
    fixture.drop().await;
}

#[test]
fn attachment_upload_reference_preserves_legacy_json_and_enforces_size_limits() {
    let legacy: uploads::PendingUploads =
        serde_json::from_str("[\"legacy local record\"]").unwrap();
    assert!(legacy[0].is_local());
    assert_eq!(
        serde_json::to_string(&legacy).unwrap(),
        "[\"legacy local record\"]"
    );
    let mut oversized = bundle().as_bundle().clone();
    oversized.entries[0].bytes = vec![0; store::MAX_FILE_BYTES + 1];
    assert!(store::check_limits(&oversized).is_err());
    oversized.entries = vec![oversized.entries[1].clone(); store::MAX_FILES + 1];
    assert!(store::check_limits(&oversized).is_err());
}
