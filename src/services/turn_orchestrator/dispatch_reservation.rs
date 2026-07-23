use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::active_source_dedup::strip_source_message_id_from_intervention;
use super::pending_queue_persistence::{
    load_channel_pending_dispatch_marker, load_channel_pending_queue,
    remove_channel_pending_dispatch_marker,
};
use super::{
    ChannelMailboxState, DispatchLease, HydratePendingQueueResult, Intervention, InterventionMode,
    QueuePersistenceContext, TakeNextSoftResult, persist_queue_or_restore,
};

pub(crate) const PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER: Duration = Duration::from_secs(10);
pub(crate) const VALVE_CLEARED_DISPATCH_MARKER_GRACE: Duration = Duration::from_secs(600);

pub(super) fn set_pending_user_dispatch(
    state: &mut ChannelMailboxState,
    message_id: MessageId,
) -> Arc<DispatchLease> {
    let lease = Arc::new(DispatchLease);
    state.pending_user_dispatch = Some(message_id);
    state.pending_user_dispatch_since = Some(Instant::now());
    state.pending_user_dispatch_yield_count = 0;
    state.pending_user_dispatch_lease = Some(Arc::clone(&lease));
    lease
}

pub(super) fn clear_pending_user_dispatch(state: &mut ChannelMailboxState) -> Option<MessageId> {
    let cleared = state.pending_user_dispatch.take();
    state.pending_user_dispatch_since = None;
    state.pending_user_dispatch_yield_count = 0;
    state.pending_user_dispatch_lease = None;
    cleared
}

fn clear_pending_user_dispatch_if_matches(
    state: &mut ChannelMailboxState,
    message_id: MessageId,
) -> bool {
    if state.pending_user_dispatch != Some(message_id) {
        return false;
    }
    clear_pending_user_dispatch(state);
    true
}

pub(super) fn record_valve_cleared_pending_dispatch(
    state: &mut ChannelMailboxState,
    message_id: MessageId,
) {
    state.recently_valve_cleared_dispatch = Some((message_id, Instant::now()));
}

pub(super) fn clear_recently_valve_cleared_dispatch_if_matches(
    state: &mut ChannelMailboxState,
    message_id: MessageId,
) {
    if state
        .recently_valve_cleared_dispatch
        .is_some_and(|(cleared_id, _)| cleared_id == message_id)
    {
        state.recently_valve_cleared_dispatch = None;
    }
}

fn marker_recently_valve_cleared(state: &ChannelMailboxState, marker_id: MessageId) -> bool {
    state
        .recently_valve_cleared_dispatch
        .is_some_and(|(cleared_id, cleared_at)| {
            cleared_id == marker_id && cleared_at.elapsed() < VALVE_CLEARED_DISPATCH_MARKER_GRACE
        })
}

fn queued_intervention_contains_message_id(queue: &[Intervention], message_id: MessageId) -> bool {
    queue
        .iter()
        .any(|item| intervention_dedup_ids(item).contains(&message_id))
}

pub(super) fn delete_pending_dispatch_marker_with_persistence(
    persistence: &QueuePersistenceContext,
    channel_id: ChannelId,
    operation: &str,
) {
    if let Err(error) = remove_channel_pending_dispatch_marker(
        &persistence.provider,
        &persistence.token_hash,
        channel_id,
    ) {
        tracing::warn!(
            operation,
            provider = persistence.provider.as_str(),
            token_hash = %persistence.token_hash,
            channel_id = channel_id.get(),
            error = %error,
            "failed to remove pending dispatch marker"
        );
    }
}

pub(super) fn consume_pending_dispatch_marker_if_matches(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    user_message_id: MessageId,
    operation: &str,
) {
    let Some(persistence) = state.last_persistence.as_ref() else {
        return;
    };
    let Some((marker, _)) = load_channel_pending_dispatch_marker(
        &persistence.provider,
        &persistence.token_hash,
        channel_id,
    ) else {
        return;
    };
    if marker.message_id == user_message_id {
        delete_pending_dispatch_marker_with_persistence(persistence, channel_id, operation);
        clear_recently_valve_cleared_dispatch_if_matches(state, user_message_id);
    }
}

pub(super) fn abandon_pending_dispatch_reservation(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    user_message_id: MessageId,
    consume_marker: bool,
    operation: &str,
) {
    let matched = clear_pending_user_dispatch_if_matches(state, user_message_id);
    if consume_marker {
        consume_pending_dispatch_marker_if_matches(state, channel_id, user_message_id, operation);
    }
    if matched {
        tracing::warn!(
            operation,
            channel_id = channel_id.get(),
            user_message_id = user_message_id.get(),
            consume_marker,
            "cleared abandoned pending dispatch reservation"
        );
    }
}

