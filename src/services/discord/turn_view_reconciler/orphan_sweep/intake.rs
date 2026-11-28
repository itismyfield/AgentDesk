use super::*;
use crate::services::discord::{mailbox_snapshot, recovery_known_ids};
use crate::services::maintenance::runtime_retention;
use crate::services::turn_orchestrator::{
    load_channel_pending_dispatch_marker_checked, queue_holds_source,
};
use std::path::Path;

const SOURCE: &str = "intake_view_retention";

pub(super) async fn sweep(
    reconciler: &TurnViewReconciler,
    shared: &SharedData,
    now: SystemTime,
    held: &(dyn Fn(u64, u64) -> bool + Send + Sync),
) -> usize {
    let Some(cutoff) = runtime_retention::cutoff(now) else {
        return 0;
    };
    let Some(root) = crate::services::discord::runtime_store::discord_turn_view_reconciler_root()
    else {
        return 0;
    };
    let dir = root.join(TurnViewTargetKind::IntakeUserMessage.as_str());
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut swept = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(target) = path
            .file_name()
            .and_then(|name| target_from_filename(name.to_str()?))
            && runtime_retention::regular_file_expired(&path, cutoff)
            && clear(reconciler, shared, target, &path, cutoff, held).await
        {
            swept += 1;
        }
    }
    swept
}

fn target_from_filename(name: &str) -> Option<TurnViewTarget> {
    let (channel, message) = name.strip_suffix(".json")?.split_once('-')?;
    let (channel, message): (u64, u64) = (channel.parse().ok()?, message.parse().ok()?);
    if channel == 0 || message == 0 || name != format!("{channel}-{message}.json") {
        return None;
    }
    Some(TurnViewTarget::intake_user_message(
        ChannelId::new(channel),
        MessageId::new(message),
    ))
}

async fn clear(
    reconciler: &TurnViewReconciler,
    shared: &SharedData,
    target: TurnViewTarget,
    path: &Path,
    cutoff: SystemTime,
    held: &(dyn Fn(u64, u64) -> bool + Send + Sync),
) -> bool {
    let lock = reconciler.target_lock(target);
    let guard = lock.lock().await;
    let swept = clear_locked(reconciler, shared, target, path, cutoff, held, &lock).await;
    drop(guard);
    drop(lock);
    reconciler.prune_target_lock_if_idle(target);
    swept
}

async fn clear_locked(
    reconciler: &TurnViewReconciler,
    shared: &SharedData,
    target: TurnViewTarget,
    path: &Path,
    cutoff: SystemTime,
    held: &(dyn Fn(u64, u64) -> bool + Send + Sync),
    lock: &Arc<tokio::sync::Mutex<()>>,
) -> bool {
    if !runtime_retention::regular_file_expired(path, cutoff) {
        return false;
    }
    let Some(record) = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<PersistedTargetState>(&bytes).ok())
    else {
        return false;
    };
    if record.provider != shared.provider.as_str()
        || record.kind != target.kind.as_str()
        || record.channel_id != target.channel_id.get()
        || record.message_id != target.message_id.get()
        || !matches!(
            record.version,
            PERSISTED_STATE_VERSION
                | LEGACY_QUEUED_HOURGLASS_STATE_VERSION
                | QUEUED_MARKER_ONLY_STATE_VERSION
                | LIVE_QUEUED_MARKER_STATE_VERSION
        )
    {
        return false;
    }
    if mailbox_holds(shared, target).await
        || disk_holds(shared, target, record.token_hash.as_deref())
        || held(target.channel_id.get(), target.message_id.get())
        || !runtime_retention::regular_file_expired(path, cutoff)
    {
        return false;
    }
    let Some(current) = reconciler.load_persisted_target(target, shared, SOURCE) else {
        return false;
    };
    if !current.applied.terminal()
        && reconciler
            .apply_diff_or_cold_terminal(
                shared,
                target,
                current.applied,
                TurnViewState::None,
                false,
                &current.legacy_queue_reactions,
                &current.identity,
                SOURCE,
            )
            .await
            == TurnViewDelivery::Failed
    {
        return false;
    }
    reconciler.discard_target_locked(target, SOURCE, lock);
    matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

async fn mailbox_holds(shared: &SharedData, target: TurnViewTarget) -> bool {
    let snapshot = mailbox_snapshot(shared, target.channel_id).await;
    let (_, known) = recovery_known_ids::recovery_known_arms_and_ids(&snapshot);
    known.contains(&target.message_id.get())
        || snapshot
            .v2_queue
            .is_some_and(|(depth, held)| depth > 0 || held)
}

fn disk_holds(shared: &SharedData, target: TurnViewTarget, persisted_token: Option<&str>) -> bool {
    let channel = target.channel_id;
    let message = target.message_id;
    let provider = &shared.provider;
    let owned = |token: &str| {
        load_channel_pending_dispatch_marker_checked(provider, token, channel).map_or(
            true,
            |marker| {
                marker.is_some_and(|(item, _)| {
                    item.message_id == message || item.source_message_ids.contains(&message)
                })
            },
        ) || !matches!(
            queue_holds_source(provider, token, channel, message),
            Ok(false)
        )
    };
    owned(&shared.token_hash)
        || persisted_token
            .filter(|token| *token != shared.token_hash)
            .is_some_and(|token| {
                let namespaced_hash = token.strip_prefix("discord_").is_some_and(|hash| {
                    hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                });
                !namespaced_hash || owned(token)
            })
}
