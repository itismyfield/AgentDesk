use super::*;
use crate::services::cluster::attachment_transfer::AttachmentEntryV1;
use crate::services::discord::input_runtime::supervisor::{Registration, command::SupervisorCmd};
use crate::services::tui_input::blob::BlobPin;
use crate::services::tui_input::rows::receipt_identity::Responsibility;
use crate::voice::prompt::VoiceTranscriptAnnouncement;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

fn draft(channel: u64, slash: bool) -> SourceDraft {
    let origin = if slash {
        Origin::SlashInteraction {
            interaction_id: 41,
            author_id: 7,
            original_channel_id: channel,
            execution_channel_id: channel,
        }
    } else {
        Origin::Message {
            message_id: 10,
            source_ids: vec![11, 10],
            author_id: 7,
            original_channel_id: channel,
            execution_channel_id: channel,
        }
    };
    SourceDraft {
        origin,
        author_is_bot: false,
        original_text: "original command".into(),
        prompt: "expanded prompt".into(),
        reply_context: Some("original reply".into()),
        attachments: [b"first".as_slice(), b"second".as_slice()]
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| {
                let filename = format!("{index}.txt");
                super::super::StagedAttachment {
                    filename: filename.clone(),
                    url: format!("https://fixture.invalid/{index}"),
                    staged: Some(AttachmentEntryV1 {
                        filename,
                        sha256: hex::encode(Sha256::digest(bytes)),
                        bytes: bytes.into(),
                    }),
                }
            })
            .collect(),
        voice_announcement: Some(VoiceTranscriptAnnouncement {
            transcript: "spoken words".into(),
            user_id: "7".into(),
            utterance_id: "utterance-10".into(),
            language: "ko".into(),
            verbose_progress: true,
            started_at: Some("2026-10-12T08:00:00Z".into()),
            completed_at: Some("2026-10-12T08:00:02Z".into()),
            samples_written: Some(240),
            control_channel_id: Some(channel),
            stt_mode: Some("streaming".into()),
            stt_latency_ms: Some(321),
        }),
    }
}

fn ready(
    registry: &'static Registry,
    channel: u64,
    root: &std::path::Path,
) -> (Registration, Arc<Client>, mpsc::Receiver<SupervisorCmd>) {
    let registration = registry
        .register(&ProviderKind::Claude, channel, root)
        .unwrap();
    let overflow = registration.order.as_ref().unwrap().pending_overflow();
    let (sender, receiver) = mpsc::channel(4);
    let client = Arc::new(Client::new(sender, overflow));
    client.publish();
    registry
        .entries
        .lock()
        .unwrap()
        .get_mut(&("claude".into(), channel))
        .unwrap()
        .client = Some(Arc::downgrade(&client));
    (registration, client, receiver)
}

#[test]
fn c2b_kind_factory_preserves_whole_original_and_staged_source_for_each_actual_kind() {
    for (kind, label) in [
        (SourceKind::LiveMessage, "live_message"),
        (SourceKind::RawAttachment, "raw_attachment"),
        (SourceKind::TextSkill, "text_skill"),
        (SourceKind::SlashSkill, "slash_skill"),
    ] {
        let slash = kind == SourceKind::SlashSkill;
        for message_kind in if slash {
            vec![None]
        } else {
            vec![Some("regular"), Some("inline_reply")]
        } {
            let original = prepare_source(draft(9, slash)).unwrap();
            let mut expected = original.input().clone();
            expected["source_kind"] = json!(label);
            expected["message_kind"] = json!(message_kind);
            let prepared = prepare(kind, draft(9, slash), message_kind).unwrap();
            assert_eq!(prepared.key(), original.key());
            assert_eq!(prepared.identity(), original.identity());
            assert_eq!(prepared.input(), &expected);
            assert_eq!(prepared.attachments(), original.attachments());
        }
    }
}

#[test]
fn c2b_kind_factory_rejects_mismatched_origin_message_kind_and_synthetic_message_ids() {
    assert!(prepare(SourceKind::SlashSkill, draft(9, false), None).is_err());
    assert!(prepare(SourceKind::TextSkill, draft(9, true), Some("regular")).is_err());
    assert!(prepare(SourceKind::LiveMessage, draft(9, false), None).is_err());
    assert!(prepare(SourceKind::RawAttachment, draft(9, false), Some("system")).is_err());
    assert!(prepare(SourceKind::SlashSkill, draft(9, true), Some("regular")).is_err());
    let mut synthetic = draft(9, false);
    synthetic.origin = Origin::Message {
        message_id: 9_000_000_000_000_000_001,
        source_ids: vec![9_000_000_000_000_000_001],
        author_id: 7,
        original_channel_id: 9,
        execution_channel_id: 9,
    };
    assert!(prepare(SourceKind::LiveMessage, synthetic, Some("regular")).is_err());
}

