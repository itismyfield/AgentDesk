use super::*;

pub(super) async fn settle_displaced_terminal(
    episode: (&ProviderKind, &InflightTurnState, &Arc<CancelToken>),
    receiver: (
        &mut StreamMessageReceiverAdapter,
        &mut VecDeque<StreamMessage>,
    ),
    guard: &mut super::guards::CompletionGuard,
) {
    let (provider, row, actor) = episode;
    let (rx, pending) = receiver;
    if observe_displaced_codex_terminal(row, actor, rx, pending).await {
        guard.settle_displaced_terminal(provider, actor, row).await;
    }
}

pub(super) async fn observe_displaced_codex_terminal(
    row: &InflightTurnState,
    actor: &Arc<CancelToken>,
    rx: &mut StreamMessageReceiverAdapter,
    pending: &mut VecDeque<StreamMessage>,
) -> bool {
    if row.runtime_kind != Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui) {
        return false;
    }
    loop {
        if cancel_requested(Some(actor.as_ref())) {
            return false;
        }
        let message = match pending.pop_front() {
            Some(message) => message,
            None => {
                let wait = turn_bridge_stream_wait_duration(false, None, std::time::Instant::now());
                match tokio::time::timeout(wait, rx.recv()).await {
                    Ok(Some(message)) => message,
                    Ok(None) => return false,
                    Err(_) => continue,
                }
            }
        };
        if displaced_codex_terminal_matches(row, actor, &message) {
            return true;
        }
    }
}

pub(super) fn displaced_codex_terminal_matches(
    row: &InflightTurnState,
    actor: &Arc<CancelToken>,
    message: &StreamMessage,
) -> bool {
    let StreamMessage::CodexTuiTerminalDone {
        tmux_session_name,
        turn_nonce,
        source_start,
        complete_record_end,
        captured_source,
        ..
    } = message
    else {
        return false;
    };
    let captured_actor_matches = captured_source.as_ref().is_none_or(|source| {
        source
            .actor
            .upgrade()
            .is_some_and(|captured| Arc::ptr_eq(&captured, actor))
    });
    captured_actor_matches
        && row.runtime_kind == Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui)
        && actor.turn_nonce() == Some(turn_nonce.as_str())
        && row.turn_nonce.as_deref() == Some(turn_nonce.as_str())
        && row.tmux_session_name.as_deref() == Some(tmux_session_name.as_str())
        && row.turn_start_offset == Some(*source_start)
        && complete_record_end > source_start
}
