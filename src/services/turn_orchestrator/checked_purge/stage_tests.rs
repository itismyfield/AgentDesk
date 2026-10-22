//! The checked purge at its actor: wherever it stops, the actor's queue, the queue file, the
//! dispatch marker and the receipt agree, and a queue that changed after the capture is refused.

use std::os::unix::fs::PermissionsExt;

use super::super::pending_queue_persistence::{
    load_channel_pending_queue, pending_queue_file_path, post_save_fault, save_fault,
};
use super::super::registry_purge::MailboxPurgeOutcome;
use super::super::test_support::{AGENTDESK_ROOT_DIR_ENV, lock_test_env};
use super::herdr::{CheckedPurge, PurgeStage};
use super::*;
use crate::services::cluster::attachment_transfer::uploads::Upload;
use crate::voice::prompt::VoiceTranscriptAnnouncement;

const TOKEN: &str = "checked-purge";
const GENERATION: u64 = 5;

/// Where each row stops; the queue, receipt, file and marker it leaves are asserted per row.
const STAGES: [&str; 9] = [
    "saved",
    "reservation-kept",
    "not-saved",
    "published-error",
    "marker-failed",
    "foreign-namespace",
    "file-differs",
    "reserved-head",
    "closed",
];

/// Changes to the captured queue, each made after the capture and before the purge.
const CHANGES: [&str; 13] = [
    "unchanged",
    "text",
    "author",
    "reply-boundary",
    "reply-context",
    "source-ids",
    "source-generation",
    "segments",
    "uploads",
    "voice",
    "route",
    "order",
    "reservation",
];

/// The actor's queue and reservation, then the queue file and dispatch marker of `TOKEN`.
type Held = (Vec<u64>, Option<u64>, Vec<u64>, Option<u64>);

fn ctx(token: &str) -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, token, None)
}

fn human(id: u64, text: &str) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: GENERATION,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: vec![SourceMessageQueuedGeneration::user_instruction(
            MessageId::new(id),
            GENERATION,
        )],
        source_text_segments: Vec::new(),
        text: text.into(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

fn ids(items: &[Intervention]) -> Vec<u64> {
    items.iter().map(|item| item.message_id.get()).collect()
}

async fn held(handle: &ChannelMailboxHandle, channel: ChannelId) -> Held {
    let snapshot = handle.snapshot().await;
    let (file, _) = load_channel_pending_queue(&ProviderKind::Claude, TOKEN, channel);
    let marker = load_channel_pending_dispatch_marker(&ProviderKind::Claude, TOKEN, channel);
    let reserved = snapshot.pending_user_dispatch.map(MessageId::get);
    let marker = marker.map(|(item, _)| item.message_id.get());
    (
        ids(&snapshot.intervention_queue),
        reserved,
        ids(&file),
        marker,
    )
}

async fn expected(handle: &ChannelMailboxHandle) -> ExpectedQueue {
    ExpectedQueue::of(&handle.try_snapshot().await.unwrap())
}

async fn purge(
    handle: &ChannelMailboxHandle,
    token: &str,
    expected: ExpectedQueue,
) -> CheckedPurge {
    let purge = handle.try_purge_queue_if_unchanged(ctx(token), expected);
    purge.await.unwrap()
}

/// The stage a purge stopped at and the ids it reports removed.
fn outcome(purge: &CheckedPurge) -> (Option<PurgeStage>, Vec<u64>) {
    let stage = purge.failed.as_ref().map(|(stage, _)| *stage);
    (stage, ids(&purge.receipt))
}

fn namespace_dir(channel: ChannelId) -> std::path::PathBuf {
    let path = pending_queue_file_path(&ProviderKind::Claude, TOKEN, channel).unwrap();
    path.parent().unwrap().to_path_buf()
}

/// The queue file holding only a submitted-source record, so every queue write publishes.
fn with_metadata(channel: ChannelId) {
    let path = pending_queue_file_path(&ProviderKind::Claude, TOKEN, channel).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let source = serde_json::json!({"message_id": 1, "queued_generation": 1});
    let terminal = serde_json::json!({"primary": 1, "queued_generation": 1, "sources": [source]});
    let file = serde_json::json!({"version": 1, "items": [], "terminal_identities": [terminal]});
    std::fs::write(path, file.to_string()).unwrap();
}

/// Runs `work` on a current-thread runtime under a fresh runtime root.
fn rooted(work: impl std::future::Future<Output = ()>) {
    let _lock = lock_test_env();
    let root = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, root.path()) };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(work);
    unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
}

