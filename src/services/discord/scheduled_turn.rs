//! The scheduled-delivery mark of an inflight turn: one channel-effects decision
//! for every completer, and settlement of the delivery before the row is dropped.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use super::inflight::InflightTurnState;
use crate::services::scheduled_messages::turn_completion::{
    self, RecordOutcome, ScheduledTurnOutcome,
};

/// Launch-time scheduled-delivery facts of an inflight turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(in crate::services::discord) enum ScheduledTurnMark {
    /// An older row without a mark; isolation is resolved from its delivery.
    #[default]
    Legacy,
    NotScheduled,
    Scheduled {
        turn_id: String,
        isolated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<ScheduledTurnOutcome>,
    },
}

impl ScheduledTurnMark {
    /// The mark of a headless turn. `isolated` is the snapshot label that also
    /// derived the turn's isolated session key.
    pub(in crate::services::discord) fn at_headless_start(
        metadata: Option<&serde_json::Value>,
        turn_id: String,
        isolated: bool,
    ) -> Self {
        let scheduled = metadata
            .and_then(|metadata| metadata.get("scheduled_message_id"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| !id.trim().is_empty());
        if scheduled || isolated {
            Self::Scheduled {
                turn_id,
                isolated,
                outcome: None,
            }
        } else {
            Self::NotScheduled
        }
    }

    /// Store the outcome next to a delivery commit; a no-op for other marks.
    pub(in crate::services::discord) fn set_outcome(
        &mut self,
        value: Option<ScheduledTurnOutcome>,
    ) {
        if let Self::Scheduled { outcome, .. } = self {
            *outcome = value;
        }
    }

    pub(in crate::services::discord) fn outcome(&self) -> Option<ScheduledTurnOutcome> {
        match self {
            Self::Scheduled { outcome, .. } => *outcome,
            Self::Legacy | Self::NotScheduled => None,
        }
    }

    /// Whether this turn may own a delivery outcome at all.
    pub(in crate::services::discord) fn may_settle(&self) -> bool {
        !matches!(self, Self::NotScheduled)
    }
}

/// Whether a completion may write the channel's shared state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum ChannelEffects {
    Allowed,
    Suppressed,
}

impl ChannelEffects {
    pub(in crate::services::discord) fn suppressed(self) -> bool {
        self == Self::Suppressed
    }
}

/// The exact turn a row settles: the launch-time turn id, or the legacy
/// `discord:<channel>:<user_msg_id>` derivation for an unmarked row.
fn settle_turn_id(state: &InflightTurnState) -> Option<String> {
    match &state.scheduled {
        ScheduledTurnMark::Scheduled { turn_id, .. } => Some(turn_id.clone()),
        ScheduledTurnMark::Legacy => (state.user_msg_id != 0)
            .then(|| format!("discord:{}:{}", state.channel_id, state.user_msg_id)),
        ScheduledTurnMark::NotScheduled => None,
    }
}

async fn row_effects(pool: Option<&PgPool>, state: &InflightTurnState) -> ChannelEffects {
    match &state.scheduled {
        ScheduledTurnMark::NotScheduled => ChannelEffects::Allowed,
        ScheduledTurnMark::Scheduled { isolated, .. } => {
            if *isolated {
                ChannelEffects::Suppressed
            } else {
                ChannelEffects::Allowed
            }
        }
        ScheduledTurnMark::Legacy => {
            // Scheduled messages require PostgreSQL; without it no row is scheduled.
            let (Some(pool), Some(turn_id)) = (pool, settle_turn_id(state)) else {
                return ChannelEffects::Allowed;
            };
            match turn_completion::legacy_snapshot_turn(pool, &turn_id).await {
                Ok(Some(true)) => ChannelEffects::Suppressed,
                Ok(Some(false) | None) => ChannelEffects::Allowed,
                Err(error) => {
                    tracing::warn!(
                        turn_id,
                        error = %error,
                        "scheduled turn isolation lookup failed; withholding channel writeback"
                    );
                    ChannelEffects::Suppressed
                }
            }
        }
    }
}

