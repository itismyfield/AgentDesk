//! #3038 S1 status-panel completion helpers moved out of turn_bridge/mod.rs.

use super::*;

pub(super) fn status_panel_completion_ready_after_terminal_body(
    terminal_delivery_committed: bool,
    terminal_body_visible: bool,
    preserve_inflight_for_cleanup_retry: bool,
) -> bool {
    terminal_delivery_committed && terminal_body_visible && !preserve_inflight_for_cleanup_retry
}

pub(super) async fn complete_status_panel_v2<G: TurnGateway + ?Sized>(
    shared: &SharedData,
    gateway: &G,
    channel_id: ChannelId,
    status_panel_msg_id: Option<MessageId>,
    provider: &ProviderKind,
    started_at_unix: i64,
    last_status_panel_text: &mut String,
    background: bool,
    source: &'static str,
    expected_user_msg_id: u64,
) -> bool {
    if !shared.status_panel_v2_enabled {
        return true;
    }
    shared
        .placeholder_live_events
        .push_status_event(channel_id, StatusEvent::TurnCompleted { background });
    let panel_text =
        shared
            .placeholder_live_events
            .render_status_panel(channel_id, provider, started_at_unix);

    match status_panel_completion_action(status_panel_msg_id, last_status_panel_text, &panel_text) {
        StatusPanelCompletionAction::AlreadyCommitted => true,
        StatusPanelCompletionAction::SendFallback => {
            complete_status_panel_v2_fallback_with_gateway(
                shared,
                gateway,
                channel_id,
                provider,
                expected_user_msg_id,
                last_status_panel_text,
                panel_text,
                source,
            )
            .await
        }
        StatusPanelCompletionAction::Edit(status_msg_id) => {
            let edit_result = if gateway.can_chain_locally() {
                gateway
                    .edit_message(channel_id, status_msg_id, &panel_text)
                    .await
            } else if let Some(http) = shared.serenity_http_or_token_fallback() {
                super::http::edit_channel_message(&http, channel_id, status_msg_id, &panel_text)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            } else {
                Err("no Discord HTTP available for status-panel-v2 completion edit".to_string())
            };
            match edit_result {
                Ok(()) => {
                    *last_status_panel_text = panel_text;
                    true
                }
                Err(error) => {
                    if status_panel_message_missing_error(&error) {
                        return complete_status_panel_v2_fallback_with_gateway(
                            shared,
                            gateway,
                            channel_id,
                            provider,
                            expected_user_msg_id,
                            last_status_panel_text,
                            panel_text,
                            source,
                        )
                        .await;
                    }
                    tracing::warn!(
                        "[turn_bridge] failed to finalize status-panel-v2 message {} in channel {} from {}: {}",
                        status_msg_id,
                        channel_id,
                        source,
                        error
                    );
                    false
                }
            }
        }
    }
}

async fn complete_status_panel_v2_fallback_with_gateway<G: TurnGateway + ?Sized>(
    shared: &SharedData,
    gateway: &G,
    channel_id: ChannelId,
    provider: &ProviderKind,
    expected_user_msg_id: u64,
    last_status_panel_text: &mut String,
    panel_text: String,
    source: &'static str,
) -> bool {
    match send_status_panel_v2_completion_fallback(shared, gateway, channel_id, &panel_text).await {
        Ok(message_id) => {
            persist_status_panel_completion_fallback_message_id(
                provider,
                channel_id,
                Some(expected_user_msg_id),
                message_id,
                source,
            );
            *last_status_panel_text = panel_text;
            true
        }
        Err(error) => {
            tracing::warn!(
                "[turn_bridge] failed to send fallback status-panel-v2 completion in channel {} from {}: {}",
                channel_id,
                source,
                error
            );
            false
        }
    }
}

pub(in crate::services::discord) async fn complete_status_panel_v2_with_http(
    shared: &std::sync::Arc<SharedData>,
    http: &serenity::Http,
    channel_id: ChannelId,
    status_panel_msg_id: Option<MessageId>,
    provider: &ProviderKind,
    started_at_unix: i64,
    last_status_panel_text: &mut String,
    background: bool,
    source: &'static str,
    expected_user_msg_id: Option<u64>,
) -> bool {
    if !shared.status_panel_v2_enabled {
        return true;
    }
    shared
        .placeholder_live_events
        .push_status_event(channel_id, StatusEvent::TurnCompleted { background });
    let panel_text =
        shared
            .placeholder_live_events
            .render_status_panel(channel_id, provider, started_at_unix);

    match status_panel_completion_action(status_panel_msg_id, last_status_panel_text, &panel_text) {
        StatusPanelCompletionAction::AlreadyCommitted => true,
        StatusPanelCompletionAction::SendFallback => {
            rate_limit_wait(shared, channel_id).await;
            complete_status_panel_v2_fallback_with_http(
                http,
                channel_id,
                provider,
                expected_user_msg_id,
                last_status_panel_text,
                panel_text,
                source,
            )
            .await
        }
        StatusPanelCompletionAction::Edit(status_msg_id) => {
            rate_limit_wait(shared, channel_id).await;
            match super::http::edit_channel_message(http, channel_id, status_msg_id, &panel_text)
                .await
            {
                Ok(_) => {
                    *last_status_panel_text = panel_text;
                    true
                }
                Err(error) => {
                    let error = error.to_string();
                    if status_panel_message_missing_error(&error) {
                        return complete_status_panel_v2_fallback_with_http(
                            http,
                            channel_id,
                            provider,
                            expected_user_msg_id,
                            last_status_panel_text,
                            panel_text,
                            source,
                        )
                        .await;
                    }
                    tracing::warn!(
                        "[turn_bridge] failed to finalize status-panel-v2 message {} in channel {} from {}: {}",
                        status_msg_id,
                        channel_id,
                        source,
                        error
                    );
                    false
                }
            }
        }
    }
}