// Every stage leaves memory, the queue file, the marker and the receipt in agreement: a stop
// before publication changes nothing, one after it keeps the removal in the receipt, and a closed
// actor answers with an explicit refusal that leaves its successor alone.
#[test]
fn checked_purge_stages_keep_queue_file_marker_and_receipt_in_agreement() {
    rooted(async {
        let mut rows = 0;
        for (index, row) in STAGES.into_iter().enumerate() {
            let channel = ChannelId::new(6_772_301 + index as u64);
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel);
            if row == "closed" {
                let closed = registry.remove_idle_entry(channel).await;
                assert_eq!(closed, MailboxPurgeOutcome::Removed, "{row}");
                let successor = registry.handle(channel);
                let input = human(64, "successor input");
                assert!(successor.enqueue(input, ctx(TOKEN)).await.enqueued, "{row}");
                let purged = purge(&handle, TOKEN, ExpectedQueue::of(&Default::default())).await;
                assert_eq!(
                    outcome(&purged),
                    (Some(PurgeStage::Closed), vec![]),
                    "{row}"
                );
                assert_eq!(purged.queue_preserved(), Some(true), "{row}");
                let kept = held(&successor, channel).await;
                assert_eq!(
                    kept,
                    (vec![64], None, vec![64], None),
                    "{row}: successor untouched"
                );
                rows += 1;
                continue;
            }
            if row == "published-error" {
                with_metadata(channel);
            }
            if row != "marker-failed" {
                for id in [61, 62] {
                    let queued = handle
                        .enqueue(human(id, &format!("queued {id}")), ctx(TOKEN))
                        .await;
                    assert!(queued.enqueued, "{row}");
                }
            }
            let mut _kept_alive = None;
            let mut token = TOKEN;
            match row {
                "reservation-kept" => {
                    let taken = handle.take_next_soft(ctx(TOKEN)).await;
                    _kept_alive = Some(Box::new(taken.dispatch_lease) as Box<dyn std::any::Any>);
                }
                "not-saved" => save_fault::fail_next(channel),
                "published-error" => post_save_fault::fail_next(channel),
                "marker-failed" => {
                    let marker = human(70, "reserved before a restart");
                    let provider = ProviderKind::Claude;
                    save_channel_pending_dispatch_marker(&provider, TOKEN, channel, &marker, None)
                        .unwrap();
                }
                "foreign-namespace" => token = "another-token",
                "file-differs" => {
                    let other = human(63, "written by another owner");
                    let theirs = [human(61, "queued 61"), human(62, "queued 62"), other];
                    let provider = ProviderKind::Claude;
                    save_channel_queue(&provider, TOKEN, channel, &theirs, None).unwrap();
                }
                "reserved-head" => {
                    let head = QueueIdentity::of(&handle.snapshot().await.intervention_queue[0]);
                    let claim = handle.claim_queued_head(head, ctx(TOKEN)).await;
                    assert!(matches!(claim, QueuedHeadClaim::Reserved(_)), "{row}");
                    _kept_alive = Some(Box::new(claim) as Box<dyn std::any::Any>);
                }
                _ => {}
            }
            let captured = expected(&handle).await;
            let before = held(&handle, channel).await;
            let dir = namespace_dir(channel);
            if row == "marker-failed" {
                // The queue file is absent, so only the marker unlink needs the directory writable.
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            }
            let purged = purge(&handle, token, captured).await;
            if row == "marker-failed" {
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let after = held(&handle, channel).await;
            match row {
                "saved" => {
                    assert_eq!(outcome(&purged), (None, vec![61, 62]), "{row}");
                    assert_eq!(after, (vec![], None, vec![], None), "{row}");
                }
                "reservation-kept" => {
                    assert_eq!(outcome(&purged), (None, vec![62]), "{row}");
                    let kept = (vec![], Some(61), vec![], Some(61));
                    assert_eq!(after, kept, "{row}: the reservation and its marker stay");
                }
                "not-saved" => {
                    assert_eq!(
                        outcome(&purged),
                        (Some(PurgeStage::NotSaved), vec![]),
                        "{row}"
                    );
                    assert_eq!(after, before, "{row}: nothing changed");
                    assert_eq!(purged.queue_preserved(), Some(true), "{row}");
                }
                "published-error" => {
                    let removed = (Some(PurgeStage::SavedWithError), vec![61, 62]);
                    assert_eq!(outcome(&purged), removed, "{row}");
                    assert_eq!(
                        after,
                        (vec![], None, vec![], None),
                        "{row}: the removal stands"
                    );
                    assert_eq!(purged.queue_preserved(), Some(false), "{row}");
                    // The actor took the published file as its own, so its next write lands.
                    let next = handle.enqueue(human(63, "next"), ctx(TOKEN)).await;
                    assert!(next.persistence_error.is_none(), "{row}");
                    let restart = ChannelMailboxRegistry::default().handle(channel);
                    let hydrated = restart.try_hydrate_pending_queue_from_disk(ctx(TOKEN));
                    assert!(hydrated.await.unwrap().persistence_error.is_none(), "{row}");
                    let restored = ids(&restart.snapshot().await.intervention_queue);
                    assert_eq!(restored, [63], "{row}");
                }
                "marker-failed" => {
                    assert_eq!(
                        outcome(&purged),
                        (Some(PurgeStage::Marker), vec![]),
                        "{row}"
                    );
                    assert_eq!(
                        after,
                        (vec![], None, vec![], Some(70)),
                        "{row}: the marker stays"
                    );
                    // Once it can be unlinked, the marker's input is part of what the purge removed.
                    let again = purge(&handle, TOKEN, expected(&handle).await).await;
                    assert_eq!(outcome(&again), (None, vec![70]), "{row}");
                    assert_eq!(held(&handle, channel).await.3, None, "{row}");
                }
                _ => {
                    assert_eq!(
                        outcome(&purged),
                        (Some(PurgeStage::Refused), vec![]),
                        "{row}"
                    );
                    assert_eq!(after, before, "{row}: nothing changed");
                }
            }
            rows += 1;
        }
        assert_eq!(rows, STAGES.len());
    });
}

