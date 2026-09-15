from pathlib import Path
base=Path('src/services/discord/tmux_watcher')
p=base/'cancel_handoff.rs';s=p.read_text();s+='''
/// A retained original episode still owes preview/actor settlement after the
/// sink commits. Do not let the rowless watermark shortcut bypass that epilogue.
/// This only preserves candidacy; existing sink ACK/receipt and lease gates run.
pub(super) fn has_recorded_completion(
    context: &TerminalPreflightContext<'_>, start: u64, end: u64,
) -> bool {
    recorded_episode(context.shared, context.watcher_provider, context.channel_id,
        context.tmux_session_name).is_some_and(|episode|
            episode.original.turn_start_offset == Some(start) && end > start
                && episode.source.metadata().is_ok_and(|meta| meta.len() >= end))
}
''';p.write_text(s)
p=base/'terminal_preflight.rs';s=p.read_text();a=s.index('    // #3017 single output-offset authority');b=s.index('    if inflight_missing_before_relay',a)
s=s[:a]+'''    // #3017: rowless idle relays may suppress an already delivered range, but a
    // retained cancellation episode must still reach preview and actor settlement.
    // This shortcut is not the transport dedup gate; that stays in the relay plan.
'''+s[b:]
old='''        if committed >= turn_consumed_offset && turn_consumed_offset > turn_data_start_offset {'''
assert s.count(old)==1
s=s.replace(old,'''        if committed >= turn_consumed_offset && turn_consumed_offset > turn_data_start_offset
            && !cancel_handoff::has_recorded_completion(context, turn_data_start_offset, turn_consumed_offset)
        {''');p.write_text(s)
p=base/'cancel_handoff/interrupted_adoption_tests.rs';s=p.read_text();s+='''
/// Exercise the real outer-watcher preflight after the sink has already committed.
/// Without the exemption this returned Continue and stranded the original actor.
pub(in crate::services::discord) async fn assert_committed_preflight_reaches_settlement(
    ctx: &TurnStreamCollectorContext, turn: &mut CollectedTurnStream,
    buffer: &String, offset: u64,
) {
    let context = TerminalPreflightContext {
        http: &ctx.http, shared: &ctx.shared, channel_id: ctx.channel_id,
        watcher_provider: &ctx.watcher_provider, tmux_session_name: &ctx.tmux_session_name,
        output_path: &ctx.output_path,
    };
    let start = turn.turn_data_start_offset;
    let end = terminal_event_consumed_offset(offset, buffer);
    assert!(ctx.shared.committed_relay_offset(ctx.channel_id) >= end);
    assert!(has_recorded_completion(&context, start, end));
    assert!(!has_recorded_completion(&context, start + 1, end));
    let mut last_offset = None;
    let mut last_generation = None;
    let result = run_terminal_preflight_suppression(
        &context,
        TerminalPreflightSuppressionLocals {
            current_offset: offset, all_data: buffer, data_start_offset: start,
            turn_data_start_offset: start, has_assistant_response: true, has_current_response: true,
            inflight_missing_before_relay: true, inflight_silent_turn: false,
            recent_stop_for_output: None, placeholder_msg_id: turn.placeholder_msg_id,
            placeholder_from_restored_inflight: turn.placeholder_from_restored_inflight,
            last_edit_text: turn.last_edit_text.clone(), last_relayed_offset: None,
            last_observed_generation_mtime_ns: None,
            monitor_auto_turn_claimed: turn.monitor_auto_turn_claimed,
            monitor_auto_turn_finished: turn.monitor_auto_turn_finished,
            monitor_auto_turn_synthetic_msg_id: turn.monitor_auto_turn_synthetic_msg_id,
            monitor_auto_turn_ledger_generation: turn.monitor_auto_turn_ledger_generation,
        },
        &mut TerminalPreflightSuppressionState {
            placeholder_from_restored_inflight: &mut turn.placeholder_from_restored_inflight,
            last_edit_text: &mut turn.last_edit_text, last_relayed_offset: &mut last_offset,
            last_observed_generation_mtime_ns: &mut last_generation,
            monitor_auto_turn_claimed: &mut turn.monitor_auto_turn_claimed,
            monitor_auto_turn_finished: &mut turn.monitor_auto_turn_finished,
            monitor_auto_turn_synthetic_msg_id: &mut turn.monitor_auto_turn_synthetic_msg_id,
            monitor_auto_turn_ledger_generation: &mut turn.monitor_auto_turn_ledger_generation,
        },
    ).await;
    assert!(matches!(result, TerminalPreflightOutcome::Proceed(_)),
        "confirmed cancelled episode must reach the existing settlement epilogue");
    assert!(ctx.shared.mailbox(ctx.channel_id).has_active_turn().await,
        "preflight alone must not release the actor");
}
''';p.write_text(s)
p=base/'streaming_status_tick/native_collector_tests.rs';s=p.read_text();old='''        assert!(plan.session_bound_relay_owns_terminal_delivery);
        terminal_send::committed_placeholder_cleanup''';assert s.count(old)==1;s=s.replace(old,'''        assert!(plan.session_bound_relay_owns_terminal_delivery);
        if rowless {
            cancel_handoff::interrupted_adoption_tests::assert_committed_preflight_reaches_settlement(
                &ctx, &mut turn, &buffer, offset,
            ).await;
        }
        terminal_send::committed_placeholder_cleanup''');p.write_text(s)
