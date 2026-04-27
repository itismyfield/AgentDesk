//! Placeholder lifecycle controller (#1255).
//!
//! Centralises the live-turn placeholder card lifecycle so every entry point
//! that wants to drive the `🔄 백그라운드 처리 중` -> terminal transition flow
//! goes through the same FSM and the same edit-coalescer.
//!
//! Lifecycle FSM:
//!   `NotCreated` → `Active` → `Completed | TimedOut | Aborted`
//!
//! Operating rules:
//!   - One `PlaceholderKey` (provider + channel + message) maps to exactly one
//!     Discord message id (1:1).
//!   - Repeated `ensure_active` calls with identical content collapse into a
//!     single Discord PATCH (coalescing).
//!   - Terminal transitions (`Completed` / `TimedOut` / `Aborted`) are
//!     idempotent — the second call only emits a trace, never a PATCH.
//!   - Once a key has reached a terminal state any further `ensure_active`
//!     calls are no-ops; `placeholder_sweeper` is responsible for cleaning up
//!     stale `Active` entries via its own TTL path.

use poise::serenity_prelude::{ChannelId, MessageId};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::services::provider::ProviderKind;

use super::formatting::{
    MonitorHandoffReason, MonitorHandoffStatus, build_monitor_handoff_placeholder_with_context,
};
use super::gateway::TurnGateway;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct PlaceholderKey {
    pub(super) provider: ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) message_id: MessageId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaceholderLifecycle {
    NotCreated,
    Active,
    Completed,
    TimedOut,
    Aborted,
}

#[derive(Debug, Clone)]
pub(super) struct PlaceholderActiveInput {
    pub(super) reason: MonitorHandoffReason,
    pub(super) started_at_unix: i64,
    pub(super) tool_summary: Option<String>,
    pub(super) command_summary: Option<String>,
    pub(super) context_line: Option<String>,
}

#[derive(Debug, Clone)]
struct PlaceholderEntry {
    state: PlaceholderLifecycle,
    /// Last-rendered placeholder text — used to coalesce duplicate `ensure_active`
    /// calls so we don't burn Discord rate-limit budget on no-op edits.
    last_rendered: Option<String>,
    /// Snapshot of the last `Active` input. Terminal transitions reuse the
    /// snapshot to render the consistent header/footer pair.
    active_snapshot: Option<PlaceholderActiveInput>,
}