pub(super) fn clear_stale_pending_dispatch_reservation(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
) -> Option<MessageId> {
    let reserved_id = state.pending_user_dispatch?;
    let Some(stored_lease) = state.pending_user_dispatch_lease.as_ref() else {
        return None;
    };
    let reservation_age = state
        .pending_user_dispatch_since
        .map(|since| since.elapsed())
        .unwrap_or_default();
    if state.cancel_token.is_some()
        || Arc::strong_count(stored_lease) > 1
        || reservation_age < PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER
    {
        return None;
    }
    clear_pending_user_dispatch(state);
    tracing::warn!(
        channel_id = channel_id.get(),
        user_message_id = reserved_id.get(),
        reservation_age_ms = reservation_age.as_millis(),
        "cleared orphaned pending dispatch reservation before marker self-heal"
    );
    Some(reserved_id)
}

pub(super) fn pending_dispatch_lease_is_orphaned(state: &ChannelMailboxState) -> bool {
    let Some(stored_lease) = state.pending_user_dispatch_lease.as_ref() else {
        return false;
    };
    let reservation_age = state
        .pending_user_dispatch_since
        .map(|since| since.elapsed())
        .unwrap_or_default();
    state.pending_user_dispatch.is_some()
        && state.cancel_token.is_none()
        && Arc::strong_count(stored_lease) == 1
        && reservation_age >= PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER
}

fn intervention_dedup_ids(item: &Intervention) -> Vec<MessageId> {
    let mut ids: Vec<MessageId> = item.source_message_ids.clone();
    if !ids.contains(&item.message_id) {
        ids.push(item.message_id);
    }
    ids
}

pub(super) fn hydrate_pending_queue_into_state(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    disk_items: Vec<Intervention>,
    persistence: QueuePersistenceContext,
    restored_override: Option<ChannelId>,
) -> HydratePendingQueueResult {
    state.last_persistence = Some(persistence.clone());
    let previous_queue = state.intervention_queue.clone();
    let mut existing_ids: HashSet<MessageId> = state
        .intervention_queue
        .iter()
        .flat_map(intervention_dedup_ids)
        .collect();
    let mut absorbed = 0usize;
    for mut item in disk_items.into_iter().rev() {
        let item_ids = intervention_dedup_ids(&item);
        let mut stripped_existing_source = false;
        for existing_id in item_ids
            .iter()
            .copied()
            .filter(|id| existing_ids.contains(id))
        {
            strip_source_message_id_from_intervention(&mut item, existing_id);
            stripped_existing_source = true;
        }
        if stripped_existing_source
            && (item.source_message_ids.is_empty()
                || (item.text.trim().is_empty()
                    && item.pending_uploads.is_empty()
                    && item.voice_announcement.is_none()))
        {
            continue;
        }
        let remaining_ids = intervention_dedup_ids(&item);
        existing_ids.extend(remaining_ids);
        state.intervention_queue.insert(0, item);
        absorbed += 1;
    }
    if absorbed > 0
        && let Err(error) = persist_queue_or_restore(
            state,
            channel_id,
            &persistence,
            previous_queue,
            "hydrate_pending_queue_from_disk",
        )
    {
        return HydratePendingQueueResult {
            absorbed: 0,
            queue_len_after: state.intervention_queue.len(),
            restored_override,
            persistence_error: Some(error),
        };
    }
    HydratePendingQueueResult {
        absorbed,
        queue_len_after: state.intervention_queue.len(),
        restored_override,
        persistence_error: None,
    }
}

pub(super) fn merge_pending_dispatch_marker_into_state(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    marker: Intervention,
    persistence: QueuePersistenceContext,
    restored_override: Option<ChannelId>,
    operation: &'static str,
) -> HydratePendingQueueResult {
    state.last_persistence = Some(persistence.clone());
    let marker_id = marker.message_id;
    let marker_ids = intervention_dedup_ids(&marker);
    let marker_already_present = marker_ids.iter().all(|id| {
        queued_intervention_contains_message_id(&state.intervention_queue, *id)
            || state.active_user_message_id == Some(*id)
    });
    if marker_already_present {
        delete_pending_dispatch_marker_with_persistence(&persistence, channel_id, operation);
        clear_recently_valve_cleared_dispatch_if_matches(state, marker_id);
        return HydratePendingQueueResult {
            absorbed: 0,
            queue_len_after: state.intervention_queue.len(),
            restored_override,
            persistence_error: None,
        };
    }
    if marker_recently_valve_cleared(state, marker_id) {
        return HydratePendingQueueResult {
            absorbed: 0,
            queue_len_after: state.intervention_queue.len(),
            restored_override,
            persistence_error: None,
        };
    }

    let previous_queue = state.intervention_queue.clone();
    state.intervention_queue.insert(0, marker);
    if let Err(error) =
        persist_queue_or_restore(state, channel_id, &persistence, previous_queue, operation)
    {
        return HydratePendingQueueResult {
            absorbed: 0,
            queue_len_after: state.intervention_queue.len(),
            restored_override,
            persistence_error: Some(error),
        };
    }
    delete_pending_dispatch_marker_with_persistence(&persistence, channel_id, operation);
    clear_recently_valve_cleared_dispatch_if_matches(state, marker_id);
    HydratePendingQueueResult {
        absorbed: 1,
        queue_len_after: state.intervention_queue.len(),
        restored_override,
        persistence_error: None,
    }
}

