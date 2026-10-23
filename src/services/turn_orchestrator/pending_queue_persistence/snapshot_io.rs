//! Legacy snapshot I/O refuses a future snapshot before any publish or removal.
use super::*;

pub(super) fn save_channel_queue_locked(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
    queue: &[Intervention],
    meta: Option<&SnapshotMeta>,
    dispatch_role_override: Option<u64>,
) -> Result<(), String> {
    let Some(path) = pending_queue_file_path(provider, token_hash, channel_id) else {
        return Err(format!(
            "pending queue root unavailable for provider={} token_hash={} channel_id={}",
            provider.as_str(),
            token_hash,
            channel_id.get()
        ));
    };
    super::super::queued_head::compat::require_legacy_write(&path)?;
    #[cfg(test)]
    if save_fault::take(channel_id) {
        return Err(format!(
            "injected pending queue save failure {}",
            path.display()
        ));
    }
    if queue.is_empty() && meta.is_none() {
        return match fs::remove_file(&path).map(|()| runtime_store::sync_removed(&path)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                let message = format!("remove pending queue file {}: {error}", path.display());
                tracing::error!(
                    provider = provider.as_str(),
                    token_hash,
                    channel_id = channel_id.get(),
                    path = %path.display(),
                    error = %message,
                    "recovery-critical pending queue removal failed"
                );
                Err(message)
            }
        };
    }
    let reference_instant = Instant::now();
    let reference_wall_time = SystemTime::now();
    let items: Vec<PendingQueueItem> = queue
        .iter()
        .map(|intervention| {
            pending_queue_item_from_intervention(
                intervention,
                channel_id,
                dispatch_role_override,
                reference_wall_time,
                reference_instant,
            )
        })
        .collect();
    let json = super::super::queued_head::snapshot_json(items, meta)
        .map_err(|error| format!("serialize pending queue {}: {error}", path.display()))?;
    let context = runtime_store::AtomicWriteContext::new("discord_pending_queue")
        .provider(provider.as_str())
        .token_hash(token_hash)
        .channel_id(channel_id.get());
    runtime_store::critical_publish(&path, &json, context)?;
    #[cfg(test)]
    if post_save_fault::take(channel_id) {
        return Err(format!(
            "injected error after queue publication {}",
            path.display()
        ));
    }
    Ok(())
}

pub(super) fn load_channel_pending_queue_checked(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: ChannelId,
) -> Result<(Vec<Intervention>, Option<ChannelId>), String> {
    let path = pending_queue_file_path(provider, token_hash, channel_id).ok_or_else(|| {
        format!(
            "pending queue root unavailable for provider={} token_hash={token_hash} channel_id={}",
            provider.as_str(),
            channel_id.get()
        )
    })?;
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), None));
        }
        Err(error) => {
            return Err(format!(
                "read pending queue file {}: {error}",
                path.display()
            ));
        }
    };
    let (items, _) = parse_snapshot(&content)
        .map_err(|error| format!("parse pending queue file {}: {error}", path.display()))?;
    let restored_override = items
        .iter()
        .find_map(|item| item.override_channel_id)
        .map(ChannelId::new);
    let reference_instant = Instant::now();
    let reference_wall_time = SystemTime::now();
    let interventions =
        pending_queue_items_to_interventions(items, reference_wall_time, reference_instant);
    Ok((interventions, restored_override))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_writer_cannot_remove_or_replace_v2_even_with_v1_metadata() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let channel = ChannelId::new(7_272_290);
        let path = pending_queue_file_path(&ProviderKind::Claude, "v2-writer", channel).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = br#"{"version":2,"items":[],"retained_inputs":[{"original":"A"}]}"#;
        fs::write(&path, bytes).unwrap();
        let meta = SnapshotMeta::default();
        for meta in [None, Some(&meta)] {
            assert!(
                save_channel_queue_locked(
                    &ProviderKind::Claude,
                    "v2-writer",
                    channel,
                    &[],
                    meta,
                    None
                )
                .is_err(),
                "a V1 metadata reference does not grant V2 write authority"
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                bytes,
                "read-only refusal precedes empty removal and V1 publication"
            );
        }
    }
}
