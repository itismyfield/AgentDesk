//! Watcher-delegation and bridge-owned terminal-response observability helpers.

use super::super::*;
use super::output_lifecycle::BridgeOutputOwner;
use crate::services::provider::ProviderKind;

/// Should the bridge delegate this turn's assistant relay to the watcher (the
/// watcher already owns the relay, it is available for the turn, the bridge has
/// no pending response of its own, and this is not a terminal-error path)?
pub(super) fn should_delegate_bridge_relay_to_watcher(
    watcher_owns_assistant_relay: bool,
    watcher_relay_available_for_turn: bool,
    bridge_response_pending: bool,
    cancelled: bool,
    is_prompt_too_long: bool,
    transport_error: bool,
    recovery_retry: bool,
) -> bool {
    watcher_owns_assistant_relay
        && watcher_relay_available_for_turn
        && !bridge_response_pending
        && !cancelled
        && !is_prompt_too_long
        && !transport_error
        && !recovery_retry
}

/// A watcher handle is registered for `owner_channel_id` and is not cancelled.
/// This availability check is used by non-handoff observability sites that only
/// need handle presence.
pub(super) fn live_watcher_registered_for_relay(
    shared: &SharedData,
    owner_channel_id: ChannelId,
) -> bool {
    shared
        .tmux_watchers
        .get(&owner_channel_id)
        .is_some_and(|watcher| !watcher.cancel.load(std::sync::atomic::Ordering::Relaxed))
}

/// #3281: which empty-terminal-response visibility event (if any) applies to
/// this finalize. Pure so the gating is unit-testable. `None` owner keeps the
/// pre-#3281 `bridge_output_owner_none_empty_response` semantics verbatim;
/// `Some(WatcherRelay)` adds the delegated quadrant ("the watcher must carry
/// the whole body from its resume offset") so a watcher parked past the
/// response bytes (#3277-shape loss) is measurable. Terminal-error paths, a
/// missing placeholder message, and a non-empty unsent response never emit.
pub(super) fn empty_terminal_response_visibility_kind(
    bridge_output_owner: Option<BridgeOutputOwner>,
    terminal_error_path: bool,
    current_msg_id: u64,
    response_unsent_empty: bool,
) -> Option<&'static str> {
    if terminal_error_path || current_msg_id == 0 || !response_unsent_empty {
        return None;
    }
    match bridge_output_owner {
        None => Some("bridge_output_owner_none_empty_response"),
        Some(BridgeOutputOwner::WatcherRelay) => Some("bridge_delegated_watcher_empty_response"),
        Some(BridgeOutputOwner::StandbyRelay) => None,
    }
}

/// #3281: emit the empty-terminal-response visibility event chosen by
/// [`empty_terminal_response_visibility_kind`]. Moved out of
/// `turn_bridge/mod.rs` (frozen giant baseline); the owner-`None` kind and
/// payload are byte-identical to the inline block this replaced, and the
/// delegated-watcher kind additionally carries `tmux_last_offset` /
/// `turn_start_offset` for offset forensics. Observability only — never
/// posts to Discord or alters relay ownership.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_bridge_empty_terminal_response_visibility(
    shared: &SharedData,
    watcher_owner_channel_id: ChannelId,
    bridge_output_owner: Option<BridgeOutputOwner>,
    terminal_error_path: bool,
    provider: &ProviderKind,
    channel_id: ChannelId,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: &str,
    current_msg_id: u64,
    response_unsent_empty: bool,
    watcher_owns_assistant_relay: bool,
    watcher_relay_available_for_turn: bool,
    standby_relay_owns_output: bool,
    rx_disconnected: bool,
    tmux_handed_off: bool,
    response_sent_offset: usize,
    full_response_len: usize,
    tmux_last_offset: Option<u64>,
    turn_start_offset: Option<u64>,
) {
    let Some(kind) = empty_terminal_response_visibility_kind(
        bridge_output_owner,
        terminal_error_path,
        current_msg_id,
        response_unsent_empty,
    ) else {
        return;
    };
    let mut extra = serde_json::json!({
        "current_msg_id": current_msg_id,
        "watcher_owns_assistant_relay": watcher_owns_assistant_relay,
        "watcher_relay_available_for_turn": watcher_relay_available_for_turn,
        "live_watcher_registered": live_watcher_registered_for_relay(
            shared,
            watcher_owner_channel_id,
        ),
        "standby_relay_owns_output": standby_relay_owns_output,
        "rx_disconnected": rx_disconnected,
        "tmux_handed_off": tmux_handed_off,
        "response_sent_offset": response_sent_offset,
        "full_response_len": full_response_len,
    });
    if kind == "bridge_delegated_watcher_empty_response"
        && let Some(map) = extra.as_object_mut()
    {
        map.insert(
            "tmux_last_offset".to_string(),
            serde_json::json!(tmux_last_offset),
        );
        map.insert(
            "turn_start_offset".to_string(),
            serde_json::json!(turn_start_offset),
        );
    }
    crate::services::observability::emit_inflight_lifecycle_event(
        provider.as_str(),
        channel_id.get(),
        dispatch_id,
        session_key,
        Some(turn_id),
        kind,
        extra,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3281 truth table for the empty-terminal-response visibility gate:
    /// owner `None` keeps the pre-#3281 kind verbatim, delegated-watcher gets
    /// its own kind, and terminal errors / missing placeholder / non-empty
    /// unsent response / standby owner never emit.
    #[test]
    fn empty_terminal_response_visibility_kind_truth_table() {
        // Owner None + empty unsent response → original kind (verbatim).
        assert_eq!(
            empty_terminal_response_visibility_kind(None, false, 42, true),
            Some("bridge_output_owner_none_empty_response"),
        );
        // Delegated to the watcher + empty unsent response → new quadrant kind.
        assert_eq!(
            empty_terminal_response_visibility_kind(
                Some(BridgeOutputOwner::WatcherRelay),
                false,
                42,
                true,
            ),
            Some("bridge_delegated_watcher_empty_response"),
        );
        // Non-empty unsent response → the bridge still owns deliverable bytes.
        assert_eq!(
            empty_terminal_response_visibility_kind(None, false, 42, false),
            None,
        );
        // Delegated watcher + non-empty unsent response is not an empty-response signal.
        assert_eq!(
            empty_terminal_response_visibility_kind(
                Some(BridgeOutputOwner::WatcherRelay),
                false,
                42,
                false,
            ),
            None,
        );
        // Terminal-error path → excluded (matches the pre-#3281 gate).
        assert_eq!(
            empty_terminal_response_visibility_kind(None, true, 42, true),
            None,
        );
        // No placeholder message id → excluded (matches the pre-#3281 gate).
        assert_eq!(
            empty_terminal_response_visibility_kind(None, false, 0, true),
            None,
        );
        // Standby relay owns output → not part of this visibility surface.
        assert_eq!(
            empty_terminal_response_visibility_kind(
                Some(BridgeOutputOwner::StandbyRelay),
                false,
                42,
                true,
            ),
            None,
        );
    }
}
