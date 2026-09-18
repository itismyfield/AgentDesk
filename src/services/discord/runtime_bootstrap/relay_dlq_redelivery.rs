//! #5941 Step B — the `relay_dead_letter` reader that returns a lost terminal
//! body to the channel it was cut from. Step A made the body durable but
//! nothing read it back, so the user still never received it.
//!
//! One row carries TWO coordinate systems and they are never mixed here. The
//! merge works only in response-String bytes (`response_sent_offset` ..
//! `full_response_len`, which bound `content` exactly); the JSONL offsets in the
//! same `reason` belong to the watcher that wrote them and are not read.

use crate::db::relay_dead_letter as dlq;
use crate::services::discord::outbound::delivery_record;
use crate::services::discord::{SharedData, formatting};
use crate::services::provider::ProviderKind;
use futures::FutureExt;
use poise::serenity_prelude as serenity;
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const SWEEP_INTERVAL_SECS: u64 = 60;
#[cfg(not(test))]
const SWEEP_INITIAL_DELAY_SECS: u64 = 45;
#[cfg(test)]
const SWEEP_INITIAL_DELAY_SECS: u64 = 0;
/// Below this age the ordinary delivery path may still settle the turn itself.
const MIN_AGE_SECS: i64 = 120;
/// Past this age `recent_delivered_content_matches` has nothing left to answer
/// with, so the row is left pending for operator recovery rather than replayed
/// without a witness. Derived from that window so the two cannot drift apart.
const MAX_AGE_SECS: i64 = (delivery_record::RECENT_DELIVERED_CONTENT_WINDOW_MS / 1_000) as i64;
const BATCH: i64 = 32;
static SWEEP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// What one row covers of the response String, plus the fences that decide
/// which rows may be merged with which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RowSpan {
    start: usize,
    end: usize,
    generation_mtime_ns: i64,
    tmux_session: String,
    provider: String,
}

/// One POST the sweep will make, and the row it settles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RedeliverySlice {
    pub row_id: i64,
    pub channel_id: u64,
    pub anchor_message_id: u64,
    pub provider: String,
    pub tmux_session: String,
    pub body: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct RedeliveryPlan {
    pub slices: Vec<RedeliverySlice>,
    pub superseded: Vec<i64>,
    pub declined: Vec<i64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct RedeliveryTally {
    pub delivered: usize,
    pub superseded: usize,
    pub declined: usize,
    pub deferred: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SliceOutcome {
    Delivered,
    /// A witness answered: the body is already in the channel. Final.
    Declined,
    /// No witness could be read, or the POST itself failed. NOT a verdict about
    /// the body, so the row goes back to `pending` for a later claim instead of
    /// retiring unread.
    Deferred,
}

/// The POST side, injectable so the claim/merge/settle path can be driven
/// against a real pool without a Discord connection.
pub(super) trait SliceSink {
    fn deliver(
        &self,
        slice: &RedeliverySlice,
    ) -> impl std::future::Future<Output = SliceOutcome> + Send;
}

fn field<'a>(reason: &'a str, key: &str) -> Option<&'a str> {
    reason
        .split_whitespace()
        .find_map(|token| token.strip_prefix(key))
}

/// Read a row's span, refusing it unless the span describes THIS row's content.
/// `content` is `full_response[response_sent_offset..]`, so the span width IS
/// its byte length; any other value means the two are not the same coordinate
/// system and the merge below would trim at the wrong place.
fn parse_span(reason: &str, content_len: usize) -> Option<RowSpan> {
    let start: usize = field(reason, "response_sent_offset=")?.parse().ok()?;
    let end: usize = field(reason, "full_response_len=")?.parse().ok()?;
    let span = RowSpan {
        start,
        end,
        generation_mtime_ns: field(reason, "generation_mtime_ns=")?.parse().ok()?,
        tmux_session: field(reason, "tmux_session=")?.to_string(),
        provider: field(reason, "provider=")?.to_string(),
    };
    (end.checked_sub(start)? == content_len).then_some(span)
}

