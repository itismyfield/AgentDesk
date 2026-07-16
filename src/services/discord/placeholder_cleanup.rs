use poise::serenity_prelude::{ChannelId, MessageId};
use std::time::{Duration, Instant};

use super::inflight::InflightTurnState;
use crate::services::provider::ProviderKind;

const PLACEHOLDER_CLEANUP_TTL: Duration = Duration::from_secs(60 * 60);
const PLACEHOLDER_CLEANUP_CAPACITY: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaceholderCleanupOperation {
    DeleteTerminal,
    DeleteNonterminal,
    EditTerminal,
    EditPreserve,
    // #3034: audit/wire operation kind ("edit_handoff") not yet emitted by a
    // live cleanup path; kept as a stable audit-string surface.
    #[allow(dead_code)]
    EditHandoff,
}

impl PlaceholderCleanupOperation {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::DeleteTerminal => "delete_terminal",
            Self::DeleteNonterminal => "delete_nonterminal",
            Self::EditTerminal => "edit_terminal",
            Self::EditPreserve => "edit_preserve",
            Self::EditHandoff => "edit_handoff",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaceholderCleanupFailureClass {
    PermissionOrRoutingDiagnostic,
    LifecycleFailure,
}

impl PlaceholderCleanupFailureClass {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::PermissionOrRoutingDiagnostic => "permission_or_routing_diagnostic",
            Self::LifecycleFailure => "lifecycle_failure",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PlaceholderCleanupOutcome {
    Succeeded,
    AlreadyGone,
    Failed {
        class: PlaceholderCleanupFailureClass,
        detail: String,
    },
}

impl PlaceholderCleanupOutcome {
    pub(super) fn is_committed(&self) -> bool {
        matches!(self, Self::Succeeded | Self::AlreadyGone)
    }

    /// #3003: a delete failure that will never succeed on retry — the bot lacks
    /// permission (403) or the message is permanently gone (410). Distinct from a
    /// transient 5xx / rate-limit / network `Failed`. Callers that block turn
    /// finalization until a panel delete commits must treat these as terminal
    /// (give up the delete) so the turn does not wedge retrying forever. Matches
    /// the permanent classification used by `status_panel_orphan_store::drain`.
    pub(super) fn is_permanent_failure(&self) -> bool {
        match self {
            // Match HTTP-status *phrases*, not bare digit substrings (codex P2
            // r21): a Discord snowflake or retry delay in the error detail can
            // contain "403"/"410" without being the status. These phrases only
            // appear in an actual permission/gone status line.
            Self::Failed { detail, .. } => {
                let lower = detail.to_ascii_lowercase();
                lower.contains("403 forbidden")
                    || lower.contains("(403)")
                    || lower.contains("http 403")
                    || lower.contains("status code 403")
                    || lower.contains("410 gone")
                    || lower.contains("(410)")
                    || lower.contains("http 410")
                    || lower.contains("status code 410")
                    || lower.contains("missing permissions")
                    || lower.contains("missing access")
            }
            Self::Succeeded | Self::AlreadyGone => false,
        }
    }

