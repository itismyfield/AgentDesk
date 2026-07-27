use super::*;
use std::sync::Arc;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AbortExitOutcome {
    ContinueWatcherLoop,
    Fallthrough,
}

pub(super) struct TerminalAbortExitContext<'a> {
    pub(super) http: &'a Arc<serenity::Http>,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) channel_id: serenity::ChannelId,
    pub(super) watcher_provider: &'a ProviderKind,
    pub(super) tmux_session_name: &'a String,
    pub(super) paused: &'a Arc<AtomicBool>,
    pub(super) pause_epoch: &'a Arc<AtomicU64>,
}

pub(super) struct TerminalAbortExitLocals {
    pub(super) was_paused: bool,
    pub(super) epoch_snapshot: u64,
    pub(super) monitor_auto_turn_deferred: bool,
    pub(super) placeholder_msg_id: Option<serenity::MessageId>,
    pub(super) turn_data_start_offset: u64,
    pub(super) current_offset: u64,
    pub(super) response_sent_offset: usize,
    pub(super) is_prompt_too_long: bool,
}

pub(super) struct TerminalAbortExitState<'a> {
    pub(super) placeholder_from_restored_inflight: &'a mut bool,
    pub(super) last_edit_text: &'a mut String,
    pub(super) monitor_auto_turn_claimed: &'a mut bool,
    pub(super) monitor_auto_turn_finished: &'a mut bool,
    pub(super) monitor_auto_turn_synthetic_msg_id: &'a mut Option<serenity::MessageId>,
    pub(super) monitor_auto_turn_ledger_generation: &'a mut Option<u64>,
    pub(super) all_data: &'a mut String,
    pub(super) all_data_start_offset: &'a mut u64,
    pub(super) all_data_fully_mirrored_to_session_relay: &'a mut bool,
    pub(super) all_data_session_bound_relay_ack: &'a mut Option<SessionBoundRelayAckTarget>,
    pub(super) all_data_first_forwarded_relay_sequence: &'a mut Option<u64>,
    pub(super) prompt_too_long_killed: &'a mut bool,
}

pub(super) async fn handle_terminal_abort_exits(
    context: &TerminalAbortExitContext<'_>,
    locals: TerminalAbortExitLocals,
    state: &mut TerminalAbortExitState<'_>,
) -> AbortExitOutcome {
    let http = context.http;
    let shared = context.shared;
    let channel_id = context.channel_id;
    let watcher_provider = context.watcher_provider;
    let tmux_session_name = context.tmux_session_name;
    let paused = context.paused;
    let pause_epoch = context.pause_epoch;

    // Discard partial data if paused while reading (even if now unpaused), or if the epoch
    // changed (a Discord turn claimed this data even when paused is now false).
    let paused_now = paused.load(Ordering::Relaxed);
    let epoch_changed_now = pause_epoch.load(Ordering::Relaxed) != locals.epoch_snapshot;
    let deferred_monitor_ready =
        *state.monitor_auto_turn_claimed && locals.monitor_auto_turn_deferred && !paused_now;
    if (locals.was_paused || paused_now || epoch_changed_now) && !deferred_monitor_ready {
        if let Some(msg_id) = locals.placeholder_msg_id {
            if watcher_should_delete_suppressed_placeholder(
                *state.placeholder_from_restored_inflight,
            ) {
                let inflight_before_cleanup =
                    crate::services::discord::inflight::load_inflight_state(
                        watcher_provider,
                        channel_id.get(),
                    );
                let _ = delete_nonterminal_placeholder_unless_delivered(
                    http,
                    channel_id,
                    shared,
                    watcher_provider,
                    tmux_session_name,
                    msg_id,
                    inflight_before_cleanup.as_ref(),
                    Some((
                        locals.turn_data_start_offset,
                        terminal_event_consumed_offset(locals.current_offset, &*state.all_data),
                    )),
                    locals.response_sent_offset,
                    state.last_edit_text.as_str(),
                    "watcher_pause_epoch_placeholder_cleanup",
                )
                .await;
            } else {
                *state.placeholder_from_restored_inflight = false;
                state.last_edit_text.clear();
            }
        }
        finish_monitor_auto_turn_if_claimed(
            shared,
            watcher_provider,
            channel_id,
            &mut *state.monitor_auto_turn_claimed,
            &mut *state.monitor_auto_turn_finished,
            &mut *state.monitor_auto_turn_synthetic_msg_id,
            &mut *state.monitor_auto_turn_ledger_generation,
        )
        .await;
        state.all_data.clear();
        *state.all_data_start_offset = locals.current_offset;
        *state.all_data_fully_mirrored_to_session_relay = true;
        *state.all_data_session_bound_relay_ack = None;
        *state.all_data_first_forwarded_relay_sequence = None;
        return AbortExitOutcome::ContinueWatcherLoop;
    }

    // Handle prompt-too-long: kill session so next message creates a fresh one
    if locals.is_prompt_too_long {
        clear_provider_overload_retry_state(channel_id);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 👁 Prompt too long detected in watcher for {tmux_session_name}, killing session"
        );
        *state.prompt_too_long_killed = true;

        let sess = (*tmux_session_name).clone();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                crate::services::termination_audit::record_termination_for_tmux(
                    &sess,
                    None,
                    "tmux_watcher",
                    "prompt_too_long",
                    Some("watcher cleanup: prompt too long"),
                    None,
                );
                record_tmux_exit_reason(&sess, "watcher cleanup: prompt too long");
                crate::services::platform::tmux::kill_session(
                    &sess,
                    "watcher cleanup: prompt too long",
                );
            }),
        )
        .await;

        let notice = "⚠️ 컨텍스트 한도 초과로 세션을 초기화했습니다. 다음 메시지부터 새 세션으로 처리됩니다.";
        match locals.placeholder_msg_id {
            Some(msg_id) => {
                rate_limit_wait(shared, channel_id).await;
                let _ = crate::services::discord::http::edit_channel_message(
                    http, channel_id, msg_id, notice,
                )
                .await;
            }
            None => {
                let _ =
                    crate::services::discord::http::send_channel_message(http, channel_id, notice)
                        .await;
            }
        }
        // Don't break — let the watcher exit naturally when session-alive check fails
        finish_monitor_auto_turn_if_claimed(
            shared,
            watcher_provider,
            channel_id,
            &mut *state.monitor_auto_turn_claimed,
            &mut *state.monitor_auto_turn_finished,
            &mut *state.monitor_auto_turn_synthetic_msg_id,
            &mut *state.monitor_auto_turn_ledger_generation,
        )
        .await;
        return AbortExitOutcome::ContinueWatcherLoop;
    }

    AbortExitOutcome::Fallthrough
}