/// `row`'s change to the captured queue `[61, 62]`.
fn change(row: &str, queue: &mut Vec<Intervention>) {
    let first = &mut queue[0];
    match row {
        "text" => first.text.push('!'),
        "author" => first.author_id = UserId::new(8),
        "reply-boundary" => first.has_reply_boundary = true,
        "reply-context" => first.reply_context = Some("quoted".into()),
        "source-ids" => first.source_message_ids.push(MessageId::new(60)),
        "source-generation" => first.source_message_queued_generations[0].queued_generation += 1,
        "segments" => {
            first.source_message_ids.push(MessageId::new(60));
            first.source_text_segments = vec![
                SourceMessageTextSegment::new(MessageId::new(61), "first"),
                SourceMessageTextSegment::new(MessageId::new(60), "absorbed"),
            ];
        }
        "uploads" => first.pending_uploads = vec![Upload::Local("late".into())],
        "voice" => {
            first.voice_announcement = Some(VoiceTranscriptAnnouncement {
                transcript: "late".into(),
                user_id: "7".into(),
                utterance_id: "u".into(),
                language: "ko".into(),
                verbose_progress: false,
                started_at: None,
                completed_at: None,
                samples_written: None,
                control_channel_id: None,
                stt_mode: None,
                stt_latency_ms: None,
            })
        }
        "route" => {
            let route = QueuedRoute {
                target: ChannelId::new(9),
                parent: None,
                role: None,
            };
            *first = first.clone().with_route(route);
        }
        "order" => queue.swap(0, 1),
        _ => unreachable!("unknown change {row}"),
    }
}

// The purge compares every persisted field of every item, their order and the reservation: any
// change after the capture refuses it and leaves the changed queue in memory and in its file.
#[test]
fn checked_purge_refuses_any_change_after_the_capture() {
    rooted(async {
        let mut rows = 0;
        for (index, row) in CHANGES.into_iter().enumerate() {
            let channel = ChannelId::new(6_772_401 + index as u64);
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel);
            let mut queue = vec![human(61, "first"), human(62, "second")];
            handle.replace_queue(queue.clone(), ctx(TOKEN)).await;
            let captured = expected(&handle).await;
            let mut _lease = None;
            match row {
                "unchanged" => {}
                "reservation" => _lease = handle.take_next_soft(ctx(TOKEN)).await.dispatch_lease,
                _ => {
                    change(row, &mut queue);
                    handle.replace_queue(queue, ctx(TOKEN)).await;
                }
            }
            let before = held(&handle, channel).await;
            let purged = purge(&handle, TOKEN, captured).await;
            if row == "unchanged" {
                assert_eq!(outcome(&purged), (None, vec![61, 62]), "{row}");
            } else {
                assert_eq!(
                    outcome(&purged),
                    (Some(PurgeStage::Refused), vec![]),
                    "{row}"
                );
                assert_eq!(
                    held(&handle, channel).await,
                    before,
                    "{row}: the change stays"
                );
            }
            rows += 1;
        }
        assert_eq!(rows, CHANGES.len());
    });
}