async fn complete_status_panel_v2_fallback_with_http(
    http: &serenity::Http,
    channel_id: ChannelId,
    provider: &ProviderKind,
    expected_user_msg_id: Option<u64>,
    last_status_panel_text: &mut String,
    panel_text: String,
    source: &'static str,
) -> bool {
    match send_status_panel_v2_completion_fallback_http(http, channel_id, &panel_text).await {
        Ok(message_id) => {
            persist_status_panel_completion_fallback_message_id(
                provider,
                channel_id,
                expected_user_msg_id,
                message_id,
                source,
            );
            *last_status_panel_text = panel_text;
            true
        }
        Err(error) => {
            tracing::warn!(
                "[turn_bridge] failed to send fallback status-panel-v2 completion in channel {} from {}: {}",
                channel_id,
                source,
                error
            );
            false
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusPanelCompletionAction {
    AlreadyCommitted,
    Edit(MessageId),
    SendFallback,
}

fn status_panel_completion_action(
    status_panel_msg_id: Option<MessageId>,
    last_status_panel_text: &str,
    panel_text: &str,
) -> StatusPanelCompletionAction {
    if panel_text == last_status_panel_text {
        return StatusPanelCompletionAction::AlreadyCommitted;
    }
    match normalize_status_panel_message_id(status_panel_msg_id) {
        Some(message_id) => StatusPanelCompletionAction::Edit(message_id),
        None => StatusPanelCompletionAction::SendFallback,
    }
}

pub(in crate::services::discord) fn normalize_status_panel_message_id(
    status_panel_msg_id: Option<MessageId>,
) -> Option<MessageId> {
    status_panel_msg_id.filter(|id| !is_synthetic_headless_message_id(*id))
}

/// #3161 (follow-up to #3142): bridge-path sibling of the watcher
/// committed-output status-panel staleness gate. The bridge captures
/// `status_panel_msg_id` from THIS turn's pinned inflight snapshot at turn start
/// and EDITs it at completion (the `complete_status_panel_v2` Edit arm). Between
/// those two points a NEWER follow-up turn on the SAME channel can re-bind the
/// on-disk `status_message_id` onto the SAME panel message (status-panel reuse),
/// so by completion time that Discord message is the newer turn's LIVE panel. If
/// the older bridge turn still EDITs it with its own `응답 완료` text it aliases
/// the newer turn's panel (the newer turn then re-overwrites it — cosmetic-
/// transient, matching the issue's severity).
///
/// The bridge is turn-pinned by IDENTITY (it owns `this_turn_user_msg_id`), not
/// by a committed offset range like the watcher, so the gate is identity-based:
/// return TRUE (skip the panel EDIT) iff the CURRENT on-disk row is concrete
/// evidence of a DIFFERENT, real turn that now OWNS this turn's panel —
/// `this_turn_user_msg_id != 0` AND `on_disk_user_msg_id != 0` AND
/// `on_disk_user_msg_id != this_turn_user_msg_id` AND the on-disk row's
/// `status_message_id` equals THIS turn's `status_panel_msg_id`.
///
/// Over-suppression guard (the issue's explicit requirement): an in-range
/// id==0 bridge/watcher-direct turn (`this_turn_user_msg_id == 0`, e.g.
/// TUI-direct / external-input) is NEVER flagged — the leading
/// `this_turn_user_msg_id != 0` short-circuit keeps it completing its panel
/// even when a different real on-disk owner is present, because a 0-id turn
/// cannot be proven stale by identity. We additionally require a real on-disk
/// owner AND that the on-disk row OWNS our exact panel id, so a turn whose
/// panel was never re-adopted still completes normally. Absent inflight row, no
/// panel id, a same-turn row, or a row pointing at a different panel all return
/// FALSE → the EDIT fires exactly as today.
pub(super) fn status_panel_completion_edit_aliases_newer_turn(
    this_turn_user_msg_id: u64,
    status_panel_msg_id: Option<MessageId>,
    on_disk_user_msg_id: u64,
    on_disk_status_message_id: Option<u64>,
) -> bool {
    let Some(panel_id) = normalize_status_panel_message_id(status_panel_msg_id) else {
        return false;
    };
    // `this_turn_user_msg_id != 0`: an in-range id==0 watcher-direct / external-
    // input bridge turn cannot be proven stale by identity, so it MUST still
    // complete its panel (the issue's over-suppression guard). Only a real
    // (non-zero) this-turn identity that a DIFFERENT real on-disk owner has
    // superseded on the SAME panel is treated as aliasing.
    this_turn_user_msg_id != 0
        && on_disk_user_msg_id != 0
        && on_disk_user_msg_id != this_turn_user_msg_id
        && on_disk_status_message_id == Some(panel_id.get())
}

/// #3161 (codex P1): pure seam deciding whether THIS turn's epilogue must
/// identity-guard the inflight-row removal instead of clearing it
/// unconditionally. A real (non-zero) this-turn identity MUST be guarded so an
/// OLD turn whose status-panel completion edit was alias-skipped (because a
/// NEWER turn re-adopted its panel — see
/// [`status_panel_completion_edit_aliases_newer_turn`]) does NOT also delete the
/// NEWER owner's on-disk inflight row. The guarded clear only removes the row
/// when the on-disk `user_msg_id` still matches THIS turn, so a newer owner is
/// preserved and can still complete its own panel.
///
/// An id==0 this-turn (TUI-direct / external-input bridge turn) cannot be
/// proven against the on-disk identity, so it keeps the unconditional clear —
/// the same over-suppression carve-out the alias predicate uses for the EDIT.
pub(super) fn bridge_epilogue_identity_guards_inflight_clear(this_turn_user_msg_id: u64) -> bool {
    this_turn_user_msg_id != 0
}

fn persist_status_panel_completion_fallback_message_id(
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_msg_id: Option<u64>,
    message_id: MessageId,
    source: &'static str,
) {
    if is_synthetic_headless_message_id(message_id) {
        return;
    }
    let Some(expected_user_msg_id) = expected_user_msg_id else {
        return;
    };
    // #3077: route the load-modify-save through the typed bind op so the
    // user_msg_id guard and the field set are serialized under the inflight
    // flock (no TOCTOU with a concurrent turn rebinding the row). Behavior is
    // preserved: bind only when the on-disk row still belongs to this turn.
    let guard = super::inflight::StatusPanelBindGuard {
        require_user_msg_id: Some(expected_user_msg_id),
        ..Default::default()
    };
    match super::inflight::bind_status_panel(provider, channel_id.get(), message_id.get(), &guard) {
        super::inflight::StatusPanelBindOutcome::Bound
        | super::inflight::StatusPanelBindOutcome::AlreadyBound
        | super::inflight::StatusPanelBindOutcome::SkippedPanelAlreadySet(_) => {}
        super::inflight::StatusPanelBindOutcome::Missing => {}
        super::inflight::StatusPanelBindOutcome::GuardMismatch => {
            tracing::debug!(
                "[turn_bridge] skipped persisting status-panel-v2 fallback id {} in channel {} from {}: inflight user_msg_id != expected {}",
                message_id,
                channel_id,
                source,
                expected_user_msg_id
            );
        }
        super::inflight::StatusPanelBindOutcome::IoError => {
            tracing::warn!(
                "[turn_bridge] failed to persist fallback status-panel-v2 message {} in channel {} from {}",
                message_id,
                channel_id,
                source
            );
        }
    }
}

async fn send_status_panel_v2_completion_fallback_http(
    http: &serenity::Http,
    channel_id: ChannelId,
    panel_text: &str,
) -> Result<MessageId, String> {
    super::http::send_channel_message(http, channel_id, panel_text)
        .await
        .map(|message| message.id)
        .map_err(|error| error.to_string())
}

async fn send_status_panel_v2_completion_fallback<G: TurnGateway + ?Sized>(
    shared: &SharedData,
    gateway: &G,
    channel_id: ChannelId,
    panel_text: &str,
) -> Result<MessageId, String> {
    if gateway.can_chain_locally() {
        return gateway.send_message(channel_id, panel_text).await;
    }
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return Err(
            "no Discord HTTP available for status-panel-v2 completion fallback".to_string(),
        );
    };
    super::http::send_channel_message(&http, channel_id, panel_text)
        .await
        .map(|message| message.id)
        .map_err(|error| error.to_string())
}

fn status_panel_message_missing_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("unknown message") || normalized.contains("10008")
}

pub(super) fn should_open_long_running_placeholder_controller(
    status_panel_v2_enabled: bool,
) -> bool {
    !status_panel_v2_enabled
}

pub(super) fn status_panel_message_id_for_turn(
    inflight_state: &mut InflightTurnState,
    reuse_status_panel_message: bool,
) -> Option<MessageId> {
    if !reuse_status_panel_message {
        inflight_state.status_message_id = None;
    }
    let status_msg_id = inflight_state.status_message_id.map(MessageId::new)?;
    if is_synthetic_headless_message_id(status_msg_id) {
        inflight_state.status_message_id = None;
        return None;
    }
    Some(status_msg_id)
}

#[cfg(test)]
mod status_panel_v2_rework_tests {
    use super::{
        ChannelId, InflightTurnState, MessageId, ProviderKind, StatusPanelCompletionAction,
        bridge_epilogue_identity_guards_inflight_clear, complete_status_panel_v2,
        should_open_long_running_placeholder_controller, status_panel_completion_action,
        status_panel_completion_edit_aliases_newer_turn,
        status_panel_completion_ready_after_terminal_body, status_panel_message_id_for_turn,
    };
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::gateway::TurnGateway;
    use crate::services::discord::inflight::{
        GuardedClearOutcome, clear_inflight_state, clear_inflight_state_if_matches,
        clear_inflight_state_if_matches_zero_owned, load_inflight_state, save_inflight_state,
    };
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    type TestGatewayFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

    struct StatusPanelFallbackGateway {
        sent_messages: Arc<Mutex<Vec<String>>>,
        edited_message_ids: Arc<Mutex<Vec<MessageId>>>,
        edit_error: Option<String>,
        send_id: MessageId,
    }

    impl StatusPanelFallbackGateway {
        fn with_edit_error(error: &str) -> Self {
            Self {
                edit_error: Some(error.to_string()),
                ..Self::default()
            }
        }
    }

    impl Default for StatusPanelFallbackGateway {
        fn default() -> Self {
            Self {
                sent_messages: Arc::new(Mutex::new(Vec::new())),
                edited_message_ids: Arc::new(Mutex::new(Vec::new())),
                edit_error: None,
                send_id: MessageId::new(1_500_000_000_000_999),
            }
        }
    }

    impl TurnGateway for StatusPanelFallbackGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            content: &'a str,
        ) -> TestGatewayFuture<'a, Result<MessageId, String>> {
            let sent_messages = self.sent_messages.clone();
            let send_id = self.send_id;
            Box::pin(async move {
                sent_messages
                    .lock()
                    .expect("sent messages lock")
                    .push(content.to_string());
                Ok(send_id)
            })
        }

        fn edit_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            message_id: MessageId,
            _content: &'a str,
        ) -> TestGatewayFuture<'a, Result<(), String>> {
            let edited_message_ids = self.edited_message_ids.clone();
            let edit_error = self.edit_error.clone();
            Box::pin(async move {
                edited_message_ids
                    .lock()
                    .expect("edited ids lock")
                    .push(message_id);
                match edit_error {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            })
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> TestGatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
        }

        fn add_reaction<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _emoji: char,
        ) -> TestGatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn remove_reaction<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _emoji: char,
        ) -> TestGatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: MessageId,
            _user_text: &'a str,
        ) -> TestGatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn dispatch_queued_turn<'a>(
            &'a self,
            _channel_id: ChannelId,
            _intervention: &'a super::super::Intervention,
            _request_owner_name: &'a str,
            _has_more_queued_turns: bool,
        ) -> TestGatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn validate_live_routing<'a>(
            &'a self,
            _channel_id: ChannelId,
        ) -> TestGatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn requester_mention(&self) -> Option<String> {
            None
        }

        fn can_chain_locally(&self) -> bool {
            true
        }

        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            Some(ProviderKind::Claude)
        }
    }

    #[test]
    fn status_panel_v2_disables_long_running_placeholder_controller() {
        assert!(!should_open_long_running_placeholder_controller(true));
        assert!(should_open_long_running_placeholder_controller(false));
    }

    fn make_status_panel_v2_shared_for_tests() -> Arc<crate::services::discord::SharedData> {
        let mut shared = super::super::make_shared_data_for_tests();
        Arc::get_mut(&mut shared)
            .expect("fresh test shared data should be uniquely owned")
            .status_panel_v2_enabled = true;
        shared
    }

    fn test_inflight_state() -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            1,
            Some("adk-cdx-test".to_string()),
            2,
            3,
            4,
            "test turn".to_string(),
            None,
            None,
            None,
            None,
            0,
        )
    }

    #[test]
    fn fresh_turn_discards_stale_status_panel_message_id() {
        let mut state = test_inflight_state();
        state.status_message_id = Some(99);

        let status_panel_msg_id = status_panel_message_id_for_turn(&mut state, false);

        assert_eq!(status_panel_msg_id, None);
        assert_eq!(state.status_message_id, None);
    }

    #[test]
    fn resume_turn_preserves_status_panel_message_id() {
        let mut state = test_inflight_state();
        state.status_message_id = Some(99);

        let status_panel_msg_id = status_panel_message_id_for_turn(&mut state, true);

        assert_eq!(status_panel_msg_id, Some(MessageId::new(99)));
        assert_eq!(state.status_message_id, Some(99));
    }

    #[test]
    fn resume_turn_discards_synthetic_status_panel_message_id() {
        let mut state = test_inflight_state();
        state.status_message_id = Some(9_100_000_000_000_000_123);

        let status_panel_msg_id = status_panel_message_id_for_turn(&mut state, true);

        assert_eq!(status_panel_msg_id, None);
        assert_eq!(state.status_message_id, None);
    }

    #[test]
    fn completion_action_does_not_fallback_when_panel_text_already_committed() {
        let panel_text = "응답 완료";

        let action = status_panel_completion_action(None, panel_text, panel_text);

        assert_eq!(action, StatusPanelCompletionAction::AlreadyCommitted);
    }

    #[test]
    fn completion_action_treats_synthetic_id_as_missing_target() {
        let action = status_panel_completion_action(
            Some(MessageId::new(9_100_000_000_000_000_123)),
            "",
            "응답 완료",
        );

        assert_eq!(action, StatusPanelCompletionAction::SendFallback);
    }

    #[test]
    fn completion_action_edits_real_status_panel_message_id() {
        let message_id = MessageId::new(1510319194921504931);

        let action = status_panel_completion_action(Some(message_id), "", "응답 완료");

        assert_eq!(action, StatusPanelCompletionAction::Edit(message_id));
    }

    // #3161: the bridge-path status-panel turn-aliasing gate. A NEWER follow-up
    // turn re-adopted THIS turn's captured panel between turn start and
    // completion (the on-disk row now carries a different, real `user_msg_id`
    // pointing at the SAME `status_message_id`), so the older bridge turn must
    // NOT edit it — that would alias the newer turn's live panel.
    #[test]
    fn completion_edit_skips_when_newer_turn_owns_this_panel() {
        let panel = MessageId::new(1510319194921504931);

        assert!(
            status_panel_completion_edit_aliases_newer_turn(
                7_000_001,
                Some(panel),
                7_000_999,
                Some(panel.get()),
            ),
            "a different real on-disk turn owning THIS panel must suppress the edit"
        );
    }

    // The common, non-aliased case: the on-disk row is still THIS turn → edit
    // proceeds. This is the GREEN companion to the aliasing case above.
    #[test]
    fn completion_edit_proceeds_when_same_turn_still_owns_panel() {
        let panel = MessageId::new(1510319194921504931);

        assert!(
            !status_panel_completion_edit_aliases_newer_turn(
                7_000_001,
                Some(panel),
                7_000_001,
                Some(panel.get()),
            ),
            "the SAME turn still owning the panel must complete normally"
        );
    }

    // Over-suppression guard (issue requirement): an in-range id==0
    // bridge/watcher-direct turn (TUI-direct / external-input) must STILL
    // complete its panel even though the on-disk id differs — a 0-id this-turn
    // can never be proven stale this way, and the panel was never re-adopted.
    #[test]
    fn completion_edit_proceeds_for_in_range_id_zero_turn() {
        let panel = MessageId::new(1510319194921504931);

        assert!(
            !status_panel_completion_edit_aliases_newer_turn(
                0,
                Some(panel),
                7_000_999,
                Some(panel.get()),
            ),
            "an id==0 watcher-direct/bridge turn must not be suppressed"
        );
    }

    // A different on-disk turn that does NOT own this turn's panel (e.g. it
    // adopted a different panel, or none) is not evidence of aliasing → edit
    // proceeds. Guards against over-suppression from a stale unrelated row.
    #[test]
    fn completion_edit_proceeds_when_newer_turn_owns_different_panel() {
        let panel = MessageId::new(1510319194921504931);
        let other_panel = 1510319194921599999u64;

        assert!(
            !status_panel_completion_edit_aliases_newer_turn(
                7_000_001,
                Some(panel),
                7_000_999,
                Some(other_panel),
            ),
            "a newer turn owning a DIFFERENT panel does not alias this one"
        );
        assert!(
            !status_panel_completion_edit_aliases_newer_turn(
                7_000_001,
                Some(panel),
                7_000_999,
                None
            ),
            "a newer turn with no panel does not alias this one"
        );
    }

    // No captured panel id (or a synthetic-headless one) → nothing to alias →
    // edit proceeds (routes to the fallback path as today).
    #[test]
    fn completion_edit_proceeds_when_no_real_panel_captured() {
        assert!(
            !status_panel_completion_edit_aliases_newer_turn(7_000_001, None, 7_000_999, Some(123)),
            "no captured panel id cannot alias"
        );
        assert!(
            !status_panel_completion_edit_aliases_newer_turn(
                7_000_001,
                Some(MessageId::new(9_100_000_000_000_000_123)),
                7_000_999,
                Some(9_100_000_000_000_000_123),
            ),
            "a synthetic-headless captured panel id cannot alias"
        );
    }

    // An absent on-disk identity (on_disk_user_msg_id == 0, the inflight row's
    // default / cleared identity) is not a newer-owner proof → edit proceeds.
    #[test]
    fn completion_edit_proceeds_when_on_disk_identity_absent() {
        let panel = MessageId::new(1510319194921504931);

        assert!(
            !status_panel_completion_edit_aliases_newer_turn(
                7_000_001,
                Some(panel),
                0,
                Some(panel.get()),
            ),
            "an id==0 on-disk row is not proof of a newer owner"
        );
    }

    #[test]
    fn status_panel_completion_waits_for_visible_terminal_body() {
        assert!(
            !status_panel_completion_ready_after_terminal_body(true, false, false),
            "terminal delivery accepted by an async body path is not enough to post completion"
        );
        assert!(
            status_panel_completion_ready_after_terminal_body(true, true, false),
            "completion may post once the terminal body is visibly committed"
        );
        assert!(
            !status_panel_completion_ready_after_terminal_body(true, true, true),
            "cleanup retry preservation must still suppress visible completion"
        );
    }

    #[tokio::test]
    async fn status_panel_fallback_completion_is_blocked_until_body_visible() {
        let shared = make_status_panel_v2_shared_for_tests();
        let gateway = StatusPanelFallbackGateway::default();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1509350490461180105);
        let mut last_status_panel_text = String::new();

        if status_panel_completion_ready_after_terminal_body(true, false, false) {
            let _ = complete_status_panel_v2(
                shared.as_ref(),
                &gateway,
                channel_id,
                Some(MessageId::new(9_100_000_000_000_000_123)),
                &provider,
                1_700_000_000,
                &mut last_status_panel_text,
                false,
                "test_completion_before_body",
                1510319194921504929,
            )
            .await;
        }

        assert!(
            gateway
                .sent_messages
                .lock()
                .expect("sent messages lock")
                .is_empty(),
            "fallback completion must not send before the terminal body is visible"
        );

        if status_panel_completion_ready_after_terminal_body(true, true, false) {
            let committed = complete_status_panel_v2(
                shared.as_ref(),
                &gateway,
                channel_id,
                Some(MessageId::new(9_100_000_000_000_000_123)),
                &provider,
                1_700_000_000,
                &mut last_status_panel_text,
                false,
                "test_completion_after_body",
                1510319194921504929,
            )
            .await;
            assert!(committed);
        }

        let sent_messages = gateway
            .sent_messages
            .lock()
            .expect("sent messages lock")
            .clone();
        assert_eq!(sent_messages.len(), 1);
        assert!(sent_messages[0].contains("응답 완료"));
    }

    #[tokio::test]
    async fn status_panel_completion_fallback_posts_when_message_id_is_synthetic() {
        let shared = make_status_panel_v2_shared_for_tests();
        let gateway = StatusPanelFallbackGateway::default();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1509350490461180105);
        let mut last_status_panel_text = String::new();

        let committed = complete_status_panel_v2(
            shared.as_ref(),
            &gateway,
            channel_id,
            Some(MessageId::new(9_100_000_000_000_000_123)),
            &provider,
            1_700_000_000,
            &mut last_status_panel_text,
            false,
            "test_synthetic_status_panel_id",
            1510319194921504929,
        )
        .await;

        assert!(committed);
        assert!(
            gateway
                .edited_message_ids
                .lock()
                .expect("edited ids lock")
                .is_empty(),
            "synthetic status-panel ids must not be edited through Discord"
        );
        let sent_messages = gateway
            .sent_messages
            .lock()
            .expect("sent messages lock")
            .clone();
        assert_eq!(sent_messages.len(), 1);
        assert!(sent_messages[0].contains("응답 완료"));
        assert_eq!(last_status_panel_text, sent_messages[0]);

        let committed = complete_status_panel_v2(
            shared.as_ref(),
            &gateway,
            channel_id,
            Some(MessageId::new(9_100_000_000_000_000_123)),
            &provider,
            1_700_000_000,
            &mut last_status_panel_text,
            false,
            "test_synthetic_status_panel_id_retry",
            1510319194921504929,
        )
        .await;

        assert!(committed);
        assert_eq!(
            gateway
                .sent_messages
                .lock()
                .expect("sent messages lock")
                .len(),
            1,
            "same completed panel text must not send duplicate fallback panels"
        );
    }

    #[tokio::test]
    async fn status_panel_completion_fallback_posts_after_unknown_message_edit() {
        let shared = make_status_panel_v2_shared_for_tests();
        let gateway = StatusPanelFallbackGateway::with_edit_error("Unknown Message");
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(1509350490461180105);
        let stale_status_msg_id = MessageId::new(1_500_000_000_000_111);
        let mut last_status_panel_text = String::new();

        let committed = complete_status_panel_v2(
            shared.as_ref(),
            &gateway,
            channel_id,
            Some(stale_status_msg_id),
            &provider,
            1_700_000_000,
            &mut last_status_panel_text,
            false,
            "test_unknown_status_panel_id",
            1510319194921504929,
        )
        .await;

        assert!(committed);
        assert_eq!(
            gateway
                .edited_message_ids
                .lock()
                .expect("edited ids lock")
                .as_slice(),
            &[stale_status_msg_id]
        );
        let sent_messages = gateway
            .sent_messages
            .lock()
            .expect("sent messages lock")
            .clone();
        assert_eq!(sent_messages.len(), 1);
        assert!(sent_messages[0].contains("응답 완료"));
        assert_eq!(last_status_panel_text, sent_messages[0]);
    }

    fn inflight_row_owned_by(
        provider: &ProviderKind,
        channel_id: u64,
        user_msg_id: u64,
        status_panel_msg_id: u64,
    ) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("alias-epilogue-test".to_string()),
            42,
            user_msg_id,
            user_msg_id + 1,
            "turn".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        state.status_message_id = Some(status_panel_msg_id);
        state
    }

    // #3161 (codex P1): the production skip-branch -> epilogue-cleanup
    // interaction. An OLD bridge turn whose status-panel completion EDIT is
    // correctly alias-skipped (a NEWER turn re-adopted its panel between turn
    // start and completion) MUST NOT remove the NEWER owner's on-disk inflight
    // row in its epilogue. Before the identity guard the removal at the
    // `clear_inflight_state` site was unconditional, so the OLD turn deleted the
    // NEWER owner's row -> the newer turn's status panel was left permanently
    // non-complete.
    //
    // RED->GREEN: this test drives the REAL on-disk inflight layer and the REAL
    // production decision seam (`bridge_epilogue_identity_guards_inflight_clear`
    // + `clear_inflight_state_if_matches`). Without the guard the epilogue would
    // run the unconditional `clear_inflight_state` (asserted as the regression
    // vector below) and the newer owner's row would be gone -> the final
    // `load_inflight_state` assertion fails.
    #[test]
    fn alias_skipped_old_turn_does_not_remove_newer_owners_inflight_row() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
            }
        }
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let provider = ProviderKind::Claude;
        let channel_id = 3_161_900u64;
        let panel = MessageId::new(1_510_319_194_921_504_931);
        let old_turn_user_msg_id = 7_000_001u64;
        let newer_turn_user_msg_id = 7_000_999u64;

        // A NEWER follow-up turn now owns the on-disk row AND has re-adopted the
        // SAME status panel the OLD turn captured at its start.
        save_inflight_state(&inflight_row_owned_by(
            &provider,
            channel_id,
            newer_turn_user_msg_id,
            panel.get(),
        ))
        .unwrap();

        // Production step 1: the OLD turn re-reads the current on-disk row at
        // completion and decides whether to EDIT the panel. The newer owner of
        // THIS panel must alias-skip the edit.
        let on_disk = load_inflight_state(&provider, channel_id).expect("newer row on disk");
        assert!(
            status_panel_completion_edit_aliases_newer_turn(
                old_turn_user_msg_id,
                Some(panel),
                on_disk.user_msg_id,
                on_disk.status_message_id,
            ),
            "precondition: the OLD turn's panel edit must be alias-skipped"
        );

        // Sanity / regression-vector: the OLD pre-fix behavior (unconditional
        // clear) WOULD have removed the newer owner's row. We assert the guard
        // routes AWAY from that path for a real this-turn identity.
        assert!(
            bridge_epilogue_identity_guards_inflight_clear(old_turn_user_msg_id),
            "a real (non-zero) this-turn identity must be identity-guarded in the epilogue"
        );

        // Production step 2: the OLD turn's epilogue cleanup. This mirrors the
        // exact production fork at the `clear_inflight_state` site.
        if bridge_epilogue_identity_guards_inflight_clear(old_turn_user_msg_id) {
            let outcome =
                clear_inflight_state_if_matches(&provider, channel_id, old_turn_user_msg_id);
            assert_eq!(
                outcome,
                GuardedClearOutcome::UserMsgMismatch,
                "the OLD turn must NOT clear a row that now belongs to the newer turn"
            );
        } else {
            clear_inflight_state(&provider, channel_id);
        }

        // The newer owner's row must survive the OLD turn's epilogue. (Pre-fix
        // unconditional clear deleted it here -> RED.)
        let survived = load_inflight_state(&provider, channel_id)
            .expect("newer owner's inflight row must survive the OLD turn's epilogue");
        assert_eq!(
            survived.user_msg_id, newer_turn_user_msg_id,
            "the surviving row must still belong to the newer turn"
        );

        // And the NEWER turn can still complete normally: its own epilogue (same
        // turn owns the row) clears it.
        let cleared =
            clear_inflight_state_if_matches(&provider, channel_id, newer_turn_user_msg_id);
        assert_eq!(
            cleared,
            GuardedClearOutcome::Cleared,
            "the newer turn must still be able to clear its own row at completion"
        );
        assert!(
            load_inflight_state(&provider, channel_id).is_none(),
            "the row is gone once the newer (owning) turn completes"
        );
    }

    // #3161 (codex P1, id==0 carve-out): the zero-id epilogue race. An OLD
    // zero-id turn (recovery / external-input / cluster-relay synthesized;
    // `user_msg_id == 0`) finalizes AFTER a NEWER real (non-zero) identity turn
    // wrote its inflight row. The pre-fix carve-out ran the UNCONDITIONAL
    // `clear_inflight_state`, blind-deleting the newer owner's row -> the newer
    // turn's status panel was left permanently non-complete (the same bug, now
    // for zero-id callers).
    //
    // RED->GREEN: this drives the REAL on-disk inflight layer and mirrors the
    // exact production fork at the zero-id epilogue site
    // (`bridge_epilogue_identity_guards_inflight_clear(0) == false` ->
    // `clear_inflight_state_if_matches_zero_owned`). With the old unconditional
    // `clear_inflight_state(...)` the final `load_inflight_state` assertion
    // would fail because the newer owner's row would be gone.
    #[test]
    fn zero_id_old_turn_does_not_remove_newer_owners_inflight_row() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
            }
        }
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let provider = ProviderKind::Claude;
        let channel_id = 3_161_950u64;
        let panel = MessageId::new(1_510_319_194_921_504_999);
        let newer_turn_user_msg_id = 7_001_999u64;

        // A NEWER real (non-zero) follow-up turn now owns the on-disk row.
        save_inflight_state(&inflight_row_owned_by(
            &provider,
            channel_id,
            newer_turn_user_msg_id,
            panel.get(),
        ))
        .unwrap();

        // The OLD turn is zero-id -> the epilogue takes the id==0 carve-out
        // branch (NOT the non-zero identity-guard branch).
        let old_turn_user_msg_id = 0u64;
        assert!(
            !bridge_epilogue_identity_guards_inflight_clear(old_turn_user_msg_id),
            "a zero-id this-turn must NOT take the non-zero identity-guard branch"
        );

        // Production step: the OLD zero-id turn's epilogue cleanup. This mirrors
        // the exact production fork's `else` arm.
        let outcome = clear_inflight_state_if_matches_zero_owned(&provider, channel_id);
        assert_eq!(
            outcome,
            GuardedClearOutcome::UserMsgMismatch,
            "the zero-id turn must NOT clear a row that now belongs to a newer non-zero turn"
        );

        // The newer owner's row must survive the OLD zero-id turn's epilogue.
        // (Pre-fix unconditional clear deleted it here -> RED.)
        let survived = load_inflight_state(&provider, channel_id)
            .expect("newer owner's inflight row must survive the OLD zero-id turn's epilogue");
        assert_eq!(
            survived.user_msg_id, newer_turn_user_msg_id,
            "the surviving row must still belong to the newer turn"
        );
    }

    // #3161 (codex P1, no-recovery-regression): a zero-id turn must STILL clear
    // its OWN zero-id row. The on-disk `user_msg_id` is 0 (a genuine
    // zero-id-owned recovery/external-input row), so the zero-owned guarded
    // clear removes it. This is the regression guard that the P1 fix did not
    // over-correct into refusing all zero-id cleanup.
    #[test]
    fn zero_id_turn_still_clears_its_own_zero_id_row() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
            }
        }
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let provider = ProviderKind::Claude;
        let channel_id = 3_161_970u64;

        // A genuine zero-id-owned row (recovery/external-input turn): on-disk
        // `user_msg_id == 0`.
        save_inflight_state(&inflight_row_owned_by(&provider, channel_id, 0, 0)).unwrap();

        let outcome = clear_inflight_state_if_matches_zero_owned(&provider, channel_id);
        assert_eq!(
            outcome,
            GuardedClearOutcome::Cleared,
            "a zero-id turn must still clear its OWN zero-id row (recovery cleanup)"
        );
        assert!(
            load_inflight_state(&provider, channel_id).is_none(),
            "the zero-id-owned row is removed by its own zero-id turn"
        );
    }

    // #3161 (codex P2): the `InflightCleanupGuard::Drop` is identity-aware. On
    // an abnormal exit the Drop must only clear THIS turn's row. We assert the
    // exact routing the Drop performs: a non-zero this-turn id routes through
    // the identity-guarded clear (preserving a newer owner), while a zero-id
    // this-turn routes through the zero-owned clear (preserving a newer
    // non-zero owner). The Drop body itself is a thin dispatch over these two
    // production helpers, so exercising them with the same inputs proves the
    // Drop's identity-awareness without spawning the full bridge task.
    #[test]
    fn cleanup_guard_drop_routing_is_identity_aware() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
            }
        }
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap()) };
        let _env_guard = EnvGuard;

        let provider = ProviderKind::Claude;

        // Case A: a non-zero guard whose abnormal drop fires AFTER a newer owner
        // re-wrote the row must NOT delete the newer owner's row.
        let channel_a = 3_161_980u64;
        let newer = 7_002_500u64;
        save_inflight_state(&inflight_row_owned_by(&provider, channel_a, newer, 111)).unwrap();
        // The drop carries the OLD turn's (different) non-zero id.
        let old_non_zero = 7_002_111u64;
        let outcome_a = clear_inflight_state_if_matches(&provider, channel_a, old_non_zero);
        assert_eq!(
            outcome_a,
            GuardedClearOutcome::UserMsgMismatch,
            "abnormal-path drop for a non-zero turn must not clear a newer owner's row"
        );
        assert_eq!(
            load_inflight_state(&provider, channel_a)
                .expect("newer owner survives")
                .user_msg_id,
            newer
        );

        // Case B: a zero-id guard whose abnormal drop fires AFTER a newer
        // non-zero owner re-wrote the row must NOT delete it either.
        let channel_b = 3_161_990u64;
        save_inflight_state(&inflight_row_owned_by(&provider, channel_b, newer, 222)).unwrap();
        let outcome_b = clear_inflight_state_if_matches_zero_owned(&provider, channel_b);
        assert_eq!(
            outcome_b,
            GuardedClearOutcome::UserMsgMismatch,
            "abnormal-path drop for a zero-id turn must not clear a newer non-zero owner's row"
        );
        assert_eq!(
            load_inflight_state(&provider, channel_b)
                .expect("newer owner survives")
                .user_msg_id,
            newer
        );

        // Case C: a guard that genuinely owns its row (matching non-zero id)
        // still cleans up on its abnormal drop.
        let channel_c = 3_161_995u64;
        let owner = 7_003_000u64;
        save_inflight_state(&inflight_row_owned_by(&provider, channel_c, owner, 333)).unwrap();
        let outcome_c = clear_inflight_state_if_matches(&provider, channel_c, owner);
        assert_eq!(
            outcome_c,
            GuardedClearOutcome::Cleared,
            "abnormal-path drop must still clean up the turn's OWN row"
        );
        assert!(load_inflight_state(&provider, channel_c).is_none());
    }
}
