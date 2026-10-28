//! Receipts of every owner that ends an execution and removes its row, committed before the row
//! goes; a row whose commit failed stays marked until boot or the next admission records it.

use std::time::Duration;

use chrono::{DateTime, Utc};
use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use super::SharedData;
use super::inflight::{
    self, GuardedClearOutcome, GuardedSaveOutcome, InflightTurnIdentity, InflightTurnState,
    TerminalReceiptStamp,
};
use super::turn_finalizer::{FinalizeContext, TerminalEvent};
use crate::db::turns::{TerminalOutcome, TurnReceipt, record_turn_receipt};
use crate::services::provider::ProviderKind;

#[cfg(test)]
#[path = "receipt_settle_tests.rs"]
mod tests;

const RECORD_RETRY_DELAYS: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_millis(250),
    Duration::from_secs(1),
];

/// Completed when the execution left an answer, empty otherwise.
pub(in crate::services::discord) fn body_outcome(body: &str) -> TerminalOutcome {
    if body.trim().is_empty() {
        TerminalOutcome::Empty
    } else {
        TerminalOutcome::Completed
    }
}

/// How a watcher-committed terminal ended.
pub(in crate::services::discord) fn watcher_outcome(
    stopped: bool,
    has_answer: bool,
) -> TerminalOutcome {
    match (stopped, has_answer) {
        (true, _) => TerminalOutcome::Cancelled,
        (false, true) => TerminalOutcome::Completed,
        (false, false) => TerminalOutcome::Empty,
    }
}

/// Commits the terminal of the managed execution `state` ran, retrying briefly. True once it is
/// durable or there is nothing to commit (no database, or a message-less execution).
pub(in crate::services::discord) async fn record(
    pool: Option<&PgPool>,
    state: &InflightTurnState,
    outcome: TerminalOutcome,
    confirmed_at: DateTime<Utc>,
) -> bool {
    let (Some(pool), Some(provider)) = (pool, state.provider_kind()) else {
        return true;
    };
    if state.user_msg_id == 0 {
        return true;
    }
    let turn_id = format!("discord:{}:{}", state.channel_id, state.user_msg_id);
    let channel = state
        .logical_channel_id
        .unwrap_or(state.channel_id)
        .to_string();
    let receipt = TurnReceipt {
        turn_id: &turn_id,
        channel_id: &channel,
        agent_id: None,
        provider: provider.as_str(),
        session_key: state.session_key.as_deref(),
        started_at: Some(state.started_at.as_str()),
        consumed_origin_ids: &state.input_origin_ids,
        outcome,
        confirmed_at,
    };
    for delay in RECORD_RETRY_DELAYS {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        match record_turn_receipt(pool, &receipt).await {
            Ok(()) => return true,
            Err(error) => tracing::warn!(%error, turn_id, "turn receipt not committed"),
        }
    }
    false
}

/// Records the terminal before the caller removes the row. False when the row must stay: the
/// commit failed and the row still owes an origin its receipt, marked when the mark was written.
pub(in crate::services::discord) async fn record_or_stamp(
    pool: Option<&PgPool>,
    state: &InflightTurnState,
    outcome: TerminalOutcome,
) -> bool {
    let stamp = state.terminal_receipt.unwrap_or(TerminalReceiptStamp {
        outcome,
        confirmed_at: Utc::now(),
    });
    if record(pool, state, stamp.outcome, stamp.confirmed_at).await {
        return true;
    }
    !keep_marked(state, stamp)
}

/// After a failed commit: marks the row so it stays until its receipt is recorded. True keeps
/// the row, even unmarked when the mark cannot be written; a row naming no origin may go.
pub(in crate::services::discord) fn keep_marked(
    state: &InflightTurnState,
    stamp: TerminalReceiptStamp,
) -> bool {
    if state.terminal_receipt.is_some() {
        return true;
    }
    let Some(provider) = state
        .provider_kind()
        .filter(|_| state.owes_origin_receipt())
    else {
        tracing::warn!(
            channel_id = state.channel_id,
            user_msg_id = state.user_msg_id,
            "turn receipt lost; the row names no origin to keep it for"
        );
        return false;
    };
    let marked = inflight::stamp_terminal_receipt_if_matches_identity(
        &provider,
        state.channel_id,
        &InflightTurnIdentity::from_state(state),
        state.turn_nonce.as_deref(),
        stamp,
    );
    if marked != GuardedSaveOutcome::Saved {
        tracing::warn!(
            channel_id = state.channel_id,
            ?marked,
            "turn receipt mark not written"
        );
    }
    true
}

