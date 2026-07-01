//! #3805 P2 (PR-B): two-message status-panel SINK creation order.
//!
//! Under the default-OFF single-message path the bridge keeps one message per
//! turn (`AGENTDESK_SINGLE_MESSAGE_PANEL` footer) or swaps a separate status
//! panel ABOVE the answer (the legacy `status_panel_v2` separate path, panel
//! anchored at the original placeholder, answer swapped into a fresh message
//! BELOW it — see `single_message_footer::maybe_create_bridge_separate_status_panel_response`).
//!
//! When `placeholder.two_message_panel_enabled` (PR-A scaffolding) is ON the
//! layout is INVERTED: the answer message (`current_msg_id`) stays put — first,
//! highest — and the status panel is created as a NEW message BELOW it. Only the
//! panel handle (`status_message_id`) and the per-turn generation epoch
//! (`status_panel_generation`) change; the answer anchor, its committed offset,
//! and the bridge-created placeholder handle are all left untouched.
//!
//! Everything downstream (the live panel-refresh edits and the terminal
//! completion edit) already targets `status_message_id`, so this module only has
//! to reverse the CREATION order and open the generation epoch. The re-anchor
//! (rollover) and recovery/orphan sequences that lean on the generation guard
//! are later stages (PR-D/E); this module also exposes the pure generation
//! staleness predicate they build on.
//!
//! Isolation: the two-message logic lives here (a `turn_bridge/**`-uncapped
//! sibling) so the EXTREME `turn_bridge/mod.rs` giant and the 700-capped
//! `turn_bridge/status_panel.rs` stay lean. The parent's call sites are thin.

use super::*;

/// Pure gate: should the sink create the #3805 P2 status panel as a NEW message
/// BELOW the answer (answer-first layout)?
///
/// Only fires when the two-message rollout flag is ON, the turn is NOT in
/// `AGENTDESK_SINGLE_MESSAGE_PANEL` footer mode, `status_panel_v2` is enabled,
/// no panel has been bound yet (`status_panel_msg_id.is_none()`), and the answer
/// anchor is a REAL Discord message (a synthetic-headless turn has no message to
/// anchor a panel under, matching the OFF path's synthetic short-circuit). When
/// this returns `false` under an ON flag the caller does nothing — the ON path
/// never falls back to the OFF panel-above swap.
pub(super) fn bridge_should_create_two_message_status_panel(
    two_message_panel_enabled: bool,
    single_message_panel_footer_mode: bool,
    status_panel_v2_enabled: bool,
    status_panel_msg_id: Option<MessageId>,
    current_msg_id: MessageId,
) -> bool {
    two_message_panel_enabled
        && !single_message_panel_footer_mode
        && status_panel_v2_enabled
        && status_panel_msg_id.is_none()
        && !is_synthetic_headless_message_id(current_msg_id)
}

/// Create the #3805 P2 status panel as a NEW message BELOW the answer.
///
/// Invariants (contrast with the OFF `maybe_create_bridge_separate_status_panel_response`
/// swap, which reassigns `current_msg_id` to the freshly sent message and makes
/// the ORIGINAL message the panel above):
/// - `current_msg_id` (the answer, sent first / higher) is NOT touched.
/// - `last_edit_text`, `current_msg_len`, `response_sent_offset`, `full_response`,
///   and the bridge-created placeholder handle are NOT touched — the answer
///   message keeps its content and orphan-cleanup identity.
/// - The new message carries the processing status block and becomes the panel;
///   `status_message_id` is set to it.
/// - `status_panel_generation` is bumped once to OPEN this turn's panel epoch and
///   mirrored back to the caller-local so the terminal completion edit can prove
///   it against the on-disk epoch. The caller persists it via `save_inflight_state`.
///
/// This is pure msg-id / HTTP bookkeeping — it never tears down the
/// per-channel `StatusPanelState` (that would drop item4's `session_banner`
/// exactly-once claim), so the top session banner is unaffected.
pub(super) async fn create_bridge_two_message_status_panel_below_answer<G: TurnGateway + ?Sized>(
    gateway: &G,
    channel_id: ChannelId,
    initial_indicator: &str,
    current_msg_id: MessageId,
    status_panel_msg_id: &mut Option<MessageId>,
    inflight_state: &mut InflightTurnState,
    status_panel_generation: &mut u64,
    status_panel_dirty: &mut bool,
) {
    let panel_block = super::formatting::build_processing_status_block(initial_indicator);
    match gateway.send_message(channel_id, &panel_block).await {
        Ok(panel_msg_id) => {
            *status_panel_msg_id = Some(panel_msg_id);
            inflight_state.status_message_id = Some(panel_msg_id.get());
            // The answer message stays put; keep the persisted answer anchor
            // coherent (idempotent — it already equals current_msg_id).
            inflight_state.current_msg_id = current_msg_id.get();
            let next_generation = inflight_state.status_panel_generation.saturating_add(1);
            inflight_state.status_panel_generation = next_generation;
            *status_panel_generation = next_generation;
        }
        Err(error) => {
            tracing::warn!(
                "[turn_bridge] #3805 P2 failed to create two-message status panel below answer in channel {}: {}",
                channel_id,
                error
            );
            *status_panel_dirty = false;
        }
    }
}

