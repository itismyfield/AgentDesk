//! Input-chunk equality and mailbox ownership at the live busy-inject boundary.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::cluster::attachment_transfer::uploads::Upload;
use crate::services::discord::health::{
    HealthRegistry, InjectPane, claim_kinded, register_inject_runtime,
};
use crate::services::discord::router::message_handler::render_input_chunks;
use crate::services::turn_orchestrator::ActiveTurnKind;
use serde_json::json;
use std::sync::Arc;

const FIRST: &str = "[File uploaded] one.txt → /tmp/i7196-one.txt (3 bytes)";
const SECOND: &str = "[File uploaded] two.txt → /tmp/i7196-two.txt (7 bytes)";

fn message(channel: u64, text: &str, attachments: usize) -> serenity::Message {
    let mut message = serenity::Message::default();
    message.id = MessageId::new(channel + 100);
    message.channel_id = ChannelId::new(channel);
    message.author.id = serenity::UserId::new(200);
    message.author.name = "lane-user".into();
    message.content = text.into();
    message.attachments = (0..attachments)
        .map(|n| {
            serde_json::from_value(json!({
                "id": (channel + 1000 + n as u64).to_string(),
                "filename": format!("file-{n}.txt"), "size": 3,
                "url": "https://invalid.example/fixture", "proxy_url": ""
            }))
            .unwrap()
        })
        .collect();
    message
}

fn live<'a>(message: &'a serenity::Message, uploads: &'a [Upload]) -> LiveText<'a> {
    LiveText {
        channel_id: message.channel_id,
        parent: None,
        message,
        author_is_allowed_automation: false,
        text: &message.content,
        reply_context: None,
        has_reply_boundary: false,
        uploads,
        voice_announcement: None,
        merge_consecutive: false,
    }
}

async fn seeded_runtime(channel: u64, pool: Option<sqlx::PgPool>) -> (Arc<SharedData>, InjectPane) {
    let shared = register_inject_runtime(&HealthRegistry::new(), &[channel], pool).await;
    shared.restart.reconcile_done.store(true, Relaxed);
    claim_kinded(&shared, channel, ActiveTurnKind::Background).await;
    (shared, InjectPane::new(channel, "all"))
}

async fn runtime(channel: u64) -> (TestPostgresDb, Arc<SharedData>, InjectPane) {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let (shared, pane) = seeded_runtime(channel, Some(pool)).await;
    (database, shared, pane)
}

