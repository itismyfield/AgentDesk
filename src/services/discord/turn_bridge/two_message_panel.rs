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
///
/// #3805 P2 (PR-C): visible to the whole `discord` module so the tmux WATCHER
/// completion guard reuses this exact predicate — the sink and watcher must
/// share ONE generation staleness rule (parity), not two divergent copies.
pub(in crate::services::discord) fn two_message_status_edit_generation_is_stale(
    this_turn_generation: u64,
    panel_owned_on_disk: bool,
    on_disk_generation: u64,
) -> bool {
    panel_owned_on_disk && on_disk_generation > this_turn_generation
}

/// #3805 P2 (PR-D) shared gate (pure): should the driver RE-ANCHOR the
/// two-message status panel BELOW the new answer chunk after a mid-turn answer
/// rollover?
///
/// Fires only when the two-message rollout flag is ON and this turn actually has
/// a live separate panel (`status_panel_present`) to move — footer-mode / v2-OFF
/// / synthetic turns carry no separate panel and never re-anchor. OFF returns
/// `false`, so the rollover path is byte-identical to today (no send/delete/CAS).
///
/// Visible to the whole `discord` module so the tmux WATCHER rollover call site
/// reuses this exact gate — the sink and watcher must share ONE re-anchor
/// decision (parity), not two divergent copies (mirrors the shared generation
/// staleness predicate above).
pub(in crate::services::discord) fn two_message_should_reanchor_panel_on_rollover(
    two_message_panel_enabled: bool,
    status_panel_present: bool,
) -> bool {
    two_message_panel_enabled && status_panel_present
}

/// #3805 P2 (PR-D): re-anchor the sink's two-message status panel BELOW the new
/// answer chunk after a mid-turn rollover created a fresh tail answer message.
///
/// Sequence:
/// 1. Send the NEW panel BELOW the new tail answer and immediately record it in
///    the durable orphan store as a crash-window safety net.
/// 2. Persist `status_message_id` + `status_panel_generation` before deleting
///    the old panel, then remove the new panel's orphan record.
/// 3. Retire the stranded OLD panel above the answer; on a delete
///    failure record it in the durable orphan store so the sweeper reclaims it
///    (never a permanently stranded "in progress" panel).
/// 4. Repoint `status_message_id` to the new panel, keep the answer anchor
///    coherent, and BUMP `status_panel_generation` (CAS epoch ++): every stale
///    in-flight completion tagged with the OLD epoch for the SAME owned panel is
///    now stale-skipped by `two_message_status_edit_generation_is_stale`, while
///    this turn's own completion (mirrored to the new epoch) passes.
///
/// This is pure msg-id / HTTP bookkeeping — it never tears down the per-channel
/// `StatusPanelState`, so item4's `session_banner` exactly-once claim is
/// untouched. When there is no live panel (`status_panel_msg_id.is_none()`) it is
/// a no-op returning `false`. On a NEW-panel send failure the OLD panel and epoch
/// are left intact (no partial re-anchor) and it returns `false`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn reanchor_bridge_two_message_status_panel_below_answer<
    G: TurnGateway + ?Sized,