/// One decision for every channel-scoped write of a completion. Each candidate
/// row of the completing turn is consulted; any isolated candidate suppresses.
pub(in crate::services::discord) async fn channel_effects(
    pool: Option<&PgPool>,
    rows: &[Option<&InflightTurnState>],
) -> ChannelEffects {
    for state in rows.iter().flatten() {
        if row_effects(pool, state).await.suppressed() {
            return ChannelEffects::Suppressed;
        }
    }
    ChannelEffects::Allowed
}

/// The provider session a restored channel session may be seeded with.
pub(in crate::services::discord) async fn channel_session_seed(
    pool: Option<&PgPool>,
    state: &InflightTurnState,
) -> Option<String> {
    if channel_effects(pool, &[Some(state)]).await.suppressed() {
        return None;
    }
    state.session_id.clone()
}

/// The result of settling one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum Settlement {
    /// The delivery row holds this turn's outcome (new or earlier record).
    Evidence,
    /// No running delivery owns this turn.
    NotOwned,
    /// Every attempt failed; the delivery will settle by timeout.
    Failed,
}

const RECORD_RETRY_DELAYS: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_millis(250),
    Duration::from_secs(1),
];

/// Bounded retry of one record attempt.
pub(in crate::services::discord) async fn record_with_retry<F, Fut>(mut attempt: F) -> Settlement
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<RecordOutcome, sqlx::Error>>,
{
    for delay in RECORD_RETRY_DELAYS {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        match attempt().await {
            Ok(outcome) if outcome.is_evidence() => return Settlement::Evidence,
            Ok(_) => return Settlement::NotOwned,
            Err(error) => tracing::warn!(
                error = %error,
                "scheduled turn outcome record failed; retrying"
            ),
        }
    }
    tracing::warn!(
        "scheduled turn outcome record exhausted its retries; the delivery settles by timeout"
    );
    Settlement::Failed
}

/// Record `outcome` for the turn this row belongs to.
pub(in crate::services::discord) async fn settle(
    pool: Option<&PgPool>,
    state: &InflightTurnState,
    outcome: Option<ScheduledTurnOutcome>,
) -> Option<Settlement> {
    let (pool, outcome, turn_id) = (pool?, outcome?, settle_turn_id(state)?);
    Some(record_with_retry(|| turn_completion::record(pool, &turn_id, outcome)).await)
}

/// The outcome a recovered row settles with: the one stored at its delivery
/// commit, or the answer's own classification for an unmarked row.
pub(in crate::services::discord) fn recovered_outcome(
    state: &InflightTurnState,
    answer: &str,
) -> Option<ScheduledTurnOutcome> {
    match &state.scheduled {
        ScheduledTurnMark::Scheduled { outcome, .. } => *outcome,
        ScheduledTurnMark::Legacy => turn_completion::classify(answer, false, false),
        ScheduledTurnMark::NotScheduled => None,
    }
}

/// The outcome of an answer restart recovery delivered itself: its own body
/// classification, unless an earlier commit of this row already stored one.
pub(in crate::services::discord) fn recovery_delivered_outcome(
    state: &InflightTurnState,
    answer: &str,
) -> Option<ScheduledTurnOutcome> {
    if !state.scheduled.may_settle() {
        return None;
    }
    state
        .scheduled
        .outcome()
        .or_else(|| turn_completion::classify(answer, false, false))
}

/// Boot settlement: record every row whose delivery was committed before the
/// previous process stopped, before any restore branch can clear the row.
pub(in crate::services::discord) async fn settle_loaded(
    pool: Option<&PgPool>,
    states: &[InflightTurnState],
) {
    for state in states
        .iter()
        .filter(|state| state.terminal_delivery_completed())
    {
        let outcome = recovered_outcome(state, &state.full_response);
        if let Some(settlement) = settle(pool, state, outcome).await {
            tracing::info!(
                channel_id = state.channel_id,
                user_msg_id = state.user_msg_id,
                ?settlement,
                "boot recovery settled a committed scheduled turn"
            );
        }
    }
}

#[cfg(test)]
#[path = "scheduled_turn_tests.rs"]
mod tests;