/// Records a marked row's terminal and removes the row; false while the commit still fails.
async fn settle_marked(pool: Option<&PgPool>, state: &InflightTurnState) -> bool {
    let (Some(stamp), Some(provider)) = (state.terminal_receipt, state.provider_kind()) else {
        return false;
    };
    if !record(pool, state, stamp.outcome, stamp.confirmed_at).await {
        return false;
    }
    matches!(
        inflight::clear_inflight_state_for_captured_episode(
            &provider,
            state.channel_id,
            &InflightTurnIdentity::from_state(state),
            state.turn_nonce.as_deref(),
        ),
        GuardedClearOutcome::Cleared | GuardedClearOutcome::Missing
    )
}

/// The next admission on a channel held by a marked row records that row and frees the channel.
pub(in crate::services::discord) async fn settle_marked_row(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
) -> bool {
    match inflight::load_inflight_state_read_only(provider, channel_id) {
        Some(row) if row.terminal_receipt.is_some() => {
            settle_marked(shared.pg_pool.as_ref(), &row).await
        }
        _ => false,
    }
}

/// The managed rows a boot restore was handed, to settle any it removes without an ending.
pub(in crate::services::discord) struct BootReceipts {
    watched: Vec<InflightTurnState>,
}

/// Boot settlement before any restore branch: committed scheduled deliveries, marked rows
/// (recorded and kept out of restore, which must not revive them) and already-delivered rows.
pub(in crate::services::discord) async fn settle_boot(
    shared: &SharedData,
    states: Vec<InflightTurnState>,
) -> (Vec<InflightTurnState>, BootReceipts) {
    let pool = shared.pg_pool.as_ref();
    super::scheduled_turn::settle_loaded(pool, &states).await;
    let mut kept = Vec::with_capacity(states.len());
    let mut watched = Vec::new();
    for state in states {
        if state.terminal_receipt.is_some() {
            settle_marked(pool, &state).await;
            continue;
        }
        if state.terminal_delivery_completed() {
            let outcome = body_outcome(&state.full_response);
            record(pool, &state, outcome, Utc::now()).await;
        }
        if state.user_msg_id != 0 {
            watched.push(state.clone());
        }
        kept.push(state);
    }
    (kept, BootReceipts { watched })
}

impl BootReceipts {
    /// After the restore: a handed row it removed while no row of the same execution remains
    /// ended without a recorded terminal, so it failed. An earlier receipt wins over this one.
    pub(in crate::services::discord) async fn settle_cleared(
        self,
        shared: &SharedData,
        provider: &ProviderKind,
    ) {
        for row in &self.watched {
            let running = inflight::load_inflight_state_read_only(provider, row.channel_id)
                .is_some_and(|current| current.user_msg_id == row.user_msg_id);
            if !running {
                record(
                    shared.pg_pool.as_ref(),
                    row,
                    TerminalOutcome::Failed,
                    Utc::now(),
                )
                .await;
            }
        }
    }
}

/// The terminal of a row a finalizer claim removes, decided by the finalize's own event.
#[derive(Clone, Copy)]
enum FinalizerOutcome {
    Body,
    Fixed(TerminalOutcome),
}

/// Whether a finalizer claim removes the row, and the terminal it commits first.
pub(in crate::services::discord) struct FinalizerClear {
    clear: bool,
    outcome: Option<FinalizerOutcome>,
}

impl FinalizerClear {
    pub(in crate::services::discord) fn new(event: &TerminalEvent, ctx: FinalizeContext) -> Self {
        let outcome = match event {
            TerminalEvent::Complete => FinalizerOutcome::Body,
            TerminalEvent::RelayMiss => FinalizerOutcome::Fixed(TerminalOutcome::Empty),
            // A backstop cancel settles a turn whose host was proven gone, not a user stop.
            TerminalEvent::Cancel if ctx.is_backstop_reconcile_path() => {
                FinalizerOutcome::Fixed(TerminalOutcome::Failed)
            }
            TerminalEvent::Cancel => FinalizerOutcome::Fixed(TerminalOutcome::Cancelled),
            TerminalEvent::GateTimeout { .. } | TerminalEvent::OperatorRelease(_) => {
                FinalizerOutcome::Fixed(TerminalOutcome::Failed)
            }
            // The terminated host, which the Herdr census keeps unnamed here, ends as a stop.
            #[cfg(unix)]
            other if other.is_mute_settlement() => {
                FinalizerOutcome::Fixed(TerminalOutcome::Cancelled)
            }
            #[cfg(unix)]
            _ => FinalizerOutcome::Fixed(TerminalOutcome::Failed),
        };
        Self {
            clear: ctx.clear_inflight,
            outcome: Some(outcome),
        }
    }