>(
    gateway: &G,
    shared: &SharedData,
    channel_id: ChannelId,
    provider: &ProviderKind,
    panel_text: &str,
    new_answer_msg_id: MessageId,
    status_panel_msg_id: &mut Option<MessageId>,
    inflight_state: &mut InflightTurnState,
    status_panel_generation: &mut u64,
    last_status_panel_text: &mut String,
) -> bool {
    let Some(old_panel_id) = *status_panel_msg_id else {
        return false;
    };
    match gateway.send_message(channel_id, panel_text).await {
        Ok(new_panel_id) => {
            crate::services::discord::status_panel_orphan_store::enqueue_pending_bind(
                provider,
                &shared.token_hash,
                channel_id.get(),
                new_panel_id.get(),
                Some(
                    crate::services::discord::inflight::InflightTurnIdentity::from_state(
                        inflight_state,
                    ),
                ),
            );
            let mut updated = inflight_state.clone();
            updated.status_message_id = Some(new_panel_id.get());
            // The answer chunk stays the tail; keep the persisted anchor coherent
            // (idempotent — the loop already advanced it to new_answer_msg_id).
            updated.current_msg_id = new_answer_msg_id.get();
            let next_generation = updated.status_panel_generation.saturating_add(1);
            updated.status_panel_generation = next_generation;
            let save_outcome =
                crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(
                    &updated,
                    "turn_bridge::two_message_panel::reanchor",
                );
            if save_outcome != crate::services::discord::inflight::GuardedSaveOutcome::Saved {
                tracing::warn!(
                    "[turn_bridge] #3805 P2 skipped/failed to persist re-anchored two-message status panel {} in channel {}: {:?}",
                    new_panel_id,
                    channel_id,
                    save_outcome
                );
                if gateway
                    .delete_message(channel_id, new_panel_id)
                    .await
                    .is_ok()
                {
                    crate::services::discord::status_panel_orphan_store::remove(
                        provider,
                        &shared.token_hash,
                        channel_id.get(),
                        new_panel_id.get(),
                    );
                }
                return false;
            }
            crate::services::discord::status_panel_orphan_store::remove(
                provider,
                &shared.token_hash,
                channel_id.get(),
                new_panel_id.get(),
            );
            *inflight_state = updated;
            *status_panel_msg_id = Some(new_panel_id);
            *status_panel_generation = next_generation;
            *last_status_panel_text = panel_text.to_string();
            if gateway
                .delete_message(channel_id, old_panel_id)
                .await
                .is_err()
            {
                // `TurnGateway` only surfaces a string error here, not the HTTP
                // status. Queueing even a permanent delete failure is
                // outcome-equivalent to the watcher path: the orphan drain
                // classifies 403/404/410 and drops the record on its next pass.
                crate::services::discord::status_panel_orphan_store::enqueue_separate_status_panel_orphan(
                    shared.ui.status_panel_v2_enabled,
                    provider,
                    &shared.token_hash,
                    channel_id.get(),
                    old_panel_id.get(),
                );
            }
            true
        }
        Err(error) => {
            tracing::warn!(
                "[turn_bridge] #3805 P2 failed to re-anchor two-message status panel below answer in channel {}: {}",
                channel_id,
                error
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::gateway::GatewayFuture;
    use std::sync::Mutex;

    /// #3293: `InflightTurnState::new` resolves the AgentDesk runtime store to
    /// stamp the born generation; the guard keeps this off the live
    /// `~/.adk/release`, falling back to a shared throwaway tempdir (#4514).
    /// Point `AGENTDESK_ROOT_DIR` at a per-test throwaway dir under the shared
    /// env lock so constructing a test inflight is
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

    fn make_status_panel_v2_shared_for_tests() -> Arc<SharedData> {
        let mut shared = super::super::make_shared_data_for_tests();
        Arc::get_mut(&mut shared)
            .expect("fresh shared data should be uniquely owned")
            .ui
            .status_panel_v2_enabled = true;
        shared
    }

    #[derive(Default)]
    struct SendTrackingGateway {
        sent: Arc<Mutex<Vec<String>>>,
        deleted: Arc<Mutex<Vec<u64>>>,
        send_id: u64,
        fail_send: bool,
        fail_delete: bool,
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
            message_id: MessageId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            let deleted = self.deleted.clone();
            let fail_delete = self.fail_delete;
            Box::pin(async move {
                deleted.lock().expect("deleted lock").push(message_id.get());
                if fail_delete {
                    Err("delete boom".to_string())
                } else {
                    Ok(())
                }
            })
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
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

    #[test]
    fn reanchor_gate_requires_flag_on_and_a_live_panel() {
        // OFF → never (the rollover path is byte-identical to today).
        assert!(!two_message_should_reanchor_panel_on_rollover(false, true));
        assert!(!two_message_should_reanchor_panel_on_rollover(false, false));
        // ON but no live panel → nothing to re-anchor.
        assert!(!two_message_should_reanchor_panel_on_rollover(true, false));
        // ON with a live panel → re-anchor below the new answer chunk.
        assert!(two_message_should_reanchor_panel_on_rollover(true, true));
    }

    #[tokio::test]
    async fn reanchor_moves_panel_below_new_answer_and_bumps_generation() {
        // The #3805 P2 (PR-D) re-anchor: send a NEW panel BELOW the new tail
        // answer, retire the OLD (stranded) panel, repoint status_message_id, and
        // BUMP the generation epoch so a stale OLD-epoch completion is later
        // stale-skipped.
        let _env = isolate_agentdesk_runtime_root();
        let old_panel = 20;
        let new_panel = 40;
        let new_answer = MessageId::new(30);
        let shared = make_status_panel_v2_shared_for_tests();
        let gateway = SendTrackingGateway::returning(new_panel);
        let mut inflight = test_inflight(new_answer.get());
        inflight.status_message_id = Some(old_panel);
        inflight.status_panel_generation = 1;
        let mut status_panel_msg_id: Option<MessageId> = Some(MessageId::new(old_panel));
        let mut generation = inflight.status_panel_generation;
        let mut last_status_panel_text = "stale old panel text".to_string();
        // #4091 r6: the reanchor persist is identity-guarded (reload-and-match);
        // the durable row must exist or the guarded save skips as Missing.
        crate::services::discord::inflight::save_inflight_state(&inflight)
            .expect("persist inflight row for guarded reanchor save");

        let reanchored = reanchor_bridge_two_message_status_panel_below_answer(
            &gateway,
            shared.as_ref(),
            ChannelId::new(777),
            &ProviderKind::Claude,
            "⠸ re-anchored panel",
            new_answer,
            &mut status_panel_msg_id,
            &mut inflight,
            &mut generation,
            &mut last_status_panel_text,
        )
        .await;

        assert!(reanchored);
        // Panel handle re-anchored to the NEW (below) message.
        assert_eq!(status_panel_msg_id, Some(MessageId::new(new_panel)));
        assert_eq!(inflight.status_message_id, Some(new_panel));
        // Answer anchor stays the tail (coherent persist).
        assert_eq!(inflight.current_msg_id, new_answer.get());
        // Generation epoch bumped 1 → 2 (CAS) and mirrored to the caller-local.
        assert_eq!(inflight.status_panel_generation, 2);
        assert_eq!(generation, 2);
        assert_eq!(last_status_panel_text, "⠸ re-anchored panel");
        // Exactly one new panel sent; the OLD panel was deleted.
        let sent = gateway.sent.lock().expect("sent lock");
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("re-anchored panel"));
        let deleted = gateway.deleted.lock().expect("deleted lock");
        assert_eq!(*deleted, vec![old_panel]);
        let pending = crate::services::discord::status_panel_orphan_store::load_pending(
            &ProviderKind::Claude,
            &shared.token_hash,
        );
        assert!(
            !pending.contains(&(777, new_panel)),
            "new panel orphan pre-registration must be removed after durable save"
        );
    }

    #[tokio::test]
    async fn reanchor_without_a_live_panel_is_a_noop() {
        // No separate panel (footer / v2-off / synthetic) → nothing to re-anchor;
        // no send, no delete, no epoch bump.
        let _env = isolate_agentdesk_runtime_root();
        let shared = make_status_panel_v2_shared_for_tests();
        let gateway = SendTrackingGateway::returning(40);
        let mut inflight = test_inflight(30);
        inflight.status_panel_generation = 1;
        let mut status_panel_msg_id: Option<MessageId> = None;
        let mut generation = 1;
        let mut last_status_panel_text = "keep".to_string();

        let reanchored = reanchor_bridge_two_message_status_panel_below_answer(
            &gateway,
            shared.as_ref(),
            ChannelId::new(777),
            &ProviderKind::Claude,
            "unused",
            MessageId::new(30),
            &mut status_panel_msg_id,
            &mut inflight,
            &mut generation,
            &mut last_status_panel_text,
        )
        .await;

        assert!(!reanchored);
        assert_eq!(status_panel_msg_id, None);
        assert_eq!(inflight.status_panel_generation, 1);
        assert_eq!(generation, 1);
        assert_eq!(last_status_panel_text, "keep");
        assert!(gateway.sent.lock().expect("sent lock").is_empty());
        assert!(gateway.deleted.lock().expect("deleted lock").is_empty());
    }

    #[tokio::test]
    async fn reanchor_send_failure_keeps_old_panel_and_epoch() {
        // A failed NEW-panel send must NOT retire the old panel, NOT bump the
        // epoch, and NOT repoint the handle — no partial re-anchor.
        let _env = isolate_agentdesk_runtime_root();
        let old_panel = 20;
        let shared = make_status_panel_v2_shared_for_tests();
        let gateway = SendTrackingGateway::failing();
        let mut inflight = test_inflight(30);
        inflight.status_message_id = Some(old_panel);
        inflight.status_panel_generation = 1;
        let mut status_panel_msg_id: Option<MessageId> = Some(MessageId::new(old_panel));
        let mut generation = 1;
        let mut last_status_panel_text = "old".to_string();

        let reanchored = reanchor_bridge_two_message_status_panel_below_answer(
            &gateway,
            shared.as_ref(),
            ChannelId::new(777),
            &ProviderKind::Claude,
            "⠸ new",
            MessageId::new(30),
            &mut status_panel_msg_id,
            &mut inflight,
            &mut generation,
            &mut last_status_panel_text,
        )
        .await;

        assert!(!reanchored);
        assert_eq!(status_panel_msg_id, Some(MessageId::new(old_panel)));
        assert_eq!(inflight.status_message_id, Some(old_panel));
        assert_eq!(inflight.status_panel_generation, 1);
        assert_eq!(generation, 1);
        assert_eq!(last_status_panel_text, "old");
        // The old panel was never deleted (no partial re-anchor).
        assert!(gateway.deleted.lock().expect("deleted lock").is_empty());
    }

    #[tokio::test]
    async fn reanchor_keeps_new_panel_orphan_registration_when_save_and_delete_fail() {
        let _env = isolate_agentdesk_runtime_root();
        let old_panel = 20;
        let new_panel = 40;
        let shared = make_status_panel_v2_shared_for_tests();
        let gateway = SendTrackingGateway {
            send_id: new_panel,
            fail_delete: true,
            ..SendTrackingGateway::default()
        };
        let mut inflight = test_inflight(30);
        inflight.provider = "unknown-provider".to_string();
        inflight.status_message_id = Some(old_panel);
        inflight.status_panel_generation = 1;
        let mut status_panel_msg_id: Option<MessageId> = Some(MessageId::new(old_panel));
        let mut generation = 1;
        let mut last_status_panel_text = "old".to_string();

        let reanchored = reanchor_bridge_two_message_status_panel_below_answer(
            &gateway,
            shared.as_ref(),
            ChannelId::new(777),
            &ProviderKind::Claude,
            "⠸ new",
            MessageId::new(30),
            &mut status_panel_msg_id,
            &mut inflight,
            &mut generation,
            &mut last_status_panel_text,
        )
        .await;

        assert!(!reanchored);
        assert_eq!(status_panel_msg_id, Some(MessageId::new(old_panel)));
        assert_eq!(inflight.status_message_id, Some(old_panel));
        assert_eq!(inflight.status_panel_generation, 1);
        assert_eq!(generation, 1);
        assert_eq!(last_status_panel_text, "old");
        assert_eq!(
            *gateway.deleted.lock().expect("deleted lock"),
            vec![new_panel]
        );
        let pending = crate::services::discord::status_panel_orphan_store::load_pending(
            &ProviderKind::Claude,
            &shared.token_hash,
        );
        assert!(
            pending.contains(&(777, new_panel)),
            "new panel must remain durably queued when it was sent but neither persisted nor deleted"
        );
    }
}