/// Round `index` up to the next UTF-8 boundary so a trim never splits a char.
fn char_boundary_at_or_after(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index.min(text.len())
}

/// Turn ONE claim batch into the POSTs that reproduce its union exactly once.
/// The frontier is local to this call, so a loss spanning more than `BATCH`
/// rows is still deduplicated only within each batch (see the follow-up noted
/// in the PR).
///
/// Recording is at-least-once, so one loss typically leaves several rows whose
/// bodies share a prefix. Rows group by channel, stranded placeholder, tmux
/// session and transcript generation, then walk in span order against a
/// frontier: a row already inside it is superseded, a row that extends it
/// contributes only the bytes past it.
///
/// The placeholder carries the grouping, because it is the only per-TURN key in
/// the row. `generation_mtime_ns` changes on spawn, restart and adoption, while
/// `response_sent_offset` restarts at 0 every turn, so grouping on the
/// generation alone would merge turns and trim a later one against an earlier
/// one's frontier. The merge arithmetic still reads response-String bytes only.
pub(super) fn build_plan(rows: Vec<dlq::ClaimedDeadLetter>) -> RedeliveryPlan {
    type SpannedRows = Vec<(RowSpan, u64, dlq::ClaimedDeadLetter)>;
    let mut plan = RedeliveryPlan::default();
    let mut groups: BTreeMap<(u64, u64, String, i64), SpannedRows> = BTreeMap::new();
    for row in rows {
        // No anchor means no witness that the body is still missing, so the row
        // is left for an operator rather than posted on the record alone.
        let (Ok(channel_id), Some(anchor)) = (
            row.channel_id.parse::<u64>(),
            row.message_id
                .as_deref()
                .and_then(|id| id.parse::<u64>().ok()),
        ) else {
            plan.declined.push(row.id);
            continue;
        };
        let Some(span) = parse_span(&row.reason, row.content.len()) else {
            plan.declined.push(row.id);
            continue;
        };
        let key = (
            channel_id,
            anchor,
            span.tmux_session.clone(),
            span.generation_mtime_ns,
        );
        groups.entry(key).or_default().push((span, anchor, row));
    }
    for ((channel_id, _, tmux_session, _), mut group) in groups {
        group.sort_by_key(|(span, _, row)| (span.start, span.end, row.id));
        let mut frontier = 0usize;
        for (span, anchor, row) in group {
            let cut = char_boundary_at_or_after(&row.content, frontier.saturating_sub(span.start));
            frontier = frontier.max(span.end);
            let Some(body) = row.content.get(cut..).filter(|t| !t.trim().is_empty()) else {
                plan.superseded.push(row.id);
                continue;
            };
            plan.slices.push(RedeliverySlice {
                row_id: row.id,
                channel_id,
                anchor_message_id: anchor,
                provider: span.provider,
                tmux_session: tmux_session.clone(),
                body: body.to_string(),
            });
        }
    }
    plan
}

/// Claim one batch, settle every claimed row, and report what happened.
pub(super) async fn sweep_once_with(
    pool: &sqlx::PgPool,
    sink: &impl SliceSink,
) -> Result<RedeliveryTally, sqlx::Error> {
    let rows = dlq::claim_pending_redeliveries(
        pool,
        dlq::KIND_TERMINAL_NO_DELIVERY_OWNER,
        MIN_AGE_SECS,
        MAX_AGE_SECS,
        BATCH,
    )
    .await?;
    if rows.is_empty() {
        return Ok(RedeliveryTally::default());
    }
    let plan = build_plan(rows);
    let mut tally = RedeliveryTally {
        superseded: plan.superseded.len(),
        declined: plan.declined.len(),
        ..RedeliveryTally::default()
    };
    for (ids, state) in [
        (&plan.superseded, dlq::REDELIVERY_SUPERSEDED),
        (&plan.declined, dlq::REDELIVERY_DECLINED),
    ] {
        for id in ids {
            settle(pool, *id, state).await;
        }
    }
    for slice in &plan.slices {
        let state = match sink.deliver(slice).await {
            SliceOutcome::Delivered => {
                tally.delivered += 1;
                dlq::REDELIVERY_DELIVERED
            }
            SliceOutcome::Declined => {
                tally.declined += 1;
                dlq::REDELIVERY_DECLINED
            }
            SliceOutcome::Deferred => {
                tally.deferred += 1;
                dlq::REDELIVERY_PENDING
            }
        };
        settle(pool, slice.row_id, state).await;
    }
    Ok(tally)
}

