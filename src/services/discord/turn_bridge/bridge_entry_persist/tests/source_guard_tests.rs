//! Source guards for the bridge entry: authority gate order and the authority-loss exit.

pub(super) fn bridge_entry_failure_gate_precedes_emit_stream_and_finalize() {
    let normalize_ws = |source: &str| source.split_whitespace().collect::<Vec<_>>().join(" ");
    let caller = normalize_ws(include_str!("../../mod.rs"));
    let helper = normalize_ws(include_str!("../../bridge_entry_persist.rs"));
    let spawn = caller
        .find("pub(in crate::services::discord) fn spawn_turn_bridge_with_pin")
        .expect("production bridge entry remains present");
    let authority = caller[spawn..]
        .find("if let Err(notice) = bridge_entry_persist::establish_bridge_entry_authority")
        .map(|offset| spawn + offset)
        .expect("production caller establishes authority");
    let guards = caller[authority..]
        .find("let (mut completion_guard, mut inflight_guard) = make_bridge_guards(")
        .map(|offset| authority + offset)
        .expect("production caller constructs guards");
    let entry_owner = caller[authority..guards]
        .find("let mut bridge_entry_watcher_owner_epoch_current = inflight_state .effective_relay_owner_kind()")
        .map(|offset| authority + offset)
        .expect("post-authority relay owner snapshot remains explicit");
    let guard_call = &caller[guards
        ..caller[guards..]
            .find(");")
            .map(|offset| guards + offset + 2)
            .expect("guard call remains bounded")];
    let notice_decision = caller[guards..]
        .find("resumed_long_running_placeholder_notice_message_id(")
        .map(|offset| guards + offset)
        .expect("restart notice is decided from the post-authority merge");
    let notice_edit = caller[notice_decision..]
        .find("resumed_msg_id,")
        .map(|offset| notice_decision + offset)
        .expect("restart notice edits only the predicate-approved anchor");
    let emit = caller[notice_edit..]
        .find("crate::services::observability::emit_turn_started")
        .map(|offset| notice_edit + offset)
        .expect("turn-start emit remains present");
    let stream = caller[emit..]
        .find("stream_loop::run_stream_loop")
        .map(|offset| emit + offset)
        .expect("stream loop remains present");
    let finalize = caller[stream..]
        .find("post_loop_finalize::run_post_loop_finalize")
        .map(|offset| stream + offset)
        .expect("post-loop finalize remains present");
    let finalize_context = &caller[finalize..];

    let establish = helper
        .find("pub(super) async fn establish_bridge_entry_authority")
        .expect("authority helper remains present");
    let persist = helper[establish..]
        .find("let outcome =")
        .map(|offset| establish + offset)
        .expect("authority helper persists first");
    let gate = helper[persist..]
        .find("if !bridge_entry_disposition_continues(")
        .map(|offset| persist + offset)
        .expect("authority helper gates persistence");
    let anchor = helper[gate..]
        .find("if !ensure_bridge_current_message_anchor")
        .map(|offset| gate + offset)
        .expect("authority helper guarded-binds an absent anchor");
    let refresh = helper[anchor..]
        .find("reconcile_runtime_locals_from_inflight_state")
        .map(|offset| anchor + offset)
        .expect("post-await durable anchor state refreshes detached locals");

    assert!(persist < gate && gate < anchor && anchor < refresh);
    assert!(
        helper[anchor..refresh]
            .contains("signal_bridge_entry_abort_completion(&mut ctx.bridge.completion_tx);")
            && helper[anchor..refresh].contains("return Err(None);"),
        "failed anchor materialization must signal EntryAborted and return before guards"
    );
    let abort = helper
        .find("pub(super) fn signal_bridge_entry_abort_completion")
        .unwrap();
    let reconcile = helper[abort..]
        .find("pub(super) fn reconcile_runtime_locals_from_inflight_state")
        .unwrap()
        + abort;
    assert!(!helper[abort..reconcile].contains("register_start"));
    assert!(!helper[abort..reconcile].contains("InflightSignal::Completed"));
    assert!(
        authority < entry_owner
            && entry_owner < guards
            && guards < notice_decision
            && notice_decision < notice_edit
            && notice_edit < emit
            && emit < stream
            && stream < finalize
    );
    assert!(
        helper[gate..anchor].contains("signal_bridge_entry_abort_completion")
            && helper[gate..anchor].contains("return Err(capture_entry_abort_notice("),
        "failed persistence must signal only the waiter and abort"
    );
    assert!(
        helper[gate..anchor]
            .contains("bridge_entry_disposition_continues(outcome, !anchor_was_absent)")
            && !helper[gate..anchor].contains("cohort"),
        "the entry gate must take the live anchor precondition and no rollout cohort read"
    );
    assert!(
        !caller[spawn..authority].contains("make_bridge_guards("),
        "pre-authority path must not register a finalizer or broadcast guard"
    );
    assert!(
        !caller[spawn..authority].contains("send_message"),
        "pre-authority path must not create a Discord placeholder"
    );
    assert!(
        guard_call.contains("&inflight_state"),
        "finalizer and cleanup guards must use the exact post-authority merge"
    );
    assert!(
        finalize_context.contains("bridge_entry_watcher_owner_epoch_current,"),
        "post-loop recovery classification must receive the entry owner epoch verdict"
    );
}

pub(super) fn stream_authority_loss_relinquishes_guards_before_visible_finalization() {
    let caller = include_str!("../../mod.rs");
    let outcome_match = caller
        .find("match stream_loop_output.outcome")
        .expect("stream-loop outcome remains handled");
    let authority_lost = caller[outcome_match..]
        .find("StreamLoopOutcome::AuthorityLost")
        .map(|offset| outcome_match + offset)
        .expect("authority loss remains explicit");
    let relinquish = caller[authority_lost..]
        .find("completion_guard.relinquish_bridge_authority()")
        .map(|offset| authority_lost + offset)
        .expect("authority loss suppresses the stale completion broadcast");
    let defuse = caller[relinquish..]
        .find("inflight_guard.defuse()")
        .map(|offset| relinquish + offset)
        .expect("authority loss suppresses stale durable cleanup");
    let early_return = caller[defuse..]
        .find("return;")
        .map(|offset| defuse + offset)
        .expect("authority loss exits the bridge immediately");
    let finalize = caller[outcome_match..]
        .find("post_loop_finalize::run_post_loop_finalize")
        .map(|offset| outcome_match + offset)
        .expect("normal bridge still has visible finalization");

    assert!(
        authority_lost < relinquish
            && relinquish < defuse
            && defuse < early_return
            && early_return < finalize
    );
}