#[tokio::test]
async fn c2b_live_pending_precedes_io_and_drop_keeps_same_overflow_and_original() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let registry = Box::leak(Box::new(Registry::new()));
    let channel = 6_328_410;
    let _gate = crate::services::discord::input_runtime::fence::Gate::protect(
        ProviderKind::Claude,
        channel,
    )
    .unwrap();
    let (mut registration, client, mut receiver) = ready(registry, channel, root.path());
    let original = ReceiptIdentity::new(10, vec![11, 10], 7, channel, channel).unwrap();
    let before = original.clone();
    let overflow = registration.order.as_ref().unwrap().pending_overflow();
    let generation = overflow.generation();
    let io_marker = root.path().join("reply-attachment-io.started");
    assert!(!io_marker.exists());
    let mut pending = Box::pin(begin_live(registry, &ProviderKind::Claude, 10, &original));
    let command = tokio::select! { command = receiver.recv() => command.unwrap(), result = &mut pending => panic!("pending must receive its real acknowledgement first: {}", result.is_ok()) };
    let SupervisorCmd::PendingSource { sources, reply } = command else {
        panic!("the first command must register original pending sources")
    };
    assert_eq!(
        sources,
        [10, 11],
        "reply/attachment IO cannot run with an empty pending scope"
    );
    assert!(
        registration
            .order
            .as_ref()
            .unwrap()
            .pending_snapshot()
            .sources
            .is_empty()
    );
    reply
        .send(Ok(registration
            .order
            .as_mut()
            .unwrap()
            .admit_pending(&sources)
            .unwrap()))
        .unwrap();
    let guard = pending.await.unwrap().unwrap();
    assert!(std::ptr::eq(guard.client(), client.as_ref()));
    assert_eq!(guard.identity(), &before);
    assert_eq!(
        registration
            .order
            .as_ref()
            .unwrap()
            .pending_snapshot()
            .sources,
        [10, 11]
    );
    assert!(!io_marker.exists());
    std::fs::write(&io_marker, b"staged only after pending acknowledgement").unwrap();
    assert!(io_marker.exists());
    let io_observed_pending = !registration
        .order
        .as_ref()
        .unwrap()
        .pending_snapshot()
        .sources
        .is_empty();
    assert!(io_observed_pending);
    drop(guard);
    assert_eq!(overflow.generation(), generation + 1);
    assert_eq!(original, before);
    assert!(
        registration
            .order
            .as_ref()
            .unwrap()
            .pending_snapshot()
            .sources
            .iter()
            .all(|id| before.source_ids.contains(id))
    );
    let slot = registration.slot().unwrap();
    assert!(registration.release(slot));
}

#[tokio::test]
async fn c2b_live_pending_cancelled_before_ack_never_runs_io_and_keeps_original() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let registry = Box::leak(Box::new(Registry::new()));
    let channel = 6_328_411;
    let _gate = crate::services::discord::input_runtime::fence::Gate::protect(
        ProviderKind::Claude,
        channel,
    )
    .unwrap();
    let (mut registration, _client, mut receiver) = ready(registry, channel, root.path());
    let original = ReceiptIdentity::new(10, vec![10, 11], 7, channel, channel).unwrap();
    let before = original.clone();
    let overflow = registration.order.as_ref().unwrap().pending_overflow();
    let generation = overflow.generation();
    let mut pending = Box::pin(begin_live(registry, &ProviderKind::Claude, 10, &original));
    let command = tokio::select! { command = receiver.recv() => command.unwrap(), result = &mut pending => panic!("pending was acknowledged without its command: {}", result.is_ok()) };
    assert!(
        matches!(&command, SupervisorCmd::PendingSource { sources, .. } if sources == &[10, 11])
    );
    drop(pending);
    assert_eq!(overflow.generation(), generation + 1);
    assert_eq!(original, before);
    assert!(
        registration
            .order
            .as_ref()
            .unwrap()
            .pending_snapshot()
            .sources
            .is_empty()
    );
    drop(command);
    let slot = registration.slot().unwrap();
    assert!(registration.release(slot));
}