impl Default for PlaceholderEntry {
    fn default() -> Self {
        Self {
            state: PlaceholderLifecycle::NotCreated,
            last_rendered: None,
            active_snapshot: None,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct PlaceholderController {
    entries: dashmap::DashMap<PlaceholderKey, Arc<Mutex<PlaceholderEntry>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaceholderControllerOutcome {
    /// Discord PATCH was issued and committed.
    Edited,
    /// Lifecycle transition succeeded but coalesced — no PATCH was issued.
    /// The caller should treat this as success.
    Coalesced,
    /// Terminal transition called twice — idempotent no-op.
    AlreadyTerminal,
    /// PATCH attempted but failed; caller decides whether to retry.
    EditFailed,
    /// Transition was rejected because the FSM forbids it (e.g. `Active` after
    /// terminal state).  Treated as a no-op with a trace.
    Rejected,
}

impl PlaceholderController {
    fn entry(&self, key: &PlaceholderKey) -> Arc<Mutex<PlaceholderEntry>> {
        if let Some(existing) = self.entries.get(key) {
            return existing.clone();
        }
        self.entries
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(PlaceholderEntry::default())))
            .clone()
    }

    /// Drive the placeholder into `Active`. Issues a Discord PATCH only if the
    /// rendered content actually changed since the last call. Safe to call on
    /// every assistant token / tool event tick.
    pub(super) async fn ensure_active<G: TurnGateway + ?Sized>(
        &self,
        gateway: &G,
        key: PlaceholderKey,
        input: PlaceholderActiveInput,
    ) -> PlaceholderControllerOutcome {
        let entry = self.entry(&key);
        let mut guarded = entry.lock().await;

        // Forbid re-activating after a terminal transition — placeholder_sweeper
        // owns stale-Active recovery and we never want to drag a closed card
        // back into the Active state.
        if matches!(
            guarded.state,
            PlaceholderLifecycle::Completed
                | PlaceholderLifecycle::TimedOut
                | PlaceholderLifecycle::Aborted
        ) {
            return PlaceholderControllerOutcome::Rejected;
        }

        let rendered = build_monitor_handoff_placeholder_with_context(
            MonitorHandoffStatus::Active,
            input.reason,
            input.started_at_unix,
            input.tool_summary.as_deref(),
            input.command_summary.as_deref(),
            input.context_line.as_deref(),
        );

        // Coalesce identical re-renders into a single PATCH.  Tool-stream
        // ticks can fire several times per second and Discord rate-limits
        // PATCHes <100ms apart.
        if guarded.last_rendered.as_deref() == Some(rendered.as_str())
            && matches!(guarded.state, PlaceholderLifecycle::Active)
        {
            return PlaceholderControllerOutcome::Coalesced;
        }

        let edit_result = gateway
            .edit_message(key.channel_id, key.message_id, &rendered)
            .await;

        match edit_result {
            Ok(_) => {
                guarded.state = PlaceholderLifecycle::Active;
                guarded.last_rendered = Some(rendered);
                guarded.active_snapshot = Some(input);
                PlaceholderControllerOutcome::Edited
            }
            Err(_) => PlaceholderControllerOutcome::EditFailed,
        }
    }

    /// Drive the placeholder into a terminal state. Idempotent: a second call
    /// for the same `(key, status)` pair returns `AlreadyTerminal` without
    /// emitting a PATCH.  Calling with a different terminal status after a
    /// terminal transition is also rejected (returns `AlreadyTerminal`) — once
    /// closed, the card stays closed.
    pub(super) async fn transition<G: TurnGateway + ?Sized>(
        &self,
        gateway: &G,
        key: PlaceholderKey,
        target: PlaceholderLifecycle,
    ) -> PlaceholderControllerOutcome {
        debug_assert!(
            matches!(
                target,
                PlaceholderLifecycle::Completed
                    | PlaceholderLifecycle::TimedOut
                    | PlaceholderLifecycle::Aborted
            ),
            "transition() expects a terminal target, got {:?}",
            target
        );
        let entry = self.entry(&key);
        let mut guarded = entry.lock().await;

        if matches!(
            guarded.state,
            PlaceholderLifecycle::Completed
                | PlaceholderLifecycle::TimedOut
                | PlaceholderLifecycle::Aborted
        ) {
            return PlaceholderControllerOutcome::AlreadyTerminal;
        }

        // Reject terminal transitions before the placeholder ever became
        // `Active` — there is nothing on Discord to update, so silently no-op.
        let snapshot = match guarded.active_snapshot.clone() {
            Some(snapshot) => snapshot,
            None => {
                // Mark terminal anyway so future Active calls remain rejected.
                guarded.state = target;
                return PlaceholderControllerOutcome::Rejected;
            }
        };

        let status = match target {
            PlaceholderLifecycle::Completed => MonitorHandoffStatus::Completed,
            PlaceholderLifecycle::TimedOut => MonitorHandoffStatus::TimedOut,
            PlaceholderLifecycle::Aborted => MonitorHandoffStatus::Aborted,
            other => unreachable!("non-terminal target {:?}", other),
        };

        let rendered = build_monitor_handoff_placeholder_with_context(
            status,
            snapshot.reason,
            snapshot.started_at_unix,
            snapshot.tool_summary.as_deref(),
            snapshot.command_summary.as_deref(),
            snapshot.context_line.as_deref(),
        );

        let edit_result = gateway
            .edit_message(key.channel_id, key.message_id, &rendered)
            .await;

        match edit_result {
            Ok(_) => {
                guarded.state = target;
                guarded.last_rendered = Some(rendered);
                PlaceholderControllerOutcome::Edited
            }
            Err(_) => PlaceholderControllerOutcome::EditFailed,
        }
    }

    /// Snapshot the current lifecycle state for a key, mainly used by tests
    /// and the placeholder_sweeper.
    pub(super) async fn lifecycle(&self, key: &PlaceholderKey) -> PlaceholderLifecycle {
        match self.entries.get(key) {
            Some(arc) => arc.lock().await.state,
            None => PlaceholderLifecycle::NotCreated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::gateway::{HeadlessGateway, TurnGateway};
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key() -> PlaceholderKey {
        PlaceholderKey {
            provider: ProviderKind::Codex,
            channel_id: ChannelId::new(1_000_000_000_000_001),
            message_id: MessageId::new(2_000_000_000_000_001),
        }
    }

    fn sample_input() -> PlaceholderActiveInput {
        PlaceholderActiveInput {
            reason: MonitorHandoffReason::ExplicitCall,
            started_at_unix: 1_700_000_000,
            tool_summary: Some("Monitor".to_string()),
            command_summary: Some("session=foo".to_string()),
            context_line: Some("⏳ CI 통과 신호 대기".to_string()),
        }
    }

    /// Counts every `edit_message` call so we can assert coalescing /
    /// idempotency without depending on real Discord.
    struct CountingGateway {
        edits: AtomicUsize,
        last_edit: tokio::sync::Mutex<Option<String>>,
        fail: bool,
    }

    impl CountingGateway {
        fn new() -> Self {
            Self {
                edits: AtomicUsize::new(0),
                last_edit: tokio::sync::Mutex::new(None),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                edits: AtomicUsize::new(0),
                last_edit: tokio::sync::Mutex::new(None),
                fail: true,
            }
        }
    }

    impl TurnGateway for CountingGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _content: &'a str,
        ) -> super::super::gateway::GatewayFuture<'a, Result<MessageId, String>> {
            Box::pin(async { Ok(MessageId::new(1)) })
        }

        fn edit_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            content: &'a str,
        ) -> super::super::gateway::GatewayFuture<'a, Result<(), String>> {
            let content = content.to_string();
            Box::pin(async move {
                self.edits.fetch_add(1, Ordering::SeqCst);
                *self.last_edit.lock().await = Some(content);
                if self.fail {
                    Err("HTTP 500 simulated".to_string())
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
        ) -> super::super::gateway::GatewayFuture<
            'a,
            Result<crate::services::discord::formatting::ReplaceLongMessageOutcome, String>,
        > {
            Box::pin(async {
                Ok(crate::services::discord::formatting::ReplaceLongMessageOutcome::EditedOriginal)
            })
        }

        fn add_reaction<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _emoji: char,
        ) -> super::super::gateway::GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn remove_reaction<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _emoji: char,
        ) -> super::super::gateway::GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: MessageId,
            _user_text: &'a str,
        ) -> super::super::gateway::GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn dispatch_queued_turn<'a>(
            &'a self,
            _channel_id: ChannelId,
            _intervention: &'a crate::services::discord::Intervention,
            _request_owner_name: &'a str,
            _has_more_queued_turns: bool,
        ) -> super::super::gateway::GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn validate_live_routing<'a>(
            &'a self,
            _channel_id: ChannelId,
        ) -> super::super::gateway::GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn requester_mention(&self) -> Option<String> {
            None
        }

        fn can_chain_locally(&self) -> bool {
            false
        }

        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            Some(ProviderKind::Codex)
        }
    }

    // Acceptance scenario 1 (#1255): Active first edit issues a PATCH and
    // surfaces the context_line ("⏳ CI 통과 신호 대기") in the rendered card.
    #[tokio::test]
    async fn ensure_active_first_call_emits_patch_with_context_line() {
        // Touch HeadlessGateway so the import isn't dead in the cfg(test) block
        // — also serves as a smoke check that the trait it implements is the
        // same one our CountingGateway implements.
        fn _assert_impls<T: TurnGateway>() {}
        _assert_impls::<HeadlessGateway>();
        let gateway = Arc::new(CountingGateway::new());
        let controller = PlaceholderController::default();
        let outcome = controller
            .ensure_active(gateway.as_ref(), key(), sample_input())
            .await;
        assert_eq!(outcome, PlaceholderControllerOutcome::Edited);
        assert_eq!(gateway.edits.load(Ordering::SeqCst), 1);
        let last = gateway.last_edit.lock().await.clone().unwrap();
        assert!(last.contains("🔄 **백그라운드 처리 중**"));
        assert!(last.contains("⏳ CI 통과 신호 대기"));
        assert_eq!(
            controller.lifecycle(&key()).await,
            PlaceholderLifecycle::Active
        );
    }

    // Acceptance scenario 2: Repeated ensure_active with identical input
    // coalesces into a single PATCH.  Defends against the rate-limit risk
    // documented in #1255 §리스크.
    #[tokio::test]
    async fn ensure_active_coalesces_identical_input() {
        let gateway = Arc::new(CountingGateway::new());
        let controller = PlaceholderController::default();
        let _ = controller
            .ensure_active(gateway.as_ref(), key(), sample_input())
            .await;
        let outcome = controller
            .ensure_active(gateway.as_ref(), key(), sample_input())
            .await;
        assert_eq!(outcome, PlaceholderControllerOutcome::Coalesced);
        assert_eq!(gateway.edits.load(Ordering::SeqCst), 1);
    }

    // Acceptance scenario 3: Active → Completed terminal transition.
    #[tokio::test]
    async fn transition_completed_after_active_emits_patch() {
        let gateway = Arc::new(CountingGateway::new());
        let controller = PlaceholderController::default();
        let _ = controller
            .ensure_active(gateway.as_ref(), key(), sample_input())
            .await;
        let outcome = controller
            .transition(gateway.as_ref(), key(), PlaceholderLifecycle::Completed)
            .await;
        assert_eq!(outcome, PlaceholderControllerOutcome::Edited);
        assert_eq!(
            controller.lifecycle(&key()).await,
            PlaceholderLifecycle::Completed
        );
        let last = gateway.last_edit.lock().await.clone().unwrap();
        assert!(last.contains("✅ **백그라운드 완료**"));
    }

    // Acceptance scenario 4: Active → TimedOut + Active → Aborted both render
    // the corresponding terminal header.
    #[tokio::test]
    async fn transition_timeout_and_abort_render_correct_headers() {
        let gateway = Arc::new(CountingGateway::new());
        let controller = PlaceholderController::default();

        let key_t = PlaceholderKey {
            message_id: MessageId::new(3001),
            ..key()
        };
        let _ = controller
            .ensure_active(gateway.as_ref(), key_t.clone(), sample_input())
            .await;
        let _ = controller
            .transition(gateway.as_ref(), key_t, PlaceholderLifecycle::TimedOut)
            .await;
        let last = gateway.last_edit.lock().await.clone().unwrap();
        assert!(last.contains("⏱ **백그라운드 타임아웃**"));

        let key_a = PlaceholderKey {
            message_id: MessageId::new(3002),
            ..key()
        };
        let _ = controller
            .ensure_active(gateway.as_ref(), key_a.clone(), sample_input())
            .await;
        let _ = controller
            .transition(gateway.as_ref(), key_a, PlaceholderLifecycle::Aborted)
            .await;
        let last = gateway.last_edit.lock().await.clone().unwrap();
        assert!(last.contains("⚠ **백그라운드 중단**"));
    }

    // Acceptance scenario 5: Calling `transition(Completed)` twice issues
    // exactly one PATCH; the second is `AlreadyTerminal`.  Defends the
    // idempotency requirement from the issue's "추가 DoD".
    #[tokio::test]
    async fn terminal_transition_is_idempotent() {
        let gateway = Arc::new(CountingGateway::new());
        let controller = PlaceholderController::default();
        let _ = controller
            .ensure_active(gateway.as_ref(), key(), sample_input())
            .await;
        let edits_before_terminal = gateway.edits.load(Ordering::SeqCst);
        let first = controller
            .transition(gateway.as_ref(), key(), PlaceholderLifecycle::Completed)
            .await;
        let second = controller
            .transition(gateway.as_ref(), key(), PlaceholderLifecycle::Completed)
            .await;
        assert_eq!(first, PlaceholderControllerOutcome::Edited);
        assert_eq!(second, PlaceholderControllerOutcome::AlreadyTerminal);
        // exactly one terminal PATCH on top of the Active PATCH
        assert_eq!(
            gateway.edits.load(Ordering::SeqCst) - edits_before_terminal,
            1
        );
    }

    // Acceptance scenario 6 (#1255 race-protection DoD): two callers driving
    // the same (channel, message_id) pair through the controller resolve
    // serially — only one Active PATCH is issued, and the terminal transition
    // remains correct.
    #[tokio::test]
    async fn concurrent_active_calls_serialize_through_controller() {
        let gateway = Arc::new(CountingGateway::new());
        let controller = Arc::new(PlaceholderController::default());

        let g1 = gateway.clone();
        let g2 = gateway.clone();
        let c1 = controller.clone();
        let c2 = controller.clone();
        let k = key();
        let i = sample_input();

        let k1 = k.clone();
        let h1 = tokio::spawn(async move { c1.ensure_active(g1.as_ref(), k1, i).await });
        let i2 = sample_input();
        let h2 = tokio::spawn(async move { c2.ensure_active(g2.as_ref(), k, i2).await });
        let (r1, r2) = (h1.await.unwrap(), h2.await.unwrap());
        // first wins → Edited; the loser sees identical content → Coalesced
        let edits = [r1, r2];
        assert!(edits.contains(&PlaceholderControllerOutcome::Edited));
        assert!(edits.contains(&PlaceholderControllerOutcome::Coalesced));
        assert_eq!(gateway.edits.load(Ordering::SeqCst), 1);
    }

    // Acceptance scenario 7: After a terminal transition, ensure_active is
    // rejected — closed cards stay closed, defending against a `tmux_handed_off`
    // edit racing in after Monitor already terminated.
    #[tokio::test]
    async fn ensure_active_after_terminal_is_rejected() {
        let gateway = Arc::new(CountingGateway::new());
        let controller = PlaceholderController::default();
        let _ = controller
            .ensure_active(gateway.as_ref(), key(), sample_input())
            .await;
        let _ = controller
            .transition(gateway.as_ref(), key(), PlaceholderLifecycle::Completed)
            .await;
        let edits_after_terminal = gateway.edits.load(Ordering::SeqCst);
        let outcome = controller
            .ensure_active(gateway.as_ref(), key(), sample_input())
            .await;
        assert_eq!(outcome, PlaceholderControllerOutcome::Rejected);
        assert_eq!(gateway.edits.load(Ordering::SeqCst), edits_after_terminal);
    }

    // Acceptance scenario 8: gateway-level edit failure surfaces as
    // `EditFailed` and does NOT mutate lifecycle state, so a retry can fix it.
    #[tokio::test]
    async fn edit_failure_keeps_state_unchanged_for_retry() {
        let gateway = Arc::new(CountingGateway::failing());
        let controller = PlaceholderController::default();
        let outcome = controller
            .ensure_active(gateway.as_ref(), key(), sample_input())
            .await;
        assert_eq!(outcome, PlaceholderControllerOutcome::EditFailed);
        assert_eq!(
            controller.lifecycle(&key()).await,
            PlaceholderLifecycle::NotCreated
        );
    }
}