    /// Commits the terminal of the row this claim is about to remove; false keeps it marked.
    pub(in crate::services::discord) async fn record_before_clear(
        &self,
        shared: &SharedData,
        row: &InflightTurnState,
    ) -> bool {
        let outcome = match self.outcome {
            // A claim with no terminal of its own never removes a row still owed its receipt.
            None => return row.terminal_receipt.is_none(),
            Some(FinalizerOutcome::Body) => body_outcome(&row.full_response),
            Some(FinalizerOutcome::Fixed(outcome)) => outcome,
        };
        record_or_stamp(shared.pg_pool.as_ref(), row, outcome).await
    }
}

/// A finalizer claim's removal request: a plain flag, or one carrying the terminal to commit.
pub(in crate::services::discord) trait ClaimClear {
    fn split(self) -> (bool, FinalizerClear);
}

impl ClaimClear for bool {
    fn split(self) -> (bool, FinalizerClear) {
        (
            self,
            FinalizerClear {
                clear: self,
                outcome: None,
            },
        )
    }
}

impl ClaimClear for FinalizerClear {
    fn split(self) -> (bool, FinalizerClear) {
        (self.clear, self)
    }
}

/// A forced stop that removes the row cancelled the execution; that receipt commits first.
pub(in crate::services::discord) async fn stopped(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    policy: super::TmuxCleanupPolicy,
) {
    if !policy.should_clear_inflight() {
        return;
    }
    if let Some(row) = inflight::load_inflight_state_read_only(provider, channel_id.get()) {
        let outcome = TerminalOutcome::Cancelled;
        record(shared.pg_pool.as_ref(), &row, outcome, Utc::now()).await;
    }
}

/// A dead pane ends the pinned turn its row still names: cancelled when the death followed a
/// stop, else failed. `stopped` is the watcher's own death judgment, which consumed the stop.
pub(in crate::services::discord) async fn pane_dead(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    pinned: Option<&InflightTurnIdentity>,
    stopped: Option<bool>,
) -> bool {
    let Some(row) = inflight::load_inflight_state_read_only(provider, channel_id.get()) else {
        return true;
    };
    let pinned_row = pinned.is_some_and(|identity| identity.matches_state(&row))
        && row.tmux_session_name.as_deref() == Some(tmux_session_name);
    // A planned restart or rebind keeps its row through the clear, so it ends nothing here.
    if !pinned_row || row.restart_mode.is_some() || row.rebind_origin {
        return true;
    }
    let stopped = stopped.unwrap_or_else(|| {
        let start = row.turn_start_offset.unwrap_or(0);
        super::tmux::recent_turn_stop_for_watcher_range(channel_id, tmux_session_name, start)
            .is_some()
    });
    let outcome = if stopped {
        TerminalOutcome::Cancelled
    } else {
        TerminalOutcome::Failed
    };
    record_or_stamp(shared.pg_pool.as_ref(), &row, outcome).await
}

/// A refused bridge entry fails its execution only when the row is still solely this turn's; a
/// row another turn, a restart marker or a watcher holds is that owner's to settle.
pub(in crate::services::discord) async fn entry_aborted(
    shared: &SharedData,
    channel_id: ChannelId,
    turn_nonce: Option<&str>,
) {
    let Some(row) = inflight::load_inflight_state_read_only(&shared.provider, channel_id.get())
    else {
        return;
    };
    let own = turn_nonce.is_some_and(|nonce| !nonce.is_empty())
        && row.owned_by_turn_nonce(turn_nonce)
        && row.restart_mode.is_none()
        && !row.rebind_origin
        && inflight::StreamRelayAuthority::from_state(&row).bridge_owns_relay();
    if own {
        record_or_stamp(shared.pg_pool.as_ref(), &row, TerminalOutcome::Failed).await;
    }
}
