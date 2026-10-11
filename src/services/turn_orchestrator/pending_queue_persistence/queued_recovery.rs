use super::{
    ChannelId, Instant, PendingQueueRecovery, ProviderKind, STALE_PENDING_QUEUE_TMP_AGE,
    SystemTime, cleanup_stale_pending_queue_tmp_files_in_dir, fs, parse_snapshot,
    pending_dispatch_marker_channel_id, pending_queue_items_to_interventions, pending_queue_root,
};

pub(crate) fn load_pending_queues_checked(
    provider: &ProviderKind,
    token_hash: &str,
) -> PendingQueueRecovery {
    let mut recovery = PendingQueueRecovery::default();
    let Some(root) = pending_queue_root() else {
        return recovery;
    };
    let dir = root.join(provider.as_str()).join(token_hash);
    let _ = cleanup_stale_pending_queue_tmp_files_in_dir(
        provider,
        token_hash,
        &dir,
        SystemTime::now(),
        STALE_PENDING_QUEUE_TMP_AGE,
    );
    let Ok(entries) = fs::read_dir(&dir) else {
        return recovery;
    };
    let reference_instant = Instant::now();
    let reference_wall_time = SystemTime::now();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if pending_dispatch_marker_channel_id(&path).is_some() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let channel_id: u64 = match path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse().ok())
        {
            Some(id) => id,
            None => continue,
        };
        let items = match fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))
            .and_then(|content| parse_snapshot(&content).map(|(items, _)| items))
        {
            Ok(items) => items,
            Err(error) => {
                recovery
                    .quarantined
                    .push((ChannelId::new(channel_id), error));
                continue;
            }
        };
        if let Some(override_id) = items.iter().find_map(|item| item.override_channel_id) {
            recovery
                .overrides
                .insert(ChannelId::new(channel_id), ChannelId::new(override_id));
        }
        let interventions =
            pending_queue_items_to_interventions(items, reference_wall_time, reference_instant);
        if !interventions.is_empty() {
            recovery
                .queues
                .insert(ChannelId::new(channel_id), interventions);
        }
    }
    recovery
}
