use super::*;
use crate::services::discord::TmuxWatcherHandle;
use crate::services::discord::task_supervisor::spawn_observed_tmux_watcher;
use crate::services::discord::tmux::tmux_watcher::cancel_handoff::capsule_observation;
use crate::services::discord::tmux::{WatchHost, claim_or_reuse_watcher_for_host};
use std::sync::atomic::{AtomicI64, AtomicU64};

pub(super) async fn fixture_with_real_legacy_custody() -> (
    InstallFixture,
    test_support::MockDiscord,
    writer::WriterFixture,
    Vec<capsule_observation::Summary>,
    Option<serde_json::Value>,
) {
    let discord = test_support::MockDiscord::start().await;
    let fixture = configured_install_fixture(discord.http.clone()).await;
    let owned = cutover::test_override::force_channels(&[]);
    let mut row = inflight_with_identity_offset(CHANNEL, &fixture.session, 710, STARTED, Some(0));
    row.current_msg_id = 88010;
    row.output_path = Some(fixture.output.clone());
    row.full_response = "Legacy original seed remains in custody".into();
    row.response_sent_offset = row.full_response.len();
    row.turn_nonce = Some("legacy-custody-nonce".into());
    row.last_watcher_relayed_offset = Some(0);
    row.last_watcher_relayed_generation_mtime_ns =
        Some(dr::current_generation_mtime_ns(&fixture.session));
    let path = test_support::seed_backfill_row(&row);
    drop(crate::services::discord::inflight::lock_inflight_state_path(&path).unwrap());
    let restored = crate::services::discord::tmux::restored_watcher_turn_from_inflight(
        &row,
        &fixture.session,
        false,
    )
    .unwrap();
    let observed = Guard::new(&fixture.provider, CHANNEL);
    observed.register_source(Path::new(&fixture.output));
    let submitted = Scope::new(&fixture.shared);
    let handle = TmuxWatcherHandle {
        tmux_session_name: fixture.session.clone(),
        output_path: fixture.output.clone(),
        paused: Arc::new(AtomicBool::new(false)),
        resume_offset: Arc::new(Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(false)),
        pause_epoch: Arc::new(AtomicU64::new(0)),
        turn_delivered: Arc::new(AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(AtomicI64::new(
            crate::services::discord::tmux_watcher_now_ms(),
        )),
    };
    let claim = claim_or_reuse_watcher_for_host(
        &fixture.shared.tmux_watchers,
        fixture.channel,
        handle,
        &fixture.provider,
        "terminal_legacy_preparation",
        None,
        WatchHost::Legacy,
    )
    .unwrap();
    assert!(claim.should_spawn());
    let handle = fixture.handle();
    spawn_observed_tmux_watcher(
        "terminal_legacy_preparation",
        fixture.shared.clone(),
        fixture.session.clone(),
        handle.cancel.clone(),
        crate::services::discord::tmux::tmux_watcher::tmux_output_watcher_with_restore(
            fixture.channel,
            fixture.http.clone(),
            fixture.shared.clone(),
            fixture.output.clone(),
            fixture.session.clone(),
            0,
            handle.cancel.clone(),
            handle.paused.clone(),
            handle.resume_offset.clone(),
            handle.pause_epoch.clone(),
            handle.turn_delivered.clone(),
            handle.last_heartbeat_ts_ms.clone(),
            Some(restored),
        ),
    );
    let ticket = watcher_completion::observe(&handle.cancel).unwrap();
    until(&observed, "actual Legacy predecessor EOF", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let old = serde_json::json!({"type":"assistant", "uuid":"legacy-custody", "apiBlockIndex":0,
        "message":{"id":"legacy-custody", "content":[{"type":"text", "text":"Legacy unfinished body remains in custody"}]}});
    let bytes = format!("{old}\n");
    append(&fixture, bytes.as_bytes());
    until(&observed, "actual Legacy predecessor decode", |s| {
        s.decoded_chunks.concat() == bytes
    })
    .await;
    crate::services::discord::tmux::tmux_watcher::cancel_for_test(
        &handle.cancel,
        Ordering::Release,
    );
    joined(ticket).await;
    assert_eq!(submitted.join_all().await.len(), 1);
    for seen in [observed.snapshot(), observed.raw_process_snapshot()] {
        assert!(seen.event_count("legacy_row_seed_adopted") > 0, "{seen:?}");
        assert!(seen.event_count("post_stream_exit") > 0, "{seen:?}");
    }
    let legacy = capsule_observation::pending(&fixture.shared, fixture.channel).await;
    assert_eq!(legacy.len(), 1, "{legacy:?}");
    assert!(!legacy[0].origin_is_retired());
    assert!(
        legacy[0].has_identity && legacy[0].has_startup_snapshot,
        "{legacy:?}"
    );
    assert!(
        legacy[0].body.contains("Legacy unfinished body"),
        "{legacy:?}"
    );
    let recorded = capsule_observation::recorded_original(&fixture.shared, fixture.channel);
    assert!(recorded.is_some());
    drop(submitted);
    drop(observed);
    drop(owned);
    // The O writer starts at an empty replacement; Legacy keeps its opened descriptor.
    std::fs::rename(
        &fixture.output,
        format!("{}.legacy-custody", fixture.output),
    )
    .unwrap();
    std::fs::write(&fixture.output, []).unwrap();
    let writer = writer_for_fixture(&fixture);
    (fixture, discord, writer, legacy, recorded)
}
