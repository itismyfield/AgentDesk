from pathlib import Path
import subprocess

ROOT = Path('.')

def replace_once(path: str, before: str, after: str) -> None:
    target = ROOT / path
    text = target.read_text()
    assert text.count(before) == 1, (path, text.count(before), before)
    target.write_text(text.replace(before, after, 1))

workflow = '.github/workflows/ci-pr.yml'
# Keep the derived block in exactly the generator's sorted order.
replace_once(workflow,
    "              - 'src/services/discord/relay_health.rs'\n",
    "              - 'src/services/discord/relay_coord.rs'\n              - 'src/services/discord/relay_health.rs'\n")

owner = 'src/services/discord/tmux_watcher/cancel_handoff.rs'
replace_once(owner,
    '            if pin.paused.load(Ordering::Acquire)\n',
    '            if self.cancel.load(Ordering::Acquire)\n                || pin.paused.load(Ordering::Acquire)\n')
replace_once(owner,
    '            Some(self.pending.remove(index))\n',
    '''            let pending = self.pending.remove(index);
            // Moving custody is not settlement. Keep an outgoing checkpoint
            // before the successor can await: cancellation before its first
            // collector checkpoint must not lose the only retained copy.
            let mut checkpoint = pending.clone();
            checkpoint.cancel = self.cancel.clone();
            self.checkpoint = Some(checkpoint);
            Some(pending)
''')
with (ROOT / owner).open('a') as handle:
    handle.write('\n#[cfg(test)]\n#[path = "cancel_handoff/interrupted_adoption_tests.rs"]\npub(super) mod interrupted_adoption_tests;\n')

native = 'src/services/discord/tmux_watcher/streaming_status_tick/native_collector_tests.rs'
replace_once(native,
    '                let saved = custody\n',
    '                let mut saved = custody\n')
replace_once(native,
    '                let mut saved = custody\n',
    '''                cancel_handoff::interrupted_adoption_tests::assert_admission_fences(&ctx, &mut custody);
                let mut saved = custody
''')
replace_once(native,
    '                let previous_sequence = relay\n',
    '''                if round == 0 {
                    (custody, saved) = cancel_handoff::interrupted_adoption_tests::interrupt_before_poll(
                        &mut ctx, custody, saved,
                    ).await;
                }
                let previous_sequence = relay
''')
helper = ROOT / 'src/services/discord/tmux_watcher/cancel_handoff/interrupted_adoption_tests.rs'
helper.parent.mkdir(exist_ok=True)
helper.write_text('''//! Exercised by the existing native collector -> sink -> receipt tests.
use super::*;

pub(in crate::services::discord) fn assert_admission_fences(
    ctx: &TurnStreamCollectorContext,
    custody: &mut Custody,
) {
    ctx.paused.store(true, Ordering::Release);
    assert!(custody.take_for_current(&ctx.shared, ctx.channel_id).is_none());
    ctx.paused.store(false, Ordering::Release);

    let resume = ctx.shared.tmux_watchers.get(&ctx.channel_id).unwrap().resume_offset.clone();
    *resume.lock().unwrap() = Some(0);
    assert!(custody.take_for_current(&ctx.shared, ctx.channel_id).is_none());
    *resume.lock().unwrap() = None;

    // Deterministic generation mismatch, without sleeping or relying on mtime
    // resolution. Restore the exact marker so the original obligation is usable.
    let marker = std::fs::File::open(crate::services::tmux_common::session_temp_path(
        &ctx.tmux_session_name, "generation",
    )).unwrap();
    let modified = marker.metadata().unwrap().modified().unwrap();
    marker.set_modified(modified + std::time::Duration::from_secs(1)).unwrap();
    assert!(custody.take_for_current(&ctx.shared, ctx.channel_id).is_none());
    marker.set_modified(modified).unwrap();
}

pub(in crate::services::discord) async fn interrupt_before_poll(
    ctx: &mut TurnStreamCollectorContext,
    custody: Custody,
    original: Pending,
) -> (Custody, Pending) {
    let frontier = ctx.shared.committed_relay_offset(ctx.channel_id);
    let before = crate::services::discord::inflight::load_inflight_state(
        &ctx.watcher_provider, ctx.channel_id.get(),
    ).map(|row| serde_json::to_value(row).unwrap());
    ctx.cancel.store(true, Ordering::Release);
    // No collector call and no manual checkpoint between adoption and drop.
    // The original implementation removed the capsule and lost it here.
    drop(custody);
    ctx.cancel = Arc::new(AtomicBool::new(false));
    ctx.shared.tmux_watchers.insert(ctx.channel_id, crate::services::discord::TmuxWatcherHandle {
        tmux_session_name: ctx.tmux_session_name.clone(),
        output_path: ctx.output_path.clone(),
        paused: ctx.paused.clone(),
        resume_offset: Arc::new(std::sync::Mutex::new(None)),
        cancel: ctx.cancel.clone(),
        pause_epoch: ctx.pause_epoch.clone(),
        turn_delivered: ctx.turn_delivered.clone(),
        last_heartbeat_ts_ms: ctx.last_heartbeat_ts_ms.clone(),
    });
    let mut next = Custody::acquire(
        &ctx.shared, ctx.channel_id, &ctx.watcher_provider,
        &ctx.tmux_session_name, &ctx.output_path, &ctx.cancel,
    ).await.unwrap();
    let saved = next.take_for_current(&ctx.shared, ctx.channel_id)
        .expect("adoption followed by cancellation before polling must retain custody");
    assert!(next.take_for_current(&ctx.shared, ctx.channel_id).is_none());
    assert!(Arc::ptr_eq(&saved.source, &original.source));
    assert_eq!(saved.offset, original.offset);
    assert_eq!(saved.buffer, original.buffer);
    assert_eq!(saved.buffer_start, original.buffer_start);
    assert_eq!(saved.utf8.has_pending(), original.utf8.has_pending());
    assert_eq!(saved.nonce, original.nonce);
    assert_eq!(saved.turn.as_ref().map(|turn| &turn.full_response), original.turn.as_ref().map(|turn| &turn.full_response));
    assert_eq!(saved.ack.as_ref().map(|ack| ack.sequence), original.ack.as_ref().map(|ack| ack.sequence));
    assert_eq!(ctx.shared.committed_relay_offset(ctx.channel_id), frontier,
        "adoption and re-cancellation are not delivery receipts");
    let after = crate::services::discord::inflight::load_inflight_state(
        &ctx.watcher_provider, ctx.channel_id.get(),
    ).map(|row| serde_json::to_value(row).unwrap());
    assert_eq!(after, before, "custody must not mutate or clear the inflight row");
    (next, saved)
}
''')

# Check that the added selector really belongs to the derived class.
globs = subprocess.check_output(['python3', 'scripts/cross_os_consumer_paths.py', '--format', 'globs'], text=True)
assert 'src/services/discord/relay_coord.rs' in globs
