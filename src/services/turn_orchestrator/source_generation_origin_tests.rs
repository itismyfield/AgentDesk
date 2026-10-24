use std::sync::MutexGuard;
use std::time::{Duration, Instant, SystemTime};

use poise::serenity_prelude::{ChannelId, MessageId};

use super::{PendingQueueSourceGeneration, SourceMessageQueuedGeneration};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::pending_queue_persistence::{
    PendingQueueItem, load_pending_queues_checked, pending_queue_file_path,
    pending_queue_item_from_intervention, pending_queue_item_to_intervention, save_channel_queue,
};
use crate::services::turn_orchestrator::queue_enqueue::enqueue_intervention;
use crate::services::turn_orchestrator::test_support::{AGENTDESK_ROOT_DIR_ENV, lock_test_env};
use crate::services::turn_orchestrator::{Intervention, QueuedRoute};

const WALL_MS: u64 = 1_700_000_000_000;
const CHANNEL: u64 = 7_240_201;
const SOURCE: &str = r#"{"message_id":17,"queued_generation":71,"enqueued_at_epoch_us":1700000000000000,"preserve_on_cancel":true}"#;
// The pre-origin serializer's complete field order and omission rules, without a schema upgrade.
const ITEM: &str = r#"{"author_id":3,"author_is_bot":false,"message_id":17,"created_at_wall_time_ms":1700000000000,"queued_generation":71,"source_message_ids":[17],"source_message_queued_generations":[{"message_id":17,"queued_generation":71,"enqueued_at_epoch_us":1700000000000000,"preserve_on_cancel":true}],"text":"first body","reply_context":null,"has_reply_boundary":false,"merge_consecutive":true,"channel_id":7240201,"channel_name":null,"override_channel_id":null}"#;

struct Root {
    _dir: tempfile::TempDir,
    _lock: MutexGuard<'static, ()>,
}

impl Root {
    fn new() -> Self {
        let lock = lock_test_env();
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, dir.path()) };
        Self {
            _dir: dir,
            _lock: lock,
        }
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }
}

fn clocks() -> (SystemTime, Instant) {
    (
        SystemTime::UNIX_EPOCH + Duration::from_millis(WALL_MS),
        Instant::now(),
    )
}

fn old_item() -> PendingQueueItem {
    serde_json::from_str(ITEM).unwrap()
}

fn item(id: u64, text: &str, origin: &str) -> Intervention {
    let (wall, instant) = clocks();
    let mut item = old_item();
    item.message_id = id;
    item.source_message_ids = vec![id];
    item.source_message_queued_generations[0].message_id = id;
    item.text = text.into();
    item.source_message_queued_generations[0].origin = Some(origin.into());
    pending_queue_item_to_intervention(item, wall, instant)
}

#[test]
fn legacy_origin_absence_preserves_exact_source_and_queue_item_bytes() {
    let _root = Root::new();
    let source: PendingQueueSourceGeneration = serde_json::from_str(SOURCE).unwrap();
    assert_eq!(source.origin, None);
    let encoded = serde_json::to_string(&source).unwrap();
    assert_eq!(encoded, SOURCE, "origin_none_exact_source_bytes_contract");
    assert_eq!(
        SourceMessageQueuedGeneration::new(MessageId::new(17), 71).origin,
        None,
        "Legacy constructors cannot acquire an intake origin"
    );
    let item = old_item();
    assert_eq!(serde_json::to_string(&item).unwrap(), ITEM);
    let (wall, instant) = clocks();
    let restored = pending_queue_item_to_intervention(item, wall, instant);
    assert_eq!(restored.source_message_queued_generations[0].origin, None);
    let saved = pending_queue_item_from_intervention(
        &restored,
        ChannelId::new(CHANNEL),
        None,
        wall,
        instant,
    );
    assert_eq!(
        serde_json::to_string(&saved).unwrap(),
        ITEM,
        "origin None must retain the pre-origin queue file bytes"
    );
}

#[test]
fn old_queue_file_without_origin_loads_and_resaves_without_new_fields() {
    let _root = Root::new();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(CHANNEL);
    let path = pending_queue_file_path(&provider, "origin-old", channel).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!("[{ITEM}]")).unwrap();
    let mut recovered = load_pending_queues_checked(&provider, "origin-old");
    assert!(recovered.scan_errors.is_empty());
    assert!(recovered.quarantined.is_empty());
    let queue = recovered.queues.remove(&channel).unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].source_message_queued_generations[0].origin, None);
    save_channel_queue(&provider, "origin-old", channel, &queue, None).unwrap();
    let saved: Vec<PendingQueueItem> =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_string(&saved[0].source_message_queued_generations[0]).unwrap(),
        SOURCE,
        "the real queue writer must omit an absent origin"
    );
}

#[test]
fn mixed_source_merge_and_disk_recovery_keep_each_origin_and_upload() {
    let _root = Root::new();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(CHANNEL);
    let mut first = item(17, "first body", "attachment");
    first.pending_uploads = vec!["first.pdf".into()];
    let mut second = item(18, "second body", "voice");
    second.pending_uploads = vec!["second.wav".into()];
    let mut queue = Vec::new();
    assert!(enqueue_intervention(&mut queue, first, None).enqueued);
    let merged = enqueue_intervention(&mut queue, second, None);
    assert!(merged.enqueued && merged.merged);
    assert_eq!(queue.len(), 1);
    assert_eq!(
        origins(&queue[0]),
        vec![(17, Some("attachment")), (18, Some("voice"))],
        "merging a new source cannot overwrite the original source's origin"
    );
    save_channel_queue(&provider, "origin-merge", channel, &queue, None).unwrap();
    let mut recovered = load_pending_queues_checked(&provider, "origin-merge");
    assert!(recovered.scan_errors.is_empty());
    assert!(recovered.quarantined.is_empty());
    let restored = recovered.queues.remove(&channel).unwrap().remove(0);
    assert_eq!(
        origins(&restored),
        vec![(17, Some("attachment")), (18, Some("voice"))],
        "source origins must survive both queue serialization directions"
    );
    assert_eq!(restored.text, "first body\nsecond body");
    assert_eq!(restored.pending_uploads, queue[0].pending_uploads);
    let routed = restored.with_route(QueuedRoute {
        target: channel,
        parent: None,
        role: None,
    });
    assert_eq!(origins(&routed), origins(&queue[0]));
}

#[test]
fn legacy_supplemented_source_has_no_fabricated_origin() {
    let _root = Root::new();
    let (wall, instant) = clocks();
    let mut item = old_item();
    item.source_message_ids.push(18);
    item.source_message_queued_generations[0].origin = Some("skill".into());
    let restored = pending_queue_item_to_intervention(item, wall, instant);
    assert_eq!(
        origins(&restored),
        vec![(17, Some("skill")), (18, None)],
        "an old missing source owner cannot borrow another source's origin"
    );
}

fn origins(item: &Intervention) -> Vec<(u64, Option<&str>)> {
    item.source_message_queued_generations
        .iter()
        .map(|source| (source.message_id.get(), source.origin.as_deref()))
        .collect()
}