    pub(super) fn failed(detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self::Failed {
            class: classify_cleanup_failure(&detail),
            detail,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlaceholderCleanupRecord {
    pub(super) provider: ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) message_id: MessageId,
    pub(super) tmux_session_name: Option<String>,
    pub(super) operation: PlaceholderCleanupOperation,
    pub(super) outcome: PlaceholderCleanupOutcome,
    pub(super) source: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PlaceholderCleanupKey {
    provider: String,
    channel_id: ChannelId,
    message_id: MessageId,
}

#[derive(Debug, Clone)]
struct StoredPlaceholderCleanupRecord {
    record: PlaceholderCleanupRecord,
    recorded_at: Instant,
}

#[derive(Debug, Default)]
pub(super) struct PlaceholderCleanupRegistry {
    records: dashmap::DashMap<PlaceholderCleanupKey, StoredPlaceholderCleanupRecord>,
}

impl PlaceholderCleanupRegistry {
    pub(super) fn record(&self, record: PlaceholderCleanupRecord) {
        self.prune_expired();
        let key = PlaceholderCleanupKey {
            provider: record.provider.as_str().to_string(),
            channel_id: record.channel_id,
            message_id: record.message_id,
        };
        self.records.insert(
            key,
            StoredPlaceholderCleanupRecord {
                record,
                recorded_at: Instant::now(),
            },
        );
        self.prune_capacity();
    }

    pub(super) fn terminal_cleanup_committed(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> bool {
        self.prune_expired();
        let key = PlaceholderCleanupKey {
            provider: provider.as_str().to_string(),
            channel_id,
            message_id,
        };
        self.records.get(&key).is_some_and(|stored| {
            matches!(
                stored.record.operation,
                PlaceholderCleanupOperation::DeleteTerminal
                    | PlaceholderCleanupOperation::EditTerminal
            ) && stored.record.outcome.is_committed()
        })
    }

    /// #3607 signal-(c): a committed terminal-cleanup tombstone marks this
    /// message id as the message a finished turn intentionally retired (deleted
    /// or edited into its terminal form). Semantic alias over the existing
    /// [`Self::terminal_cleanup_committed`] tombstone lookup — same
    /// `DeleteTerminal | EditTerminal & committed` predicate — surfaced under a
    /// guard-oriented name so the cleanup-protection call sites read clearly.
    /// The tombstone survives the inflight row being cleared (TTL 1h), so it is
    /// the durable authority a generic janitor consults before deleting.
    pub(super) fn is_committed_terminal_anchor(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> bool {
        self.terminal_cleanup_committed(provider, channel_id, message_id)
    }

    pub(super) fn terminal_cleanup_retry_pending(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> bool {
        self.prune_expired();
        let key = PlaceholderCleanupKey {
            provider: provider.as_str().to_string(),
            channel_id,
            message_id,
        };
        self.records.get(&key).is_some_and(|stored| {
            matches!(
                stored.record.operation,
                PlaceholderCleanupOperation::DeleteTerminal
                    | PlaceholderCleanupOperation::EditTerminal
            ) && matches!(
                stored.record.outcome,
                PlaceholderCleanupOutcome::Failed { .. }
            )
        })
    }

    fn prune_expired(&self) {
        let now = Instant::now();
        self.records
            .retain(|_, stored| now.duration_since(stored.recorded_at) <= PLACEHOLDER_CLEANUP_TTL);
    }

    fn prune_capacity(&self) {
        let excess = self
            .records
            .len()
            .saturating_sub(PLACEHOLDER_CLEANUP_CAPACITY);
        if excess == 0 {
            return;
        }

        let mut oldest: Vec<_> = self
            .records
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().recorded_at))
            .collect();
        oldest.sort_by_key(|(_, recorded_at)| *recorded_at);
        for (key, _) in oldest.into_iter().take(excess) {
            self.records.remove(&key);
        }
    }
}

pub(super) fn classify_cleanup_failure(detail: &str) -> PlaceholderCleanupFailureClass {
    let lower = detail.to_ascii_lowercase();
    if lower.contains("403")
        || lower.contains("forbidden")
        || lower.contains("missing permissions")
        || lower.contains("missing access")
        || lower.contains("not allowed for bot settings")
        || lower.contains("channelnotallowed")
        || lower.contains("agentnotallowed")
        || lower.contains("routing")
        || lower.contains("wrong bot")
    {
        PlaceholderCleanupFailureClass::PermissionOrRoutingDiagnostic
    } else {
        PlaceholderCleanupFailureClass::LifecycleFailure
    }
}

#[cfg(test)]
mod permanent_failure_tests {
    use super::PlaceholderCleanupOutcome;

    // #4158: the watcher's skip-already-committed arm clears its live
    // placeholder ONLY when the guarded cleanup reports a committed outcome
    // (`is_committed()`), i.e. the residue was actually deleted (or was already
    // gone). A preserve decision returns `None` from the helper (not covered
    // here — it never reaches an outcome), and a transient `Failed` must NOT be
    // treated as committed, so the placeholder id is kept for the next pass /
    // durable orphan record. This pins the exact gate the arm relies on.
    #[test]
    fn only_succeeded_or_already_gone_counts_as_committed_cleanup() {
        assert!(PlaceholderCleanupOutcome::Succeeded.is_committed());
        assert!(PlaceholderCleanupOutcome::AlreadyGone.is_committed());
        assert!(
            !PlaceholderCleanupOutcome::failed("HTTP 500 Internal Server Error").is_committed(),
            "a transient delete failure must not clear the placeholder (kept for retry)"
        );
        assert!(
            !PlaceholderCleanupOutcome::failed("HTTP 403 Forbidden: Missing Permissions")
                .is_committed(),
            "even a permanent-failure delete is not a committed cleanup"
        );
    }

    #[test]
    fn permanent_failure_matches_http_status_phrases_not_digit_substrings() {
        // #3003 codex P2 r21: real permanent statuses are permanent.
        for detail in [
            "HTTP 403 Forbidden: Missing Permissions",
            "Unsuccessful request (403)",
            "HTTP 403",
            "error: status code 403",
            "HTTP 410 Gone",
            "Discord error (410)",
            "HTTP 410",
            "status code 410",
            "Missing Access",
        ] {
            assert!(
                PlaceholderCleanupOutcome::failed(detail).is_permanent_failure(),
                "{detail}"
            );
        }
    }

    #[test]
    fn permanent_failure_does_not_match_incidental_digit_substrings() {
        // A snowflake / retry delay containing 403/410 is NOT an HTTP status.
        for detail in [
            "503 Service Unavailable",
            "rate limited, retry after 4103ms",
            "timeout deleting message 1410403000000000000",
            "connection reset",
        ] {
            assert!(
                !PlaceholderCleanupOutcome::failed(detail).is_permanent_failure(),
                "{detail}"
            );
        }
    }

    #[test]
    fn committed_outcomes_are_not_permanent_failures() {
        assert!(!PlaceholderCleanupOutcome::Succeeded.is_permanent_failure());
        assert!(!PlaceholderCleanupOutcome::AlreadyGone.is_permanent_failure());
    }
}

pub(super) fn classify_delete_error(detail: &str) -> PlaceholderCleanupOutcome {
    let lower = detail.to_ascii_lowercase();
    if lower.contains("404") || lower.contains("unknown message") || lower.contains("not found") {
        PlaceholderCleanupOutcome::AlreadyGone
    } else {
        PlaceholderCleanupOutcome::failed(detail)
    }
}

/// #3607 cleanup-protection guard: true when `message_id` is a committed
/// terminal anchor that a generic cleanup path (orphan-spinner sweep, panel
/// reclaim, replay-prefix GC, idle/monitoring janitors) must NOT delete.
///
/// Fuses the two trustworthy "this turn already retired this message" signals:
///   - signal (a) — the *live* inflight row, when one is still in scope: a turn
///     whose own `current_msg_id` equals `message_id` and whose terminal
///     delivery already committed owns that message; deleting it would race the
///     turn's own finalization. This is the fast path that needs no registry
///     write to have landed yet.
///   - signal (c) — the durable [`PlaceholderCleanupRegistry`] tombstone
///     ([`PlaceholderCleanupRegistry::is_committed_terminal_anchor`]). It is
///     written by the terminal-cleanup commit and survives the inflight row
///     being cleared (TTL 1h, production-wired in `tmux_watcher.rs`), so it is
///     the authority for the accident shape this guard exists to stop: the
///     inflight is already gone but a janitor is about to delete the message the
///     terminal cleanup just committed.
///
/// Signal (b) (the `delivery_record` panel-message ledger) is deliberately
/// EXCLUDED: it is #3089 dead code (env OFF, and even the accident record stores
/// `panel_msg_id = null`), so it would never fire. When #3089 cuts over, add it
/// here as an additional OR-term.
pub(in crate::services::discord) fn committed_terminal_anchor_protects_delete(
    registry: &PlaceholderCleanupRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    live_inflight: Option<&InflightTurnState>,
) -> bool {
    if let Some(inflight) = live_inflight
        && inflight.current_msg_id == message_id.get()
        && inflight.terminal_delivery_completed()
    {
        return true;
    }
    registry.is_committed_terminal_anchor(provider, channel_id, message_id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TerminalCleanupDeleteProtection {
    CommittedTerminal,
    RetryPending,
}

impl TerminalCleanupDeleteProtection {
    pub(in crate::services::discord) fn relay_delete_outcome(self) -> &'static str {
        match self {
            Self::CommittedTerminal => "skipped_committed_terminal",
            Self::RetryPending => "skipped_terminal_retry_pending",
        }
    }
}

pub(in crate::services::discord) fn terminal_cleanup_protects_delete(
    registry: &PlaceholderCleanupRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Option<TerminalCleanupDeleteProtection> {
    if committed_terminal_anchor_protects_delete(registry, provider, channel_id, message_id, None) {
        return Some(TerminalCleanupDeleteProtection::CommittedTerminal);
    }
    registry
        .terminal_cleanup_retry_pending(provider, channel_id, message_id)
        .then_some(TerminalCleanupDeleteProtection::RetryPending)
}

/// #3607 panel-sweep terminal-anchor guard wrapper. Returns true (and emits the
/// `skipped_committed_terminal` delete event + a log) when the orphan
/// status-panel sweeper is about to delete a committed terminal anchor — so the
/// sweeper bails before the destructive delete. Wraps
/// [`committed_terminal_anchor_protects_delete`] so the (non-hot but
/// near-1000-LoC) sweeper file only carries the compact call.
pub(in crate::services::discord) fn committed_terminal_panel_anchor_skip(
    registry: &PlaceholderCleanupRegistry,
    provider: &ProviderKind,
    channel_id: ChannelId,
    panel_msg: MessageId,
    live_inflight: &InflightTurnState,
) -> bool {
    if !committed_terminal_anchor_protects_delete(
        registry,
        provider,
        channel_id,
        panel_msg,
        Some(live_inflight),
    ) {
        return false;
    }
    tracing::info!(
        "[placeholder_sweeper] 🛡 #3607 preserved orphan status-panel anchor — committed terminal cleanup owns msg {} (channel {})",
        panel_msg.get(),
        channel_id.get()
    );
    crate::services::observability::emit_relay_delete(
        provider.as_str(),
        channel_id.get(),
        panel_msg.get(),
        None,
        None,
        "placeholder_sweeper_orphan_panel",
        PlaceholderCleanupOperation::DeleteTerminal.as_str(),
        "skipped_committed_terminal",
        None,
    );
    true
}

/// #3607: emit the durable `relay_delete` observation for the orphan
/// status-panel sweeper's *actual* delete (the path that runs when the
/// terminal-anchor guard above did NOT skip). Outcome mirrors the sweeper's own
/// convergence branches: `Ok` → committed, an `Err` whose Discord status is a
/// permanent message-gone (404/403/410, via
/// [`super::placeholder_sweeper::is_permanent_message_gone_status`]) →
/// already_gone, any other `Err` → failed. The panel is a non-terminal cleanup.
/// Observation only — the caller's enqueue / convergence logic is unchanged, and
/// this lives here (not in the near-1000-LoC sweeper) so that file carries only
/// the compact call.
pub(in crate::services::discord) fn emit_orphan_panel_sweep_delete<T>(
    provider: &ProviderKind,
    channel_id: ChannelId,
    panel_msg: MessageId,
    result: &Result<T, poise::serenity_prelude::Error>,
) {
    let permanent = result
        .as_ref()
        .err()
        .is_some_and(orphan_panel_delete_is_permanent_gone);
    let outcome = panel_sweep_delete_outcome(result.is_ok(), permanent);
    let detail = result.as_ref().err().map(|err| err.to_string());
    crate::services::observability::emit_relay_delete(
        provider.as_str(),
        channel_id.get(),
        panel_msg.get(),
        None,
        None,
        "placeholder_sweeper_orphan_panel",
        PlaceholderCleanupOperation::DeleteNonterminal.as_str(),
        outcome,
        detail.as_deref(),
    );
}

/// #3607: pure 3-way outcome classifier for the orphan-panel sweep delete (and
/// the orphan-store drain), split out from [`emit_orphan_panel_sweep_delete`] so
/// the committed / already_gone / failed mapping is unit-testable without
/// constructing a live `serenity::Error`. `permanent` is the caller's
/// permanent-gone (404/403/410) match; it is only consulted on the error path.
pub(in crate::services::discord) fn panel_sweep_delete_outcome(
    committed: bool,
    permanent: bool,
) -> &'static str {
    if committed {
        "committed"
    } else if permanent {
        "already_gone"
    } else {
        "failed"
    }
}

/// True when this delete error is a permanent message-gone status the sweeper
/// treats as success (404/403/410, via
/// [`super::placeholder_sweeper::is_permanent_message_gone_status`]).
fn orphan_panel_delete_is_permanent_gone(err: &poise::serenity_prelude::Error) -> bool {
    matches!(err, poise::serenity_prelude::Error::Http(http_err)
    if http_err.status_code().is_some_and(|status| {
        super::placeholder_sweeper::is_permanent_message_gone_status(status.as_u16())
    }))
}

/// True when the placeholder abandoned branch in `sweep_orphan_status_panel`
/// will NOT evict this row this pass — either it has no placeholder
/// (`current_msg_id == 0`) or it already streamed partial output (the
/// partial-response guard at the top of `run_placeholder_sweep_pass`). For those
/// rows the panel sweep must clear the persisted `status_message_id` itself to
/// converge; for rows the placeholder branch WILL evict, clearing there would
/// only refresh the file mtime and defer that eviction (codex P2 r12). Lives
/// here (not in the near-1000-LoC sweeper) so that file stays under the giant
/// threshold (#3607).
pub(in crate::services::discord) fn placeholder_sweep_leaves_row_unevicted(
    state: &InflightTurnState,
) -> bool {
    state.current_msg_id == 0
        || (!state.long_running_placeholder_active
            && (!state.full_response.is_empty() || state.response_sent_offset > 0))
}

#[cfg(test)]
mod terminal_anchor_guard_tests {
    use super::*;

    const PROVIDER: ProviderKind = ProviderKind::Codex;

    fn channel() -> ChannelId {
        ChannelId::new(4242)
    }

    fn anchor() -> MessageId {
        MessageId::new(9001)
    }

    fn record(
        registry: &PlaceholderCleanupRegistry,
        message_id: MessageId,
        operation: PlaceholderCleanupOperation,
        outcome: PlaceholderCleanupOutcome,
    ) {
        registry.record(PlaceholderCleanupRecord {
            provider: PROVIDER,
            channel_id: channel(),
            message_id,
            tmux_session_name: None,
            operation,
            outcome,
            source: "test",
        });
    }

    fn inflight_with(current_msg_id: u64, terminal_committed: bool) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            PROVIDER,
            channel().get(),
            None,
            1,
            2,
            current_msg_id,
            "test".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        state.terminal_delivery_committed = terminal_committed;
        state
    }

    #[test]
    fn committed_terminal_tombstone_protects_delete() {
        // Signal (c): a committed terminal delete tombstone marks the anchor as
        // protected even without a live inflight.
        let registry = PlaceholderCleanupRegistry::default();
        record(
            &registry,
            anchor(),
            PlaceholderCleanupOperation::DeleteTerminal,
            PlaceholderCleanupOutcome::Succeeded,
        );
        assert!(committed_terminal_anchor_protects_delete(
            &registry,
            &PROVIDER,
            channel(),
            anchor(),
            None,
        ));
    }

    #[test]
    fn terminal_cleanup_delete_protection_distinguishes_commit_and_retry() {
        let committed = PlaceholderCleanupRegistry::default();
        record(
            &committed,
            anchor(),
            PlaceholderCleanupOperation::EditTerminal,
            PlaceholderCleanupOutcome::Succeeded,
        );
        let committed_protection =
            terminal_cleanup_protects_delete(&committed, &PROVIDER, channel(), anchor());
        assert_eq!(
            committed_protection,
            Some(TerminalCleanupDeleteProtection::CommittedTerminal),
        );
        assert_eq!(
            committed_protection.map(TerminalCleanupDeleteProtection::relay_delete_outcome),
            Some("skipped_committed_terminal"),
        );

        let retry_pending = PlaceholderCleanupRegistry::default();
        record(
            &retry_pending,
            anchor(),
            PlaceholderCleanupOperation::EditTerminal,
            PlaceholderCleanupOutcome::failed("transient terminal edit failure"),
        );
        let retry_protection =
            terminal_cleanup_protects_delete(&retry_pending, &PROVIDER, channel(), anchor());
        assert_eq!(
            retry_protection,
            Some(TerminalCleanupDeleteProtection::RetryPending),
        );
        assert_eq!(
            retry_protection.map(TerminalCleanupDeleteProtection::relay_delete_outcome),
            Some("skipped_terminal_retry_pending"),
        );
        assert_eq!(
            terminal_cleanup_protects_delete(
                &PlaceholderCleanupRegistry::default(),
                &PROVIDER,
                channel(),
                anchor(),
            ),
            None,
        );
    }

    #[test]
    fn non_terminal_or_unrecorded_anchor_is_not_protected() {
        let registry = PlaceholderCleanupRegistry::default();
        // A non-terminal cleanup record must not protect.
        record(
            &registry,
            anchor(),
            PlaceholderCleanupOperation::DeleteNonterminal,
            PlaceholderCleanupOutcome::Succeeded,
        );
        assert!(!committed_terminal_anchor_protects_delete(
            &registry,
            &PROVIDER,
            channel(),
            anchor(),
            None,
        ));
        // An unrecorded anchor must not protect.
        assert!(!committed_terminal_anchor_protects_delete(
            &registry,
            &PROVIDER,
            channel(),
            MessageId::new(7),
            None,
        ));
    }

    #[test]
    fn registry_protects_after_inflight_cleared() {
        // The accident shape #3607 exists to stop: the inflight row is already
        // gone (live_inflight = None) but the committed terminal tombstone alone
        // still protects the just-retired anchor from a generic janitor.
        let registry = PlaceholderCleanupRegistry::default();
        record(
            &registry,
            anchor(),
            PlaceholderCleanupOperation::EditTerminal,
            PlaceholderCleanupOutcome::Succeeded,
        );
        assert!(committed_terminal_anchor_protects_delete(
            &registry,
            &PROVIDER,
            channel(),
            anchor(),
            None,
        ));
    }

    #[test]
    fn live_inflight_fast_path_protects_with_empty_registry() {
        // Signal (a) only: registry is empty, but the live inflight owns the
        // anchor (same current_msg_id) and committed its terminal delivery.
        let registry = PlaceholderCleanupRegistry::default();
        let inflight = inflight_with(anchor().get(), true);
        assert!(committed_terminal_anchor_protects_delete(
            &registry,
            &PROVIDER,
            channel(),
            anchor(),
            Some(&inflight),
        ));
    }

    #[test]
    fn neither_signal_does_not_protect() {
        // Empty registry + a live inflight that neither owns the anchor nor has
        // committed terminal delivery → no protection.
        let registry = PlaceholderCleanupRegistry::default();
        let other_anchor = inflight_with(123, true);
        assert!(!committed_terminal_anchor_protects_delete(
            &registry,
            &PROVIDER,
            channel(),
            anchor(),
            Some(&other_anchor),
        ));
        let uncommitted = inflight_with(anchor().get(), false);
        assert!(!committed_terminal_anchor_protects_delete(
            &registry,
            &PROVIDER,
            channel(),
            anchor(),
            Some(&uncommitted),
        ));
    }

    #[test]
    fn panel_sweep_delete_outcome_classifies_three_ways() {
        // #3607: the actual-delete observation must split committed (Ok),
        // already_gone (permanent 404/403/410 Err), and failed (transient Err) —
        // the gap codex flagged was emitting only the guard-skip case.
        assert_eq!(panel_sweep_delete_outcome(true, false), "committed");
        // `committed` wins even if a stray permanent flag is set (Ok path).
        assert_eq!(panel_sweep_delete_outcome(true, true), "committed");
        assert_eq!(panel_sweep_delete_outcome(false, true), "already_gone");
        assert_eq!(panel_sweep_delete_outcome(false, false), "failed");
    }

    #[test]
    fn orphan_panel_sweep_committed_delete_emits_relay_delete() {
        // #3607: the wired actual-delete path emits a durable `relay_delete`
        // with outcome=committed on Ok, attributed to the sweeper panel site as a
        // non-terminal cleanup — the result-side observation that was missing.
        let _guard = crate::services::observability::test_runtime_lock();
        crate::services::observability::reset_for_tests();

        let ok: Result<(), poise::serenity_prelude::Error> = Ok(());
        emit_orphan_panel_sweep_delete(&PROVIDER, channel(), anchor(), &ok);

        let events = crate::services::observability::events::recent(50);
        let event = events
            .iter()
            .find(|event| event.event_type == "relay_delete")
            .expect("relay_delete should be in the recent ring");
        assert_eq!(event.channel_id, Some(channel().get()));
        assert_eq!(event.payload["message_id"], anchor().get());
        assert_eq!(event.payload["source"], "placeholder_sweeper_orphan_panel");
        assert_eq!(event.payload["operation_kind"], "delete_nonterminal");
        assert_eq!(event.payload["outcome"], "committed");
        // outcome doubles as the correlation status.
        assert_eq!(event.payload["status"], "committed");
    }
}
