use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n1a_collector_status_tick_cannot_recreate_confirmed_row() {
    let test = "n1a_collector_status_tick_cannot_recreate_confirmed_row";
    if !isolated_in("n1a_turn_mode_tests", test, &[]) {
        return;
    }
    let seed = format!("{}{}{}", user("earlier"), said("delivered"), stop());
    let mut h = Harness::new(6325, &seed).await;
    let confirmed = crate::services::tui_o::turn_mode::TestConfirmation::new(h.channel.get());
    let frontier = seed.len() as u64;
    h.commit(0, frontier);
    h.take_tmux_calls();
    let tick = crate::services::tui_o::turn_mode::test_tick::signal(h.channel.get());
    h.spawn(frontier);
    h.append(format!("{}{}", user("direct"), said("still streaming")).as_bytes());
    tokio::time::timeout(Duration::from_secs(30), tick)
        .await
        .unwrap()
        .unwrap();
    assert!(
        h.row().is_none(),
        "collector→status tick must leave confirmed channel row absent"
    );
    assert!(
        !h.take_tmux_calls()
            .iter()
            .any(|c| c.starts_with("capture-pane")),
        "turn mode gates pane capture"
    );
    h.cancel();
    let task = h.watcher.take().unwrap().task;
    task.abort();
    let _ = task.await;
    drop(confirmed);

    let mut legacy = Harness::new(6326, &seed).await;
    legacy.commit(0, frontier);
    let tick = crate::services::tui_o::turn_mode::test_tick::signal(legacy.channel.get());
    legacy.spawn(frontier);
    legacy.append(format!("{}{}", user("direct"), said("still streaming")).as_bytes());
    tokio::time::timeout(Duration::from_secs(30), tick)
        .await
        .unwrap()
        .unwrap();
    assert!(
        legacy.row().is_some(),
        "unconfirmed collector retains its row producer"
    );
    legacy.cancel();
    let task = legacy.watcher.take().unwrap().task;
    task.abort();
    let _ = task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n1a_post_terminal_prologue_cannot_recreate_confirmed_row() {
    use super::super::super::loop_poll_prologue::*;
    let test = "n1a_post_terminal_prologue_cannot_recreate_confirmed_row";
    if !isolated_in("n1a_turn_mode_tests", test, &[]) {
        return;
    }
    let seed = format!("{}{}{}", user("earlier"), said("delivered"), stop());
    let h = Harness::new(6327, &seed).await;
    let _confirmed = crate::services::tui_o::turn_mode::TestConfirmation::new(h.channel.get());
    let frontier = seed.len() as u64;
    h.commit(0, frontier);
    h.append(format!("{}{}", user("direct"), said("post-terminal streaming")).as_bytes());
    h.take_tmux_calls();
    let host = Arc::new(HostSnapshot::new(WatchHost::Legacy));
    let context = PollWatcherContext {
        http: &h.http,
        shared: &h.shared,
        channel_id: h.channel,
        watcher_provider: &CLAUDE,
        tmux_session_name: &h.tmux,
        output_path: &h.path,
        watcher_thread_channel_id: None,
        watcher_instance_id: 6327,
        host: &host,
        legacy_mode: WatcherLegacyMode::Legacy,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    let resume = Arc::new(Mutex::new(None));
    let epoch = Arc::new(AtomicU64::new(0));
    let delivered = Arc::new(AtomicBool::new(false));
    let heartbeat = Arc::new(AtomicI64::new(0));
    let notify = Arc::new(tokio::sync::Notify::new());
    let dead_notify = Arc::new(tokio::sync::Notify::new());
    let controls = PollWatcherControls {
        cancel: &cancel,
        paused: &paused,
        resume_offset: &resume,
        pause_epoch: &epoch,
        turn_delivered: &delivered,
        last_heartbeat_ts_ms: &heartbeat,
        jsonl_notify: &notify,
        dead_marker_notify: &dead_notify,
    };
    let mut offset = frontier;
    let mut terminal_observed = true;
    let mut relayed = Some(frontier);
    let mut generation = Some(dr::current_generation_mtime_ns(&h.tmux));
    let mut rotation = 0;
    let mut identity = None;
    let mut nonce = None;
    let mut offsets = RelayOffsetState {
        current_offset: &mut offset,
        terminal_delivery_observed: &mut terminal_observed,
        last_relayed_offset: &mut relayed,
        last_observed_generation_mtime_ns: &mut generation,
        rotation_tick: &mut rotation,
        watcher_turn_identity: &mut identity,
        watcher_turn_nonce: &mut nonce,
    };
    let mut source = None;
    let mut decoder = Utf8ChunkDecoder::default();
    let mut footer = WatcherCompletionFooterIdleState::default();
    let mut activity = None;
    let mut poll = LoopPollState {
        retained_source: &mut source,
        prompt_too_long_killed: false,
        all_data: &String::new(),
        utf8_decoder: &mut decoder,
        completion_footer_idle: &mut footer,
        last_activity_heartbeat_at: &mut activity,
    };
    let mut continuation_logged = false;
    let mut suppressed_range = None;
    let mut reacquire_logged = false;
    let restored = None;
    let mut terminal = PostTerminalState {
        turn_result_relayed: true,
        post_terminal_continuation_logged: &mut continuation_logged,
        last_post_terminal_suppressed_range: &mut suppressed_range,
        active_stream_inflight_reacquire_logged: &mut reacquire_logged,
        restored_turn: &restored,
        restored_injected_prompt_message_id: None,
    };
    poll_watcher_output_or_continue(&context, &controls, &mut offsets, &mut poll, &mut terminal)
        .await;
    assert!(
        h.row().is_none(),
        "post-terminal prologue must leave confirmed channel row absent"
    );
    assert!(
        !h.take_tmux_calls()
            .iter()
            .any(|c| c.starts_with("capture-pane")),
        "confirmed prologue must not capture the pane"
    );
    assert!(!reacquire_watcher_inflight_for_active_stream(
        &CLAUDE, h.channel, &h.tmux, &h.path, frontier, None, None, None
    ));
    assert!(h.row().is_none(), "producer itself must gate every caller");
}