async fn settle(pool: &sqlx::PgPool, id: i64, state: &str) {
    if let Err(error) = dlq::settle_redelivery(pool, id, state).await {
        tracing::warn!(dead_letter_id = id, state, "[dlq] settle failed: {error}");
    }
}

struct DiscordSliceSink<'a> {
    shared: &'a Arc<SharedData>,
    http: &'a serenity::Http,
}

impl SliceSink for DiscordSliceSink<'_> {
    async fn deliver(&self, slice: &RedeliverySlice) -> SliceOutcome {
        let channel = serenity::ChannelId::new(slice.channel_id);
        let anchor = serenity::MessageId::new(slice.anchor_message_id);
        // The stranded placeholder is the witness: while it still ends with the
        // streaming footer, nothing replaced it, so the body is not in the
        // channel. Failing to READ the witness is not that answer — a 429, a
        // 5xx or a dropped socket says nothing about the body — so it defers.
        let Ok(message) = self.http.get_message(channel, anchor).await else {
            return SliceOutcome::Deferred;
        };
        if !formatting::text_ends_with_streaming_footer(&message.content) {
            return SliceOutcome::Declined;
        }
        if ProviderKind::from_str(&slice.provider).is_some_and(|provider| {
            delivery_record::recent_delivered_content_matches(
                &provider,
                channel,
                &slice.tmux_session,
                &slice.body,
            )
        }) {
            return SliceOutcome::Declined;
        }
        let posted = formatting::send_long_message_raw_with_reference_returning_message_ids(
            self.http,
            channel,
            &slice.body,
            self.shared,
            Some((channel, anchor)),
        )
        .await;
        match posted {
            Ok(_) => SliceOutcome::Delivered,
            // A long body posts as several messages, so a failure here may have
            // landed a prefix; the retry can repeat it. Bounded duplication is
            // the lesser loss against never delivering the body at all.
            Err(error) => {
                tracing::warn!(
                    channel_id = slice.channel_id,
                    dead_letter_id = slice.row_id,
                    "[dlq] redelivery POST failed; row returns to pending: {error}"
                );
                SliceOutcome::Deferred
            }
        }
    }
}

/// Start the redelivery sweep. Returns whether it started, so a caller holding
/// a pool can tell "no PostgreSQL configured" from "already running".
pub(super) fn spawn_relay_dlq_redelivery(shared: Arc<SharedData>) -> bool {
    let Some(pool) = shared.pg_pool.clone() else {
        return false;
    };
    if SWEEP_ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    crate::services::discord::task_supervisor::spawn_observed("relay_dlq_redelivery", async move {
        tokio::time::sleep(std::time::Duration::from_secs(SWEEP_INITIAL_DELAY_SECS)).await;
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let Some(http) = shared.serenity_http_or_token_fallback() else {
                continue;
            };
            let sink = DiscordSliceSink {
                shared: &shared,
                http: &http,
            };
            // A panicking tick must not silently end the only reader.
            match AssertUnwindSafe(sweep_once_with(&pool, &sink))
                .catch_unwind()
                .await
            {
                Ok(Ok(tally)) if tally.delivered > 0 => {
                    tracing::info!(
                        ?tally,
                        "[dlq] returned lost terminal bodies to their channels"
                    )
                }
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!("[dlq] redelivery sweep tick failed: {error}"),
                Err(_) => tracing::error!("[dlq] sweep tick panicked; loop remains active"),
            }
        }
    });
    true
}

#[cfg(test)]
mod tests;
