//! Full canonical payload and both prepared-upload reference formats across the actual restore.
use super::*;
use crate::services::cluster::attachment_transfer::{self as transfer, uploads::Upload};
use crate::services::turn_orchestrator::PendingQueueItem;

fn stored_payload(f: &Fixture) -> PendingQueueItem {
    let path = crate::services::discord::input_queue_path_for_test(&f.shared, f.channel);
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let item = if value.is_array() {
        &value[0]
    } else {
        &value["items"][0]
    };
    serde_json::from_value(item.clone()).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn writer_boot_restores_absorbed_sources_reply_and_prepared_upload_bytes_pg() {
    scenario("race_tests::payload_tests::writer_boot_restores_absorbed_sources_reply_and_prepared_upload_bytes_pg",
        73_369_537, "mac-a", |f| Box::pin(async move {
            let done = KickoffCompletion::install(channel(f));
            let root = crate::config::runtime_root().unwrap();
            let local_path = root.join("prepared-boot-attachment.bin");
            let local_bytes: Vec<u8> = (0..=u8::MAX).collect();
            std::fs::write(&local_path, &local_bytes).unwrap();
            let local_record = format!("[File uploaded] {} → {} ({} bytes)",
                serde_json::json!("원문.bin"), local_path.display(), local_bytes.len());
            let pool = f.shared.pg_pool.as_ref().unwrap().clone();
            let identity = transfer::AttachmentMessageIdentity {
                provider: "claude".into(), channel_id: f.channel.to_string(),
                user_msg_id: f.message.to_string(),
            };
            let bundle_bytes = vec![0, 255, b'B', b'\n', 128, 0];
            let bundle = transfer::validate_attachment_bundle_v1(transfer::AttachmentBundleV1 {
                version: transfer::ATTACHMENT_BUNDLE_V1, identity: identity.clone(),
                source_attachment_count: 1,
                entries: vec![transfer::AttachmentEntryV1 {
                    filename: "portable.bin".into(),
                    sha256: transfer::attachment_sha256_hex(&bundle_bytes), bytes: bundle_bytes.clone(),
                }],
            }, &identity).unwrap();
            let reference = transfer::store::put(&pool, &bundle).await.unwrap();
            let uploads = vec![Upload::Local(local_record.clone()), Upload::Bundle(reference.clone())];
            let mut request = f.request();
            request.user_text = "  원문 _WIP_\r\n첨부를 읽어 주세요.\n".into();
            request.source_message_ids = vec![serenity::MessageId::new(f.message - 2),
                serenity::MessageId::new(f.message - 1)];
            request.reply_context = Some("인용 원문\r\n> keep _this_ exactly\n".into());
            request.has_reply_boundary = true;
            let original_request = request.clone();
            let checkpoint = f.checkpoints.clone();
            let deps = crate::services::discord::router::IntakeDeps {
                http: &f.discord.http, cache: None, ctx_for_chained_dispatch: None,
                shared: &f.shared, token: "test-token",
            };
            herdr_consent::dispatch_boot_payload_for_tests(&deps, request, uploads.clone(), move || {
                checkpoint.fetch_add(1, Ordering::SeqCst);
            }).await.unwrap();
            let saved = stored_payload(f);
            let expected_sources = vec![f.message, f.message - 2, f.message - 1];
            assert_eq!(saved.source_message_ids, expected_sources);
            assert_eq!(saved.source_message_queued_generations.len(), 3);
            assert!(saved.source_message_queued_generations.iter().all(|source| source.preserve_on_cancel));
            assert_eq!(saved.text.as_bytes(), original_request.user_text.as_bytes());
            assert_eq!(saved.reply_context, original_request.reply_context);
            assert!(saved.has_reply_boundary);
            assert_eq!(saved.pending_uploads, uploads);
            assert_eq!(serde_json::to_value(&saved.pending_uploads).unwrap(),
                serde_json::json!([local_record, reference]));
            assert_eq!(f.effects.load(Ordering::SeqCst), 0);
            assert_eq!(f.checkpoints.load(Ordering::SeqCst), 1);
            // Retire the in-memory actor, retaining its exact queue file; restore through production.
            f.shared.mailboxes.remove_fixture_for_test(channel(f));
            let http = f.shared.http.cached_serenity_ctx.get().unwrap().http.clone();
            crate::services::discord::runtime_bootstrap::restore_queued_and_inflight_for_tests(
                &http, &f.shared, &ProviderKind::Claude).await;
            let restored = stored_payload(f);
            for field in ["message_id", "queued_generation", "source_message_ids",
                "source_message_queued_generations", "source_text_segments", "text", "reply_context",
                "has_reply_boundary", "pending_uploads"] {
                assert_eq!(serde_json::to_value(&saved).unwrap()[field],
                    serde_json::to_value(&restored).unwrap()[field], "restored standard field {field}");
            }
            let actual = mailbox_snapshot(&f.shared, channel(f)).await.intervention_queue;
            assert_eq!(actual.len(), 1);
            assert_eq!(actual[0].source_message_ids.iter().map(|id| id.get()).collect::<Vec<_>>(), expected_sources);
            assert_eq!(actual[0].text.as_bytes(), saved.text.as_bytes());
            assert_eq!(actual[0].reply_context, saved.reply_context);
            assert!(actual[0].has_reply_boundary);
            assert_eq!(actual[0].pending_uploads, uploads);
            assert_eq!(std::fs::read(&local_path).unwrap(), local_bytes);
            assert_eq!(transfer::store::load(&pool, &reference).await.unwrap()
                .as_bundle().entries[0].bytes, bundle_bytes);
            let (effects, checkpoints) = (f.effects.clone(), f.checkpoints.clone());
            let expected_uploads = uploads.clone();
            let effect_override = herdr_consent::override_boot_payload_for_tests(move |request, uploads| {
                assert_eq!(request.user_text.as_bytes(), original_request.user_text.as_bytes());
                assert_eq!(request.reply_context, original_request.reply_context);
                assert!(request.has_reply_boundary);
                assert_eq!(request.source_message_ids.iter().map(|id| id.get()).collect::<Vec<_>>(), expected_sources);
                assert_eq!(uploads, &expected_uploads);
                assert_eq!(std::fs::read(&local_path).unwrap(), local_bytes);
                assert_eq!(checkpoints.load(Ordering::SeqCst), 1);
                effects.fetch_add(1, Ordering::SeqCst);
                Ok(crate::services::discord::IntakeExecution::Executed)
            });
            f.release.add_permits(1);
            one_effect(f, &done, 1).await;
            drop(effect_override);
        })).await;
}