#[tokio::test]
async fn c2b_borrowed_slash_submit_survives_cancellation_with_whole_source_and_one_receipt() {
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &root_path);
    let registry = Box::leak(Box::new(Registry::new()));
    let channel = 6_328_412;
    let _gate = crate::services::discord::input_runtime::fence::Gate::protect(
        ProviderKind::Claude,
        channel,
    )
    .unwrap();
    let (mut registration, client, mut receiver) = ready(registry, channel, &root_path);
    let selected = begin_external(registry, &ProviderKind::Claude, channel)
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&selected, &client));
    let prepared = prepare(SourceKind::SlashSkill, draft(channel, true), None).unwrap();
    let original = (
        prepared.key(),
        prepared.identity().clone(),
        prepared.input().clone(),
        prepared.attachments().to_vec(),
    );
    let overflow = registration.order.as_ref().unwrap().pending_overflow();
    let generation = overflow.generation();
    let mut request = Box::pin(submit_slash(&selected, &prepared));
    let command = tokio::select! { command = receiver.recv() => command.unwrap(), result = &mut request => panic!("durable acknowledgement cannot precede the supervisor loan: {result:?}") };
    let SupervisorCmd::SubmitExternal { source, reply } = command else {
        panic!("slash uses the existing external receipt command")
    };
    assert_eq!(source.key(), original.0);
    assert_eq!(source.identity(), &original.1);
    assert_eq!(source.input(), &original.2);
    drop(request);
    assert_eq!(overflow.generation(), generation + 1);
    assert_eq!(
        (
            prepared.key(),
            prepared.identity().clone(),
            prepared.input().clone(),
            prepared.attachments().to_vec()
        ),
        original
    );
    let mut slot = registration.slot().unwrap();
    let result =
        crate::services::discord::input_runtime::supervisor::loan(&mut slot, move |lease| {
            external::submit(lease, *source, true)
        })
        .await
        .expect("the actual receipt loan must restore its ledger slot");
    assert!(
        matches!(
            &result,
            external::ExternalReceipt::Received {
                duplicate: false,
                ..
            }
        ),
        "actual borrowed external receipt: {result:?}"
    );
    assert!(
        reply.send(result).is_err(),
        "receiver loss does not cancel accepted work"
    );
    let identity = prepared.identity().clone();
    let expected = original.clone();
    crate::services::discord::input_runtime::supervisor::loan(&mut slot, move |lease| {
        let ledger = lease.reopen().unwrap();
        let rows = ledger.rows().unwrap();
        assert_eq!(rows.open_rows().count(), 1);
        assert!(
            matches!(rows.responsibility(&identity), Responsibility::Known { key, .. } if key == expected.0)
        );
        let mut durable = rows.row(expected.0).unwrap().input.clone();
        let pins: Vec<BlobPin> = serde_json::from_value(
            durable
                .as_object_mut()
                .unwrap()
                .remove("blob_pins")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(durable, expected.2);
        assert_eq!(pins.len(), expected.3.len());
        for (pin, attachment) in pins.iter().zip(&expected.3) {
            assert_eq!(
                ledger.read_blob(pin).unwrap(),
                attachment.staged.as_ref().unwrap().bytes
            );
        }
    })
    .await
    .expect("receipt readback uses the same blocking loan and returns the slot");
    assert_eq!(
        (
            prepared.key(),
            prepared.identity().clone(),
            prepared.input().clone(),
            prepared.attachments().to_vec()
        ),
        original
    );
    assert!(registration.release(slot));
}

#[tokio::test]
async fn c2b_slash_helper_rejects_message_material_before_mailbox_and_off_lookup_stays_legacy() {
    let root = tempfile::tempdir().unwrap();
    let registry = Box::leak(Box::new(Registry::new()));
    assert!(
        begin_external(registry, &ProviderKind::Claude, 6_328_413)
            .unwrap()
            .is_none()
    );
    assert!(!registry.used());
    assert_eq!(registry.population_for_test(), (0, 0));
    let (mut registration, client, mut receiver) = ready(registry, 6_328_414, root.path());
    let prepared = prepare(
        SourceKind::LiveMessage,
        draft(6_328_414, false),
        Some("regular"),
    )
    .unwrap();
    let mut request = Box::pin(submit_slash(&client, &prepared));
    tokio::select! { result = &mut request => assert_eq!(result, Err(Deferred::Conflict)), command = receiver.recv() => panic!("invalid source reached mailbox: {}", command.is_some()) }
    assert!(receiver.try_recv().is_err());
    let slot = registration.slot().unwrap();
    assert!(registration.release(slot));
}
