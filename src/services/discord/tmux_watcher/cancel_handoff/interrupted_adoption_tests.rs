//! Exercised by the existing native collector -> sink -> receipt tests.
use super::*;

pub(in crate::services::discord) fn assert_admission_fences(
    ctx: &TurnStreamCollectorContext,
    custody: &mut Custody,
) {
    ctx.paused.store(true, Ordering::Release);
    assert!(
        custody
            .take_for_current(&ctx.shared, ctx.channel_id)
            .is_none()
    );
    ctx.paused.store(false, Ordering::Release);

    let resume = ctx
        .shared
        .tmux_watchers
        .get(&ctx.channel_id)
        .unwrap()
        .resume_offset
        .clone();
    *resume.lock().unwrap() = Some(0);
    assert!(
        custody
            .take_for_current(&ctx.shared, ctx.channel_id)
            .is_none()
    );
    *resume.lock().unwrap() = None;

    // Deterministic generation mismatch, without sleeping or relying on mtime
    // resolution. Restore the exact marker so the original obligation is usable.
    let marker = std::fs::File::open(crate::services::tmux_common::session_temp_path(
        &ctx.tmux_session_name,
        "generation",
    ))
    .unwrap();
    let modified = marker.metadata().unwrap().modified().unwrap();
    marker
        .set_modified(modified + std::time::Duration::from_secs(1))
        .unwrap();
    assert!(
        custody
            .take_for_current(&ctx.shared, ctx.channel_id)
            .is_none()
    );
    marker.set_modified(modified).unwrap();
}

pub(in crate::services::discord) async fn interrupt_before_poll(
    ctx: &mut TurnStreamCollectorContext,
    custody: Custody,
    original: Pending,
) -> (Custody, Pending) {
    let frontier = ctx.shared.committed_relay_offset(ctx.channel_id);
    let before = crate::services::discord::inflight::load_inflight_state(
        &ctx.watcher_provider,
        ctx.channel_id.get(),
    )
    .map(|row| serde_json::to_value(row).unwrap());
    ctx.cancel.store(true, Ordering::Release);
    // No collector call and no manual checkpoint between adoption and drop.
    // The original implementation removed the capsule and lost it here.
    drop(custody);
    ctx.cancel = Arc::new(AtomicBool::new(false));
    ctx.shared.tmux_watchers.insert(
        ctx.channel_id,
        crate::services::discord::TmuxWatcherHandle {
            tmux_session_name: ctx.tmux_session_name.clone(),
            output_path: ctx.output_path.clone(),
            paused: ctx.paused.clone(),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: ctx.cancel.clone(),
            pause_epoch: ctx.pause_epoch.clone(),
            turn_delivered: ctx.turn_delivered.clone(),
            last_heartbeat_ts_ms: ctx.last_heartbeat_ts_ms.clone(),
        },
    );
    let mut next = Custody::acquire(
        &ctx.shared,
        ctx.channel_id,
        &ctx.watcher_provider,
        &ctx.tmux_session_name,
        &ctx.output_path,
        &ctx.cancel,
    )
    .await
    .unwrap();
    let saved = next
        .take_for_current(&ctx.shared, ctx.channel_id)
        .expect("adoption followed by cancellation before polling must retain custody");
    assert!(next.take_for_current(&ctx.shared, ctx.channel_id).is_none());
    assert!(Arc::ptr_eq(&saved.source, &original.source));
    assert_eq!(saved.offset, original.offset);
    assert_eq!(saved.buffer, original.buffer);
    assert_eq!(saved.buffer_start, original.buffer_start);
    assert_eq!(saved.utf8.has_pending(), original.utf8.has_pending());
    assert_eq!(saved.nonce, original.nonce);
    assert_eq!(
        saved.turn.as_ref().map(|turn| &turn.full_response),
        original.turn.as_ref().map(|turn| &turn.full_response)
    );
    assert_eq!(
        saved.ack.as_ref().map(|ack| ack.sequence),
        original.ack.as_ref().map(|ack| ack.sequence)
    );
    assert_eq!(
        ctx.shared.committed_relay_offset(ctx.channel_id),
        frontier,
        "adoption and re-cancellation are not delivery receipts"
    );
    let after = crate::services::discord::inflight::load_inflight_state(
        &ctx.watcher_provider,
        ctx.channel_id.get(),
    )
    .map(|row| serde_json::to_value(row).unwrap());
    assert_eq!(
        after, before,
        "custody must not mutate or clear the inflight row"
    );
    (next, saved)
}