pub(super) fn hydrate_pending_queue_from_disk_if_present(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> HydratePendingQueueResult {
    let (disk_items, mut restored_override) =
        load_channel_pending_queue(&persistence.provider, &persistence.token_hash, channel_id);

    let mut effective_persistence = persistence.clone();
    if effective_persistence.dispatch_role_override.is_none() {
        effective_persistence.dispatch_role_override =
            restored_override.map(|channel| channel.get());
    }
    let mut result = if disk_items.is_empty() {
        HydratePendingQueueResult {
            absorbed: 0,
            queue_len_after: state.intervention_queue.len(),
            restored_override,
            persistence_error: None,
        }
    } else {
        hydrate_pending_queue_into_state(
            state,
            channel_id,
            disk_items,
            effective_persistence.clone(),
            restored_override,
        )
    };
    if result.persistence_error.is_some() || state.pending_user_dispatch.is_some() {
        return result;
    }
    restored_override = result.restored_override;
    let Some((marker, marker_override)) = load_channel_pending_dispatch_marker(
        &persistence.provider,
        &persistence.token_hash,
        channel_id,
    ) else {
        return result;
    };
    if restored_override.is_none() {
        restored_override = marker_override;
    }
    if effective_persistence.dispatch_role_override.is_none() {
        effective_persistence.dispatch_role_override =
            restored_override.map(|channel| channel.get());
    }
    let marker_result = merge_pending_dispatch_marker_into_state(
        state,
        channel_id,
        marker,
        effective_persistence,
        restored_override,
        "hydrate_pending_dispatch_marker",
    );
    result.absorbed += marker_result.absorbed;
    result.queue_len_after = marker_result.queue_len_after;
    result.restored_override = marker_result.restored_override;
    result.persistence_error = marker_result.persistence_error;
    result
}

pub(super) fn reconcile_pending_dispatch_marker_before_take_next(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> Option<TakeNextSoftResult> {
    if state.pending_user_dispatch.is_some() {
        return Some(TakeNextSoftResult {
            intervention: None,
            dispatch_lease: None,
            has_more: state
                .intervention_queue
                .iter()
                .any(|item| item.mode == InterventionMode::Soft),
            queue_len_after: state.intervention_queue.len(),
            queue_exit_events: Vec::new(),
            persistence_error: None,
        });
    }

    let Some((mut marker, marker_override)) = load_channel_pending_dispatch_marker(
        &persistence.provider,
        &persistence.token_hash,
        channel_id,
    ) else {
        return None;
    };
    let marker_id = marker.message_id;
    let marker_ids = intervention_dedup_ids(&marker);
    let stale_duplicate = marker_ids.iter().all(|id| {
        queued_intervention_contains_message_id(&state.intervention_queue, *id)
            || state.active_user_message_id == Some(*id)
    });
    if stale_duplicate {
        delete_pending_dispatch_marker_with_persistence(
            persistence,
            channel_id,
            "take_next_soft_stale_marker",
        );
        clear_recently_valve_cleared_dispatch_if_matches(state, marker_id);
        return None;
    }
    if marker_recently_valve_cleared(state, marker_id) {
        return None;
    }
    let mut stripped_existing_source = false;
    for present_id in marker_ids.iter().copied().filter(|id| {
        queued_intervention_contains_message_id(&state.intervention_queue, *id)
            || state.active_user_message_id == Some(*id)
    }) {
        strip_source_message_id_from_intervention(&mut marker, present_id);
        stripped_existing_source = true;
    }
    if stripped_existing_source
        && (marker.source_message_ids.is_empty()
            || (marker.text.trim().is_empty()
                && marker.pending_uploads.is_empty()
                && marker.voice_announcement.is_none()))
    {
        delete_pending_dispatch_marker_with_persistence(
            persistence,
            channel_id,
            "take_next_soft_stale_marker",
        );
        clear_recently_valve_cleared_dispatch_if_matches(state, marker_id);
        return None;
    }

    let mut effective_persistence = persistence.clone();
    if effective_persistence.dispatch_role_override.is_none() {
        effective_persistence.dispatch_role_override = marker_override.map(|channel| channel.get());
    }
    let marker_result = merge_pending_dispatch_marker_into_state(
        state,
        channel_id,
        marker,
        effective_persistence,
        marker_override,
        "take_next_soft_restore_marker",
    );
    if let Some(error) = marker_result.persistence_error {
        return Some(TakeNextSoftResult {
            intervention: None,
            dispatch_lease: None,
            has_more: state
                .intervention_queue
                .iter()
                .any(|item| item.mode == InterventionMode::Soft),
            queue_len_after: state.intervention_queue.len(),
            queue_exit_events: Vec::new(),
            persistence_error: Some(error),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Instant;

    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    use super::super::pending_queue_persistence::{
        load_channel_pending_dispatch_marker, load_channel_pending_queue,
        save_channel_pending_dispatch_marker,
    };
    use super::super::test_support::{AGENTDESK_ROOT_DIR_ENV, lock_test_env};
    use super::super::{
        ChannelMailboxRegistry, ChannelMailboxState, Intervention, InterventionMode,
        QueuePersistenceContext,
    };
    use super::*;
    use crate::services::provider::ProviderKind;

    struct EnvGuard {
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set_root(root: &Path) -> Self {
            let previous = std::env::var(AGENTDESK_ROOT_DIR_ENV).ok();
            unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, root) };
            Self { previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.as_ref() {
                unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, previous) };
            } else {
                unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
            }
        }
    }

    fn make_intervention(
        message_id: MessageId,
        source_ids: &[MessageId],
        text: &str,
    ) -> Intervention {
        Intervention {
            author_id: UserId::new(100),
            author_is_bot: false,
            message_id,
            queued_generation: crate::services::discord::runtime_store::process_generation(),
            source_message_ids: source_ids.to_vec(),
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    fn single_source_intervention(message_id: MessageId, text: &str) -> Intervention {
        make_intervention(message_id, &[message_id], text)
    }

    fn run_async<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    #[test]
    fn hydrate_skips_empty_text_after_legacy_head_source_strip() {
        let _lock = lock_test_env();
        let provider = ProviderKind::Claude;
        let persistence =
            QueuePersistenceContext::new(&provider, "hydrate-empty-after-legacy-head-strip", None);
        let channel_id = ChannelId::new(4_132_301);
        let head_id = MessageId::new(4_132_302);
        let tail_id = MessageId::new(4_132_303);
        let mut state = ChannelMailboxState::default();
        state
            .intervention_queue
            .push(single_source_intervention(head_id, "already queued head"));
        let legacy_merged = make_intervention(
            tail_id,
            &[head_id, tail_id],
            "legacy head body\nextra legacy line\nambiguous tail",
        );

        let result = hydrate_pending_queue_into_state(
            &mut state,
            channel_id,
            vec![legacy_merged],
            persistence,
            None,
        );

        assert_eq!(result.absorbed, 0);
        assert_eq!(result.queue_len_after, 1);
        assert_eq!(state.intervention_queue.len(), 1);
        assert_eq!(state.intervention_queue[0].message_id, head_id);
        assert!(
            state
                .intervention_queue
                .iter()
                .all(|item| !item.text.trim().is_empty()),
            "hydrate must not insert an empty-text item after stripping a legacy head source"
        );
    }

    #[test]
    fn hydrate_preserves_captionless_image_after_legacy_head_source_strip() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        let provider = ProviderKind::Claude;
        let token_hash = "hydrate-captionless-image-after-legacy-head-strip";
        let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
        let channel_id = ChannelId::new(4_132_304);
        let text_id = MessageId::new(4_132_305);
        let image_id = MessageId::new(4_132_306);
        let image_upload = "attachment://captionless-image.png".to_string();
        let mut state = ChannelMailboxState::default();
        state
            .intervention_queue
            .push(single_source_intervention(text_id, "already queued text"));
        let mut legacy_merged =
            make_intervention(image_id, &[text_id, image_id], "already queued text\n");
        legacy_merged.pending_uploads = vec![image_upload.clone()];

        let result = hydrate_pending_queue_into_state(
            &mut state,
            channel_id,
            vec![legacy_merged],
            persistence,
            None,
        );

        assert_eq!(result.absorbed, 1);
        assert_eq!(result.queue_len_after, 2);
        assert!(result.persistence_error.is_none());
        let image = state
            .intervention_queue
            .iter()
            .find(|item| item.message_id == image_id)
            .expect("captionless image source must survive hydrate stripping");
        assert_eq!(image.source_message_ids, vec![image_id]);
        assert!(image.text.trim().is_empty());
        assert_eq!(image.pending_uploads, vec![image_upload]);
    }

    #[test]
    fn take_next_soft_restores_partial_merged_marker_when_only_head_source_present() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        run_async(async {
            let provider = ProviderKind::Claude;
            let token_hash = "take-next-partial-marker-source";
            let channel_id = ChannelId::new(4_132_311);
            let source_a = MessageId::new(4_132_312);
            let source_b = MessageId::new(4_132_313);
            let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel_id);
            let queued_b = single_source_intervention(source_b, "queued b");
            handle
                .replace_queue(vec![queued_b.clone()], persistence.clone())
                .await;
            let marker = make_intervention(source_b, &[source_a, source_b], "marker a\nmarker b");
            save_channel_pending_dispatch_marker(&provider, token_hash, channel_id, &marker, None)
                .unwrap();

            let taken = handle.take_next_soft(persistence.clone()).await;

            let intervention = taken
                .intervention
                .expect("source A from the partial merged marker must be restored and dequeued");
            assert_eq!(intervention.message_id, source_a);
            assert_eq!(intervention.source_message_ids, vec![source_a]);
            assert_eq!(intervention.text, "marker a");
            assert!(
                !intervention.text.contains("marker b"),
                "already queued source B must be stripped before marker restore"
            );
            assert_eq!(taken.queue_len_after, 1);

            let queue = handle.snapshot().await.intervention_queue;
            assert_eq!(queue.len(), 1);
            assert_eq!(queue[0].message_id, source_b);
            assert_eq!(queue[0].source_message_ids, vec![source_b]);

            let (disk, _) = load_channel_pending_queue(&provider, token_hash, channel_id);
            assert_eq!(disk.len(), 1);
            assert_eq!(disk[0].message_id, source_b);
            assert_eq!(disk[0].source_message_ids, vec![source_b]);

            let (marker_after, _) =
                load_channel_pending_dispatch_marker(&provider, token_hash, channel_id)
                    .expect("dequeued source A should leave a pending dispatch marker");
            assert_eq!(marker_after.message_id, source_a);
            assert_eq!(marker_after.source_message_ids, vec![source_a]);
            assert_eq!(marker_after.text, "marker a");
        });
    }

    #[test]
    fn take_next_soft_does_not_dispatch_empty_legacy_partial_marker_remainder() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        let _env_guard = EnvGuard::set_root(tmp.path());

        run_async(async {
            let provider = ProviderKind::Claude;
            let token_hash = "take-next-empty-partial-marker-remainder";
            let channel_id = ChannelId::new(4_132_321);
            let source_a = MessageId::new(4_132_322);
            let source_b = MessageId::new(4_132_323);
            let persistence = QueuePersistenceContext::new(&provider, token_hash, None);
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel_id);
            let queued_a = single_source_intervention(source_a, "queued a");
            handle
                .replace_queue(vec![queued_a.clone()], persistence.clone())
                .await;
            let marker = make_intervention(
                source_b,
                &[source_a, source_b],
                "legacy head body\nextra legacy line\nambiguous tail",
            );
            save_channel_pending_dispatch_marker(&provider, token_hash, channel_id, &marker, None)
                .unwrap();

            let taken = handle.take_next_soft(persistence.clone()).await;

            let intervention = taken
                .intervention
                .expect("queued source A should dispatch after stale marker remainder is consumed");
            assert_eq!(intervention.message_id, source_a);
            assert_eq!(intervention.source_message_ids, vec![source_a]);
            assert_eq!(intervention.text, "queued a");
            assert!(
                !intervention.text.trim().is_empty(),
                "legacy marker remainder must not dispatch as an empty intervention"
            );
            assert_eq!(taken.queue_len_after, 0);

            let queue = handle.snapshot().await.intervention_queue;
            assert!(queue.is_empty());

            let (marker_after, _) =
                load_channel_pending_dispatch_marker(&provider, token_hash, channel_id)
                    .expect("dequeued source A should write its own pending dispatch marker");
            assert_eq!(marker_after.message_id, source_a);
            assert_eq!(marker_after.source_message_ids, vec![source_a]);
            assert_eq!(marker_after.text, "queued a");
        });
    }
}
