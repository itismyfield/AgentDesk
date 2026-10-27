//! Queue codecs keep legacy consumers separate from V2 inspection.
use super::*;

/// The V1 snapshot: queue items with the head reservation and the submitted sources.
#[derive(serde::Serialize, serde::Deserialize)]
struct ChannelQueueSnapshotV1 {
    version: u32,
    items: Vec<PendingQueueItem>,
    #[serde(flatten)]
    meta: SnapshotMeta,
}

/// The legacy array while `meta` is unset, the V1 object otherwise.
pub(crate) fn snapshot_json(
    items: Vec<PendingQueueItem>,
    meta: Option<&SnapshotMeta>,
) -> serde_json::Result<String> {
    let Some(meta) = meta.cloned() else {
        return serde_json::to_string_pretty(&items);
    };
    serde_json::to_string_pretty(&ChannelQueueSnapshotV1 {
        version: 1,
        items,
        meta,
    })
}

/// A legacy array, or a version-1 object; any other object is an error, never an empty queue.
pub(crate) fn parse_snapshot(
    content: &str,
) -> Result<(Vec<PendingQueueItem>, Option<SnapshotMeta>), String> {
    if !content.trim_start().starts_with('{') {
        let items = serde_json::from_str(content).map_err(|e| e.to_string())?;
        return Ok((items, None));
    }
    let snapshot: ChannelQueueSnapshotV1 =
        serde_json::from_str(content).map_err(|e| e.to_string())?;
    if snapshot.version != 1 {
        return Err(format!(
            "unsupported queue snapshot version {}",
            snapshot.version
        ));
    }
    if let Some(head) = &snapshot.meta.head_injection {
        let first = snapshot
            .items
            .first()
            .ok_or("reserved snapshot has no head")?;
        for item in [first, &head.original] {
            if item.message_id == 0
                || item.author_id == 0
                || item.channel_id.is_none_or(|channel| channel == 0)
                || item.source_message_ids.contains(&0)
                || item
                    .source_text_segments
                    .iter()
                    .any(|segment| segment.message_id == 0)
            {
                return Err("reserved snapshot has an invalid zero identity".into());
            }
        }
        let identity = |item: &PendingQueueItem| QueueIdentity {
            primary: item.message_id,
            queued_generation: item.queued_generation,
            sources: (if item.source_message_ids.is_empty() {
                vec![item.message_id]
            } else {
                item.source_message_ids.clone()
            })
            .into_iter()
            .map(|message_id| SourceIdentity {
                message_id,
                queued_generation: item
                    .source_message_queued_generations
                    .iter()
                    .find(|owner| owner.message_id == message_id)
                    .map_or(item.queued_generation, |owner| owner.queued_generation),
            })
            .collect(),
        };
        if identity(first) != head.identity || identity(&head.original) != head.identity {
            return Err("reserved snapshot source identity differs from its original".into());
        }
        let channel = first.channel_id.ok_or("reserved snapshot has no channel")?;
        let item = super::super::pending_queue_persistence::pending_queue_item_to_intervention(
            first.clone(),
            std::time::SystemTime::now(),
            Instant::now(),
        );
        if !is_head(head, &item, ChannelId::new(channel)) {
            return Err("reserved snapshot payload differs from its original".into());
        }
    }
    Ok((snapshot.items, Some(snapshot.meta)))
}