/// #3805 P2 generation guard (pure): is a status edit/completion tagged with
/// `this_turn_generation` superseded by a newer panel epoch on disk?
///
/// Ownership-scoped: only the panel THIS turn actually owns on disk
/// (`panel_owned_on_disk`) can supersede it, so an unrelated turn's epoch never
/// suppresses this completion. Inert on the default-OFF path where every
/// generation is `0` (`0 > 0 == false`) — the completion edit fires exactly as
/// today. The later re-anchor/recovery stages (PR-D/E) bump the epoch mid-turn
/// so a stale in-flight edit for the OLD generation is skipped here.
pub(super) fn two_message_status_edit_generation_is_stale(
    this_turn_generation: u64,
    panel_owned_on_disk: bool,
    on_disk_generation: u64,
) -> bool {
    panel_owned_on_disk && on_disk_generation > this_turn_generation
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::gateway::GatewayFuture;
    use std::sync::Mutex;

    /// #3293: `InflightTurnState::new` resolves the AgentDesk runtime store to
    /// stamp the born generation, which panics unless the runtime root is a
    /// tempdir (never the live `~/.adk/release`). Point `AGENTDESK_ROOT_DIR` at a
    /// throwaway dir under the shared env lock so constructing a test inflight is
    /// deterministic regardless of the ambient environment; restore on drop.
    struct RuntimeRootGuard {
        previous: Option<std::ffi::OsString>,
        _root: tempfile::TempDir,
    }

    impl RuntimeRootGuard {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("runtime root");
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
            Self {
                previous,
                _root: root,
            }
        }
    }

    impl Drop for RuntimeRootGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn isolate_agentdesk_runtime_root() -> (std::sync::MutexGuard<'static, ()>, RuntimeRootGuard) {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = RuntimeRootGuard::new();
        (lock, root)
    }

    fn test_inflight(current_msg_id: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Claude,
            777,
            None,
            1,
            7_000_001,
            current_msg_id,
            "hello".to_string(),
            None,
            None,
            None,
            None,
            0,
        )
    }

    #[derive(Default)]
    struct SendTrackingGateway {
        sent: Arc<Mutex<Vec<String>>>,
        send_id: u64,
        fail_send: bool,
    }

    impl SendTrackingGateway {
        fn returning(send_id: u64) -> Self {
            Self {
                send_id,
                ..Self::default()
            }
        }

        fn failing() -> Self {
            Self {
                fail_send: true,
                ..Self::default()
            }
        }
    }

    impl TurnGateway for SendTrackingGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            content: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            let sent = self.sent.clone();
            let send_id = self.send_id;
            let fail_send = self.fail_send;
            Box::pin(async move {
                sent.lock().expect("sent lock").push(content.to_string());
                if fail_send {
                    Err("boom".to_string())
                } else {
                    Ok(MessageId::new(send_id))
                }
            })
        }

        fn edit_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn delete_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
        }

        fn add_reaction<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _emoji: char,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn remove_reaction<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _emoji: char,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: MessageId,
            _user_text: &'a str,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn dispatch_queued_turn<'a>(
            &'a self,
            _channel_id: ChannelId,
            _intervention: &'a super::super::Intervention,
            _request_owner_name: &'a str,
            _has_more_queued_turns: bool,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn validate_live_routing<'a>(
            &'a self,
            _channel_id: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
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
    fn gate_requires_flag_on_v2_no_footer_no_panel_real_answer() {
        let answer = MessageId::new(10);

        // Happy path: flag ON, footer OFF, v2 ON, no panel yet, real answer.
        assert!(bridge_should_create_two_message_status_panel(
            true, false, true, None, answer,
        ));
        // Flag OFF → never (the OFF single-message layout is unchanged).
        assert!(!bridge_should_create_two_message_status_panel(
            false, false, true, None, answer,
        ));
        // Footer mode → never (single-message footer owns the surface).
        assert!(!bridge_should_create_two_message_status_panel(
            true, true, true, None, answer,
        ));
        // v2 OFF → never.
        assert!(!bridge_should_create_two_message_status_panel(
            true, false, false, None, answer,
        ));
        // Panel already bound → never (idempotent; no second panel).
        assert!(!bridge_should_create_two_message_status_panel(
            true,
            false,
            true,
            Some(MessageId::new(99)),
            answer,
        ));
    }

    #[test]
    fn gate_rejects_synthetic_headless_answer_anchor() {
        // A synthetic-headless answer id is not a real Discord message, so there
        // is nothing to anchor a panel beneath — no two-message panel.
        let synthetic = MessageId::new(9_100_000_000_000_000_123);
        assert!(is_synthetic_headless_message_id(synthetic));
        assert!(!bridge_should_create_two_message_status_panel(
            true, false, true, None, synthetic,
        ));
    }

    #[tokio::test]
    async fn create_puts_panel_below_answer_and_opens_generation_epoch() {
        // The #3805 P2 ordering assertion: the ANSWER (`current_msg_id`) is sent
        // first and stays put; the panel is a NEW message created BELOW it. Only
        // the panel handle + generation epoch change.
        let _env = isolate_agentdesk_runtime_root();
        let answer = MessageId::new(10);
        let panel_id = 20;
        let gateway = SendTrackingGateway::returning(panel_id);
        let mut inflight = test_inflight(answer.get());
        inflight.current_msg_len = 42;
        inflight.response_sent_offset = 7;
        inflight.full_response = "partial answer".to_string();
        let mut status_panel_msg_id: Option<MessageId> = None;
        let mut generation = inflight.status_panel_generation;
        let mut dirty = true;

        create_bridge_two_message_status_panel_below_answer(
            &gateway,
            ChannelId::new(777),
            "⠸",
            answer,
            &mut status_panel_msg_id,
            &mut inflight,
            &mut generation,
            &mut dirty,
        )
        .await;

        // Panel is the NEW (below) message; the answer anchor is UNCHANGED.
        assert_eq!(status_panel_msg_id, Some(MessageId::new(panel_id)));
        assert_eq!(inflight.status_message_id, Some(panel_id));
        assert_eq!(inflight.current_msg_id, answer.get());
        // Answer content / offsets are untouched (not swapped like the OFF path).
        assert_eq!(inflight.current_msg_len, 42);
        assert_eq!(inflight.response_sent_offset, 7);
        assert_eq!(inflight.full_response, "partial answer");
        // Generation epoch opened (0 → 1) and mirrored back to the caller-local.
        assert_eq!(inflight.status_panel_generation, 1);
        assert_eq!(generation, 1);
        assert!(
            dirty,
            "a successful create must keep the panel dirty for edits"
        );
        // Exactly one message was sent — the panel, carrying the status block.
        let sent = gateway.sent.lock().expect("sent lock");
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains('⠸'));
    }

    #[tokio::test]
    async fn create_failure_clears_dirty_and_leaves_answer_and_generation() {
        // A failed send must not bind a panel, must not bump the epoch, and must
        // clear the dirty flag so the loop does not keep retrying an edit against
        // a panel that was never created.
        let _env = isolate_agentdesk_runtime_root();
        let answer = MessageId::new(10);
        let gateway = SendTrackingGateway::failing();
        let mut inflight = test_inflight(answer.get());
        let mut status_panel_msg_id: Option<MessageId> = None;
        let mut generation = inflight.status_panel_generation;
        let mut dirty = true;

        create_bridge_two_message_status_panel_below_answer(
            &gateway,
            ChannelId::new(777),
            "⠸",
            answer,
            &mut status_panel_msg_id,
            &mut inflight,
            &mut generation,
            &mut dirty,
        )
        .await;

        assert_eq!(status_panel_msg_id, None);
        assert_eq!(inflight.status_message_id, None);
        assert_eq!(inflight.current_msg_id, answer.get());
        assert_eq!(inflight.status_panel_generation, 0);
        assert_eq!(generation, 0);
        assert!(!dirty, "a failed create must clear the dirty flag");
    }

    #[test]
    fn generation_guard_is_ownership_scoped_and_off_inert() {
        // Default-OFF / single-turn PR-B: this-turn generation equals the on-disk
        // epoch, so nothing is stale (0 == 0 and 1 == 1).
        assert!(!two_message_status_edit_generation_is_stale(0, true, 0));
        assert!(!two_message_status_edit_generation_is_stale(1, true, 1));
        // A newer on-disk epoch for the SAME (owned) panel supersedes the stale
        // edit — the re-anchor guard PR-D/E rely on.
        assert!(two_message_status_edit_generation_is_stale(1, true, 2));
        // Not owned on disk → never suppress, even if the epoch is higher (an
        // unrelated turn's epoch must not gate this completion).
        assert!(!two_message_status_edit_generation_is_stale(1, false, 9));
    }
}
