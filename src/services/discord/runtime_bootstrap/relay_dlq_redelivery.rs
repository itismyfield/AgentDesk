//! #5941 — the planning half of the `relay_dead_letter` redelivery. Nothing calls
//! `build_plan` yet: the claim, the POST and the settle are not in this module.
//!
//! One row carries TWO coordinate systems and they are never mixed. The merge
//! reads response-String bytes only (`response_sent_offset .. full_response_len`,
//! which bound `content` exactly); the JSONL offsets in the same `reason` belong
//! to the watcher that wrote them.

use crate::db::relay_dead_letter as dlq;
use std::collections::BTreeMap;

/// What one row covers of the response String, plus its merge fences.
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
    /// Rows with no destination to name: channel id, anchor message id or span
    /// unusable. **Not a verdict about the body** — no witness was consulted and
    /// none could be. Settling these as `dlq::REDELIVERY_DECLINED` ("A witness
    /// answered...") records an answer nobody gave; use a state of your own.
    pub unaddressable: Vec<i64>,
}

fn field<'a>(reason: &'a str, key: &str) -> Option<&'a str> {
    reason
        .split_whitespace()
        .find_map(|token| token.strip_prefix(key))
}

/// Read a row's span, refusing it unless the span describes THIS row's content:
/// `content` is `full_response[response_sent_offset..]`, so the span width IS its
/// byte length, and any other value means the merge would trim at the wrong place.
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
/// Recording is at-least-once, so one loss leaves rows sharing a prefix. Rows
/// group, then walk in span order against a frontier: a row inside it is
/// superseded, a row extending it contributes only the bytes past it. The
/// frontier is local to this call, so a loss wider than one batch is
/// deduplicated only within each batch.
///
/// The stranded placeholder must stay in the group key: it is the only per-TURN
/// key in the row, while `generation_mtime_ns` survives across turns and
/// `response_sent_offset` restarts at 0 in each, so grouping without it merges
/// two turns and trims the later one against the earlier one's frontier.
pub(super) fn build_plan(rows: Vec<dlq::ClaimedDeadLetter>) -> RedeliveryPlan {
    type SpannedRows = Vec<(RowSpan, u64, dlq::ClaimedDeadLetter)>;
    let mut plan = RedeliveryPlan::default();
    let mut groups: BTreeMap<(u64, u64, String, i64), SpannedRows> = BTreeMap::new();
    for row in rows {
        // No channel to post in, or no placeholder to reply to: see
        // `RedeliveryPlan::unaddressable`.
        let (Ok(channel_id), Some(anchor)) = (
            row.channel_id.parse::<u64>(),
            row.message_id
                .as_deref()
                .and_then(|id| id.parse::<u64>().ok()),
        ) else {
            plan.unaddressable.push(row.id);
            continue;
        };
        let Some(span) = parse_span(&row.reason, row.content.len()) else {
            plan.unaddressable.push(row.id);
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

#[cfg(test)]
mod tests;