async fn assert_injected_chunks(
    channel: u64,
    text: &str,
    uploads: &[Upload],
    reply: Option<&str>,
    expected: &str,
) {
    let (_database, shared, pane) = runtime(channel).await;
    let _gate = test_support::open_gate(channel);
    let message = message(channel, text, uploads.len());
    let mut live = live(&message, uploads);
    live.reply_context = reply;
    live.has_reply_boundary = reply.is_some();
    let records: Vec<_> = uploads.iter().map(Upload::history_record).collect();
    let rendered = render_input_chunks(&records, reply.map(str::to_string), text.to_string());
    assert_eq!(rendered.as_bytes(), expected.as_bytes());
    assert_eq!(body(&live, &records).as_bytes(), rendered.as_bytes());
    pane.fold_paste(expected.matches('\n').count() + 1);
    pane.set(
        "receipt",
        &json!({"type": "user", "message": {"role": "user", "content":
            "\n\n<pasted_content id=\"dfd9\">\n@BODY@\n</pasted_content id=\"dfd9\">\n"
        }})
        .to_string(),
    );
    let result = attempt(&shared, &ProviderKind::Claude, &live).await;
    assert!(
        matches!(result, Some(InjectAttempt::Injected { .. })),
        "{result:?}"
    );
    assert_eq!(pane.keys(), ["paste-buffer", "send-keys"]);
    let buffer = pane.read("buffer").expect("submitted buffer");
    let (_, chunk) = buffer.split_once('\n').expect("outer author frame");
    assert_eq!(chunk.as_bytes(), expected.as_bytes());
    assert_eq!(chunk.as_bytes(), rendered.as_bytes());
    let snapshot = crate::services::discord::mailbox_snapshot(&shared, live.channel_id).await;
    assert_eq!(
        snapshot.intervention_queue.len(),
        0,
        "injection owns the input"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn multiline_input_chunks_match_normal_turn_and_confirm_native_padding_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    assert_injected_chunks(
        7_196_100,
        "one\ntwo\n\nthree",
        &[],
        Some("reply context"),
        "reply context\n\none\ntwo\n\nthree",
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn one_attachment_input_chunks_match_normal_turn_and_confirm_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let uploads = [Upload::Local(FIRST.into())];
    assert_injected_chunks(
        7_196_101,
        "inspect",
        &uploads,
        Some("reply context"),
        "[File uploaded] one.txt → /tmp/i7196-one.txt (3 bytes)\n\nreply context\n\ninspect",
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn multiple_attachment_input_chunks_match_normal_turn_and_confirm_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let uploads = [Upload::Local(FIRST.into()), Upload::Local(SECOND.into())];
    assert_injected_chunks(
        7_196_102, "inspect\nboth", &uploads, None,
        "[File uploaded] one.txt → /tmp/i7196-one.txt (3 bytes)\n[File uploaded] two.txt → /tmp/i7196-two.txt (7 bytes)\n\ninspect\nboth",
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn pane_veto_hands_back_one_normal_busy_queue_payload_with_uploads_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 7_196_103;
    let (_database, shared, pane) = runtime(channel).await;
    let _gate = test_support::open_gate(channel);
    let uploads = [Upload::Local(FIRST.into()), Upload::Local(SECOND.into())];
    let message = message(channel, "inspect\nboth", uploads.len());
    let mut live = live(&message, &uploads);
    live.reply_context = Some("reply context");
    live.has_reply_boundary = true;
    let normal = super::super::busy_queue::spec(&live).into_intervention();
    assert_eq!(normal.pending_uploads, uploads);
    pane.draft();
    let before = crate::services::discord::mailbox_snapshot(&shared, live.channel_id).await;
    assert_eq!(before.intervention_queue.len(), 0);
    let result = attempt(&shared, &ProviderKind::Claude, &live).await;
    assert!(
        matches!(
            result,
            Some(InjectAttempt::HandedBack { veto: "draft", .. })
        ),
        "{result:?}"
    );
    assert!(pane.keys().is_empty(), "veto must precede paste and Enter");
    let snapshot = crate::services::discord::mailbox_snapshot(&shared, live.channel_id).await;
    assert_eq!(snapshot.intervention_queue.len(), 1);
    let queued = snapshot.intervention_queue.first().unwrap();
    assert_eq!(queued.message_id, normal.message_id);
    assert_eq!(queued.author_id, normal.author_id);
    assert_eq!(queued.author_is_bot, normal.author_is_bot);
    assert_eq!(queued.text, normal.text);
    assert_eq!(queued.reply_context, normal.reply_context);
    assert_eq!(queued.has_reply_boundary, normal.has_reply_boundary);
    assert_eq!(queued.merge_consecutive, normal.merge_consecutive);
    assert_eq!(queued.pending_uploads, normal.pending_uploads);
    assert_eq!(queued.pending_uploads, uploads);
    assert_eq!(queued.text.as_bytes(), b"inspect\nboth");
    assert!(queued.voice_announcement.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn incomplete_uploads_and_unresolved_voice_never_attempt_pane_input() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 7_196_104;
    let (shared, pane) = seeded_runtime(channel, None).await;
    let _gate = test_support::open_gate(channel);
    let uploads = [Upload::Local(FIRST.into())];
    let incomplete = message(channel, "inspect both", 2);
    let incomplete_live = live(&incomplete, &uploads);
    assert_eq!(readiness(&incomplete_live), Some("uploads_not_ready"));
    assert!(
        attempt(&shared, &ProviderKind::Claude, &incomplete_live)
            .await
            .is_none()
    );
    let voice = message(
        channel,
        "🎙️ \"원문\"\n||ADK_VOICE_ANNOUNCE_REF v1 key=voice:1:7196104:fixture||",
        0,
    );
    let voice_live = live(&voice, &[]);
    assert_eq!(readiness(&voice_live), Some("voice_claim_deferred"));
    assert!(
        attempt(&shared, &ProviderKind::Claude, &voice_live)
            .await
            .is_none()
    );
    assert_eq!(
        pane.tmux_calls(),
        0,
        "readiness guards must precede target/pane resolution"
    );
    assert!(pane.keys().is_empty());
    let snapshot =
        crate::services::discord::mailbox_snapshot(&shared, ChannelId::new(channel)).await;
    assert_eq!(
        snapshot.intervention_queue.len(),
        0,
        "existing intake retains unoffered input"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn plain_text_bytes_and_unregistered_command_routes_stay_unchanged_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    assert_injected_chunks(7_196_105, "status?", &[], None, "status?").await;
    let channel = 7_196_106;
    let (shared, pane) = seeded_runtime(channel, None).await;
    let _gate = test_support::open_gate(channel);
    for (n, text) in ["!unregistered", "DISPATCH: task", "/meeting task"]
        .into_iter()
        .enumerate()
    {
        let mut message = message(channel, text, 0);
        message.id = MessageId::new(channel + 200 + n as u64);
        let live = live(&message, &[]);
        assert_eq!(body(&live, &[]).as_bytes(), text.as_bytes());
        assert!(
            attempt(&shared, &ProviderKind::Claude, &live)
                .await
                .is_none(),
            "{text}"
        );
    }
    assert_eq!(pane.tmux_calls(), 0);
    assert!(pane.keys().is_empty());
}

#[test]
fn normal_busy_voice_spec_keeps_readable_ref_uploads_and_no_accepted_payload() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let uploads = [Upload::Local(FIRST.into())];
    let text = "🎙️ \"원문\"\n||ADK_VOICE_ANNOUNCE_REF v1 key=voice:1:7196104:fixture||";
    let message = message(7_196_107, text, 1);
    let announcement = crate::voice::prompt::voice_transcript_announcement_meta(
        "원문",
        200,
        "utt-i7196",
        "ko-KR",
        false,
        "2026-10-20T00:00:00Z",
        "2026-10-20T00:00:01Z",
        48_000,
        None,
        None,
        None,
    );
    let mut live = live(&message, &uploads);
    live.voice_announcement = Some(&announcement);
    live.merge_consecutive = true;
    assert_eq!(readiness(&live), Some("voice_claim_deferred"));
    let normal = super::super::busy_queue::spec(&live).into_intervention();
    let (handback, _) = handback(&live);
    assert_eq!(normal.pending_uploads, uploads);
    assert_eq!(normal.text.as_bytes(), text.as_bytes());
    assert!(normal.voice_announcement.is_none());
    assert!(!normal.merge_consecutive);
    assert_eq!(handback.pending_uploads, normal.pending_uploads);
    assert_eq!(handback.text, normal.text);
    assert!(handback.voice_announcement.is_none());
    assert!(!handback.merge_consecutive);
}

#[tokio::test(flavor = "current_thread")]
async fn retained_bundle_files_outlive_preparation_and_confirm_as_normal_input_chunks_pg() {
    use crate::services::cluster::attachment_transfer::{
        self as transfer, AttachmentBundleV1, AttachmentEntryV1, AttachmentMessageIdentity,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 7_196_108;
    let (_database, shared, pane) = runtime(channel).await;
    let _gate = test_support::open_gate(channel);
    let message = message(channel, "inspect retained", 1);
    let identity = AttachmentMessageIdentity {
        provider: "claude".into(),
        channel_id: message.channel_id.to_string(),
        user_msg_id: message.id.to_string(),
    };
    let bundle = transfer::validate_attachment_bundle_v1(
        AttachmentBundleV1 {
            version: transfer::ATTACHMENT_BUNDLE_V1,
            identity: identity.clone(),
            source_attachment_count: 1,
            entries: vec![AttachmentEntryV1 {
                filename: "bundle.txt".into(),
                sha256: transfer::attachment_sha256_hex(b"bundle bytes"),
                bytes: b"bundle bytes".to_vec(),
            }],
        },
        &identity,
    )
    .unwrap();
    let pool = shared
        .pg_pool
        .as_ref()
        .expect("isolated attachment storage");
    let reference = transfer::store::put(pool, &bundle).await.unwrap();
    let uploads = [Upload::Bundle(reference)];
    let prepared = transfer::materialize::prepare(&uploads, Some(pool))
        .await
        .unwrap();
    let record_path = |record: &str| {
        std::path::PathBuf::from(
            record
                .split_once(" → ")
                .unwrap()
                .1
                .rsplit_once(" (")
                .unwrap()
                .0,
        )
    };
    let temporary = record_path(&prepared.records[0]);
    assert_eq!(std::fs::read(&temporary).unwrap(), b"bundle bytes");
    let records = crate::services::discord::router::message_handler::retain_busy_uploads(
        ChannelId::new(channel),
        prepared,
    )
    .await
    .expect("retain a bundle loaded from durable storage");
    assert!(
        !temporary.exists(),
        "preparation's private directory has dropped"
    );
    assert_eq!(records.len(), 1);
    let stable = record_path(&records[0]);
    let directory = crate::services::discord::settings::channel_upload_dir(ChannelId::new(channel))
        .expect("channel upload directory");
    assert_eq!(stable.parent(), Some(directory.as_path()));
    assert_eq!(std::fs::read(&stable).unwrap(), b"bundle bytes");
    let before: std::collections::HashSet<_> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    let live = live(&message, &uploads);
    assert_eq!(readiness(&live), None);
    pane.fold_paste(3);
    pane.set(
        "receipt",
        &json!({"type": "user", "message": {"role": "user", "content":
            "\n\n<pasted_content id=\"dfd9\">\n@BODY@\n</pasted_content id=\"dfd9\">\n"
        }})
        .to_string(),
    );
    let result = attempt(&shared, &ProviderKind::Claude, &live).await;
    assert!(
        matches!(result, Some(InjectAttempt::Injected { .. })),
        "{result:?}"
    );
    assert_eq!(pane.keys(), ["paste-buffer", "send-keys"]);
    let newly_retained: Vec<_> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| !before.contains(path))
        .collect();
    assert_eq!(
        newly_retained.len(),
        1,
        "the actual Bundle attempt retains its own materialization"
    );
    let injected_file = &newly_retained[0];
    let record = format!(
        "[File uploaded] \"bundle.txt\" → {} (12 bytes)",
        injected_file.display()
    );
    let expected = format!("{record}\n\ninspect retained");
    let records = [record];
    let rendered = render_input_chunks(&records, None, "inspect retained".into());
    assert_eq!(rendered.as_bytes(), expected.as_bytes());
    assert_eq!(body(&live, &records).as_bytes(), expected.as_bytes());
    let buffer = pane.read("buffer").unwrap();
    let (_, chunk) = buffer.split_once('\n').unwrap();
    assert_eq!(chunk.as_bytes(), rendered.as_bytes());
    let snapshot = crate::services::discord::mailbox_snapshot(&shared, live.channel_id).await;
    assert_eq!(snapshot.intervention_queue.len(), 0);
    drop(pane);
    assert_eq!(
        std::fs::read(injected_file).unwrap(),
        b"bundle bytes",
        "the actual stored Bundle remains readable after the injection call and pane fixture end"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failed_download_existing_error_never_reaches_injection() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 7_196_109;
    let (shared, pane) = seeded_runtime(channel, None).await;
    let _gate = test_support::open_gate(channel);
    let http = Arc::new(serenity::Http::new("i7196-unused-invalid-token"));
    let attachments = [
        crate::services::discord::router::message_handler::AttachmentDescriptor {
            filename: "unavailable.txt".into(),
            url: "https://invalid.example/unavailable.txt".into(),
        },
    ];
    let permit = crate::services::discord::router::message_handler::LocalAttachmentPreparationPermit::after_local_admission();
    let error =
        crate::services::discord::router::message_handler::prepare_admitted_local_attachment(
            &http,
            ChannelId::new(channel),
            &attachments,
            &shared,
            &permit,
        )
        .await
        .expect_err("failed attachment download retains the existing preparation error");
    assert_eq!(error.to_string(), "attachment URL host is not allowed");
    assert_eq!(pane.tmux_calls(), 0);
    assert!(pane.keys().is_empty());
    let snapshot =
        crate::services::discord::mailbox_snapshot(&shared, ChannelId::new(channel)).await;
    assert_eq!(snapshot.intervention_queue.len(), 0);
}

#[test]
fn bundle_handback_preserves_the_original_durable_reference_and_source_count() {
    use crate::services::cluster::attachment_transfer::{
        AttachmentMessageIdentity, uploads::BundleRef,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let message = message(7_196_110, "inspect both", 2);
    let uploads = [Upload::Bundle(BundleRef {
        bundle_id: uuid::Uuid::from_u128(7196),
        identity: AttachmentMessageIdentity {
            provider: "claude".into(),
            channel_id: message.channel_id.to_string(),
            user_msg_id: message.id.to_string(),
        },
        source_count: 2,
    })];
    let live = live(&message, &uploads);
    assert_eq!(
        readiness(&live),
        None,
        "one bundle represents both completed files"
    );
    let (queued, _) = handback(&live);
    assert_eq!(
        queued.pending_uploads, uploads,
        "queue keeps portable references"
    );
    assert_eq!(queued.text, message.content);
}
