//! Read-only projection index over the durable delivery evidence kept by
//! [`super::delivery_record`] (#5071 T4-B3, 4987 S2).
//!
//! It answers one question — "are these transcript bytes of this incarnation
//! already backed by durable delivery evidence?" — and it answers with the
//! subtraction 4987 §3.2 step 4 describes, which has TWO operands the record
//! keeps independently: `DeliveryRecord::confirmed_deliveries` (exact
//! per-receipt ranges) and `DeliveryRecord::delivered_frontier` (the
//! release-surviving watermark). Both are read; neither is written.
//!
//! This module has no verdict authority. It does not mutate the store, does not
//! produce a health or recovery decision, and nothing constructs it yet — the
//! `dead_code` warnings on its surface are the expected state until a later
//! slice composes the fact it returns.
//!
//! Each guarantee below belongs to a different subject and a different axis;
//! they are deliberately not collapsed into one sentence.
//!
//! - **The projection key omits `turn_nonce`.** The key is
//!   `(provider, tmux_session_name, generation_mtime_ns)`, so receipts from
//!   different turns over the same bytes of the same incarnation land under one
//!   key. On this axis only, dropping the nonce widens each key's receipt set:
//!   for a fixed store and a fixed obligation, the union this module compares
//!   against is a superset of the union a nonce-exact key would build, so the
//!   nonce omission can turn an uncovered answer into a covered one and not the
//!   reverse. This says nothing about the frontier operand or the range
//!   arithmetic, which have their own bullets.
//! - **`turn_nonce` must still be present.** [`project_receipt`] admits a
//!   receipt only through `ExactJsonlSourceIdentity::is_authoritative`, which
//!   requires a non-empty `turn_nonce`. So the comparison never reads the nonce
//!   VALUE while admission depends on its EXISTENCE; a receipt written without
//!   one contributes no coverage here.
//! - **Ranges are compared as a union.** [`ReceiptIndex::from_record`] sorts and
//!   merges each key's ranges (adjacent ends merge, so `(10,15)` and `(15,20)`
//!   become `(10,20)`) and [`ReceiptIndex::covers`] sweeps the merged list.
//!   Asking whether one single receipt contains the obligation would call
//!   `[10,20)` uncovered in exactly that case — a false uncovered answer, the
//!   direction 4987 §7 rules out.
//! - **The frontier contributes a prefix `[0, range.1)`, keyed by generation.**
//!   `DeliveredCommit` carries no provider or session field, so the only axis
//!   this module can check for it is `generation_mtime_ns` equality — the same
//!   axis `delivery_record::delivered_frontier_end_current_generation` uses when
//!   it turns a session name into the current `.generation` mtime and compares.
//!   The provider and session axes of a frontier are therefore the CALLER's,
//!   established by how the caller resolved `path` (see
//!   `delivery_record::delivery_record_path`), not re-derived here.
//! - **Generation distinctness is borrowed, per session, best-effort.**
//!   `GENERATION_MTIME_BUMP_STEPS_NS` in
//!   [`crate::services::discord::tmux_session_files`] tries escalating bumps to
//!   stamp a session's own `.generation` marker after that session's previous
//!   incarnation (#5437, #5439), and gives up rather than fail the respawn:
//!   `bump_generation_mtime_past_previous` returns the last OBSERVED mtime when
//!   every escalation step fails, the caller publishes the marker anyway, and
//!   `unbumpable_generation_mtime_still_publishes_the_marker_and_warns_5437`
//!   pins that published-equal path. That is a within-one-marker-file,
//!   best-effort relation.
//!
//! What this does NOT establish, kept explicit:
//!
//! - No same-session generation uniqueness on the bump-failure path. When the
//!   escalating bump gives up, a new incarnation can publish the same
//!   `generation_mtime_ns` as its predecessor, and this key cannot tell them
//!   apart — the predecessor's receipts and frontier then cover the successor's
//!   byte ranges (a false-covered residue, the direction 4987 §7 rules out for
//!   verdicts). A consumer slice must not promote coverage from an unproven
//!   generation to a green verdict without an additional witness.
//! - No transcript-EOF bound. `delivery_record`'s own frontier reader
//!   (`current_generation_durable_frontier_at`) additionally distrusts a
//!   frontier whose end exceeds the current transcript length (#4188), because
//!   an in-place rewrite can shorten a transcript without rotating its path.
//!   This index takes no EOF input and applies no such bound, so a stale-high
//!   same-generation frontier over-reports coverage. A consumer that needs the
//!   EOF guard has to apply it on its own inputs — [`ReceiptIndex::with_frontier_clamped_to_eof`]
//!   is the opt-in adapter for that, and `read_receipt_index_at` still does not
//!   apply it on its own because it has no transcript to measure.
//! - No cross-session frontier separation. Two different sessions' `.generation`
//!   markers carry no ordering relation to each other, so generation-only
//!   attribution conflates them if their marker mtimes coincide exactly.
//! - No partial salvage of an anomalous store. One receipt that fails
//!   [`project_receipt`] makes the whole read `Unknown`, which discards the
//!   frontier evidence in the same record along with it.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use super::delivery_record::{
    ConfirmedDeliveryReceipt, DeliveredCommit, DeliveryRecord, read_record_at,
};
use crate::services::provider::ProviderKind;

/// A successful read, a genuinely absent store, or a present store whose
/// coverage cannot be interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum ReceiptIndexRead {
    Ready(ReceiptIndex),
    Absent,
    Unknown(ReceiptIndexUnknownReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum ReceiptIndexUnknownReason {
    ReceiptStoreUnreadable,
}

/// The receipt projection coordinate of 4987 §-1.3 minus the turn axis. Byte
/// ranges are VALUES under this key, so no turn coordinate enters the index.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReceiptProjectionKey {
    provider: ProviderKind,
    tmux_session_name: String,
    generation_mtime_ns: i64,
}

/// The record's `delivered_frontier` reduced to what this index can check: an
/// end offset and the generation it was stamped under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrontierPrefix {
    generation_mtime_ns: i64,
    end: u64,
}

/// Merged delivered-range unions per projection key, plus the record's single
/// frontier prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct ReceiptIndex {
    /// Sorted and merged by [`ReceiptIndex::from_record`]; [`ReceiptIndex::covers`]
    /// relies on both properties.
    receipt_ranges: HashMap<ReceiptProjectionKey, Vec<(u64, u64)>>,
    frontier: Option<FrontierPrefix>,
}

impl ReceiptIndex {
    /// Pure union-coverage test for the obligation `[start, end)`.
    ///
    /// Covered means every byte of the obligation falls inside the merged
    /// receipt ranges of this key, the frontier prefix of this generation, or
    /// both together. An empty or inverted obligation is reported uncovered
    /// rather than trivially covered, so a degenerate range cannot be retired
    /// by this index.
    pub(in crate::services::discord) fn covers(
        &self,
        provider: &ProviderKind,
        tmux_session_name: &str,
        generation_mtime_ns: i64,
        obligation: (u64, u64),
    ) -> bool {
        let (start, end) = obligation;
        if end <= start {
            return false;
        }

        // The frontier is the low operand: it covers `[0, frontier.end)`, so it
        // can only advance the sweep cursor from the obligation start.
        let mut cursor = start;
        if let Some(frontier) = self.frontier
            && frontier.generation_mtime_ns == generation_mtime_ns
            && frontier.end > cursor
        {
            cursor = frontier.end;
        }
        if cursor >= end {
            return true;
        }

        let key = ReceiptProjectionKey {
            provider: provider.clone(),
            tmux_session_name: tmux_session_name.to_owned(),
            generation_mtime_ns,
        };
        let Some(ranges) = self.receipt_ranges.get(&key) else {
            return false;
        };
        // Sweep the merged ranges rather than asking whether ONE range contains the
        // obligation. `sort_and_merge` already folds overlapping and adjacent pairs,
        // so on its output the two forms agree; the sweep is kept because it does
        // not depend on that maximality, and a per-receipt containment test over
        // UNMERGED ranges is exactly the false-uncovered answer for
        // `(10,15)`+`(15,20)` that 4987 section 7 rules out.
        for &(range_start, range_end) in ranges {
            // Merged ranges are disjoint and ascending, so a range that starts
            // past the cursor leaves a gap no later range reaches back into.
            if range_start > cursor {
                return false;
            }
            if range_end > cursor {
                cursor = range_end;
            }
            if cursor >= end {
                return true;
            }
        }
        false
    }

    /// Bound the frontier operand by the transcript's current length, the guard
    /// the module docs above name as this index's missing EOF input (#4188).
    ///
    /// An in-place rewrite can shorten a transcript without rotating its path,
    /// which leaves a same-generation frontier pointing past the end of the file
    /// it was stamped against. [`ReceiptIndex::covers`] would then advance its
    /// sweep cursor over bytes that no longer exist and report them covered.
    /// Clamping shrinks the frontier operand only, so it can turn a covered
    /// answer into an uncovered one and never the reverse — the direction 4987
    /// §7 asks a consumer to prefer.
    ///
    /// This bounds the frontier operand alone. Individual receipt ranges are
    /// left as recorded: they carry their own confirmation, and no consumer has
    /// asked for them to be clipped. It also does not detect the rewrite — a
    /// transcript that shrank and then regrew past the old frontier presents the
    /// same length as one that never shrank.
    pub(in crate::services::discord) fn with_frontier_clamped_to_eof(
        mut self,
        transcript_len: u64,
    ) -> Self {
        if let Some(frontier) = self.frontier.as_mut() {
            frontier.end = frontier.end.min(transcript_len);
        }
        self
    }

    /// Pure adapter from the durable record shape into the projection.
    ///
    /// Both durable operands are read here. `confirmed_deliveries` is the newer
    /// field and `#[serde(default)]`, so a rewrite by a binary that predates it
    /// can leave the vector empty while `delivered_frontier` (#3610 and older)
    /// survives; the record then parses cleanly and the receipt half is silent.
    /// `delivery_record::append_confirmed_receipt` also caps the vector at
    /// `CONFIRMED_DELIVERY_RECEIPT_LIMIT` and drains the oldest entries first,
    /// so for evicted receipts the frontier is the surviving witness. Reading
    /// only `confirmed_deliveries` turns both cases into a false uncovered
    /// answer.
    fn from_record(record: &DeliveryRecord) -> Result<Self, ReceiptIndexUnknownReason> {
        let mut receipt_ranges: HashMap<ReceiptProjectionKey, Vec<(u64, u64)>> = HashMap::new();
        for receipt in &record.confirmed_deliveries {
            let (key, range) = project_receipt(receipt)
                .ok_or(ReceiptIndexUnknownReason::ReceiptStoreUnreadable)?;
            receipt_ranges.entry(key).or_default().push(range);
        }
        for ranges in receipt_ranges.values_mut() {
            sort_and_merge(ranges);
        }

        Ok(Self {
            receipt_ranges,
            // Unconditional, and deliberately not gated on the receipt half having
            // produced rows: the frontier is an INDEPENDENT operand, so such a gate
            // would erase exactly the two cases this function's doc names — a
            // pre-`confirmed_deliveries` binary's rewrite, and the cap eviction —
            // where the frontier is the only surviving witness.
            frontier: record
                .delivered_frontier
                .as_ref()
                .and_then(project_frontier),
        })
    }
}

/// Sort ascending and merge every overlapping OR adjacent pair, so the result is
/// a disjoint ascending cover of the same byte set.
fn sort_and_merge(ranges: &mut Vec<(u64, u64)>) {
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        match merged.last_mut() {
            Some(previous) if range.0 <= previous.1 => previous.1 = previous.1.max(range.1),
            _ => merged.push(range),
        }
    }
    *ranges = merged;
}

/// I/O adapter around `delivery_record`'s canonical read path.
///
/// `read_record_at` merges missing and malformed into `None` on purpose (its I3
/// conservative contract), so `symlink_metadata` performs the one further
/// classification this module needs: a missing path is `Absent`, and a path that
/// exists or cannot be stat'ed is `Unknown`.
pub(in crate::services::discord) fn read_receipt_index_at(path: &Path) -> ReceiptIndexRead {
    if let Some(record) = read_record_at(path) {
        return match ReceiptIndex::from_record(&record) {
            Ok(index) => ReceiptIndexRead::Ready(index),
            Err(reason) => ReceiptIndexRead::Unknown(reason),
        };
    }

    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ReceiptIndexRead::Absent,
        _ => ReceiptIndexRead::Unknown(ReceiptIndexUnknownReason::ReceiptStoreUnreadable),
    }
}

/// Project `delivered_frontier` onto the prefix `[0, range.1)`.
///
/// The END is the operand, not the stored `range` pair: the field is documented
/// as the durable mirror of `confirmed_end_offset`, and its production consumers
/// read `range.1` alone — `delivery_record::range_already_committed` treats every
/// range ending at or below that value as already delivered, and
/// `delivered_frontier_end_current_generation` fuses it by `max` with the
/// in-memory committed offset. So the coordinate system is transcript byte
/// offsets, the same one `ExactJsonlSourceIdentity::range` uses (the #4188 guard
/// bound-checks `range.1` against the transcript byte length). Counting only
/// `[range.0, range.1)` would understate the field and report bytes below
/// `range.0` as uncovered even though the production dedup gate already treats
/// them as delivered.
///
/// A zero generation or a zero end is fail-closed to no coverage: zero is the
/// serde default for both, and the record module's own generation predicate
/// (`durable_frontier_generation_current`) also refuses a zero generation.
fn project_frontier(frontier: &DeliveredCommit) -> Option<FrontierPrefix> {
    if frontier.generation_mtime_ns == 0 || frontier.range.1 == 0 {
        return None;
    }
    Some(FrontierPrefix {
        generation_mtime_ns: frontier.generation_mtime_ns,
        end: frontier.range.1,
    })
}

/// Admit a receipt and place it under its projection key, or reject it.
///
/// The three conjuncts restate `ConfirmedDeliveryReceipt::is_authoritative`,
/// which is private to `delivery_record`; this slice may not widen that module's
/// surface, so the predicate is mirrored instead of called. Keep the two in step
/// if the record module's definition of an authoritative receipt changes.
fn project_receipt(
    receipt: &ConfirmedDeliveryReceipt,
) -> Option<(ReceiptProjectionKey, (u64, u64))> {
    if !receipt.source.is_authoritative()
        || receipt.delivery_channel_id != receipt.source.delivery_channel_id
        || receipt.message_id == 0
    {
        return None;
    }
    Some((
        ReceiptProjectionKey {
            provider: ProviderKind::from_str(&receipt.source.provider)?,
            tmux_session_name: receipt.source.tmux_session_name.clone(),
            generation_mtime_ns: receipt.source.generation_mtime_ns,
        },
        receipt.source.range,
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::services::discord::outbound::delivery_record::ExactJsonlSourceIdentity;

    const GENERATION: i64 = 1_700_491_601;
    const OTHER_GENERATION: i64 = 1_700_491_777;
    const SESSION: &str = "AgentDesk-claude-b3v2";

    fn receipt(range: (u64, u64), generation: i64, turn_nonce: &str) -> ConfirmedDeliveryReceipt {
        receipt_for_session(SESSION, range, generation, turn_nonce)
    }

    fn receipt_for_session(
        tmux_session_name: &str,
        range: (u64, u64),
        generation: i64,
        turn_nonce: &str,
    ) -> ConfirmedDeliveryReceipt {
        ConfirmedDeliveryReceipt {
            source: ExactJsonlSourceIdentity {
                provider: "claude".to_string(),
                tmux_session_name: tmux_session_name.to_string(),
                turn_nonce: turn_nonce.to_string(),
                range,
                generation_mtime_ns: generation,
                offset_authority_channel_id: 41,
                delivery_channel_id: 42,
            },
            delivery_channel_id: 42,
            message_id: 99,
        }
    }

    fn frontier(range: (u64, u64), generation_mtime_ns: i64) -> DeliveredCommit {
        DeliveredCommit {
            range,
            generation_mtime_ns,
            attempts: 1,
            panel_msg_id: None,
            panel_channel_id: None,
        }
    }

    fn record(
        receipts: Vec<ConfirmedDeliveryReceipt>,
        delivered_frontier: Option<DeliveredCommit>,
    ) -> DeliveryRecord {
        DeliveryRecord {
            confirmed_deliveries: receipts,
            delivered_frontier,
            ..DeliveryRecord::default()
        }
    }

    fn index(receipts: Vec<ConfirmedDeliveryReceipt>) -> ReceiptIndex {
        ReceiptIndex::from_record(&record(receipts, None)).expect("authoritative receipt index")
    }

    fn covered(index: &ReceiptIndex, generation: i64, range: (u64, u64)) -> bool {
        index.covers(&ProviderKind::Claude, SESSION, generation, range)
    }

    fn write_record(path: &Path, record: &DeliveryRecord) {
        fs::write(
            path,
            serde_json::to_string(record).expect("serialize record"),
        )
        .expect("write record");
    }

    #[test]
    fn adjacent_receipts_union_covers_the_spanning_obligation() {
        // The P1 red line: two receipts that meet exactly at 15 deliver every
        // byte of [10, 20). A per-receipt containment test calls this
        // uncovered, which is a false uncovered answer.
        let index = index(vec![
            receipt((10, 15), GENERATION, "turn-a"),
            receipt((15, 20), GENERATION, "turn-b"),
        ]);
        assert!(covered(&index, GENERATION, (10, 20)));
        assert!(covered(&index, GENERATION, (12, 18)));
    }

    #[test]
    fn overlapping_receipts_union_covers_the_spanning_obligation() {
        let index = index(vec![
            receipt((0, 200), GENERATION, "turn-a"),
            receipt((100, 300), GENERATION, "turn-b"),
        ]);
        assert!(covered(&index, GENERATION, (0, 300)));
    }

    #[test]
    fn the_three_receipt_coexistence_fixture_shape_unions_into_one_range() {
        // `delivery_record`'s phase-A fixtures pin three receipts coexisting
        // under one generation with these ranges, mixing overlap and
        // containment; the merge has to fold all three.
        let index = index(vec![
            receipt((100, 300), GENERATION, "newer-turn"),
            receipt((0, 200), GENERATION, "older-turn"),
            receipt((50, 300), GENERATION, "delayed-equal-end-turn"),
        ]);
        assert!(covered(&index, GENERATION, (0, 300)));
        assert!(covered(&index, GENERATION, (199, 201)));
        assert!(!covered(&index, GENERATION, (0, 301)));
    }

    #[test]
    fn a_contained_receipt_after_a_wider_one_does_not_shrink_the_merge() {
        // Sorted order puts the wide range first and the fully contained one
        // second; a merge that took the later end instead of the max would
        // silently shrink (0, 300) to (0, 200) and uncover the tail. Review
        // r1 measured exactly that mutation surviving the previous fixtures.
        let index = index(vec![
            receipt((0, 300), GENERATION, "wide-turn"),
            receipt((100, 200), GENERATION, "contained-turn"),
        ]);
        assert!(covered(&index, GENERATION, (250, 300)));
    }

    #[test]
    fn a_one_byte_gap_between_receipts_leaves_the_obligation_uncovered() {
        let index = index(vec![
            receipt((10, 15), GENERATION, "turn-a"),
            receipt((16, 20), GENERATION, "turn-b"),
        ]);
        assert!(!covered(&index, GENERATION, (10, 20)));
        assert!(!covered(&index, GENERATION, (15, 16)));
        assert!(covered(&index, GENERATION, (10, 15)));
        assert!(covered(&index, GENERATION, (16, 20)));
    }

    #[test]
    fn the_frontier_alone_covers_its_prefix_when_the_receipt_list_is_empty() {
        // The second P1: an older binary rewriting the sidecar drops the newer
        // `confirmed_deliveries` field but preserves `delivered_frontier`. The
        // record parses cleanly, so only the frontier operand can answer.
        let index =
            ReceiptIndex::from_record(&record(vec![], Some(frontier((100, 300), GENERATION))))
                .expect("frontier-only index");
        assert!(covered(&index, GENERATION, (0, 300)));
        assert!(covered(&index, GENERATION, (250, 300)));
        // Bytes above the watermark have no evidence behind them.
        assert!(!covered(&index, GENERATION, (300, 400)));
        assert!(!covered(&index, GENERATION, (299, 301)));
    }

    #[test]
    fn the_frontier_completes_what_the_surviving_receipts_leave_uncovered() {
        // The receipt-eviction case: `append_confirmed_receipt` drains the
        // oldest receipts at its cap, so the bytes below the watermark keep the
        // frontier as their only witness while a fresh receipt carries the tail.
        let index = ReceiptIndex::from_record(&record(
            vec![receipt((300, 400), GENERATION, "surviving-turn")],
            Some(frontier((100, 300), GENERATION)),
        ))
        .expect("fused index");
        assert!(covered(&index, GENERATION, (0, 400)));
        assert!(!covered(&index, GENERATION, (0, 401)));
    }

    // #5071 T4-B6 consumer obligation: the frontier operand can outlive the
    // bytes it was stamped against when an in-place rewrite shortens the
    // transcript without rotating its path (#4188). The clamp is opt-in, so
    // this pins both halves — unclamped over-reports, clamped does not.
    #[test]
    fn clamping_the_frontier_to_the_transcript_eof_withdraws_the_bytes_past_it() {
        let stale_high =
            ReceiptIndex::from_record(&record(vec![], Some(frontier((100, 300), GENERATION))))
                .expect("frontier-only index");
        assert!(covered(&stale_high, GENERATION, (250, 300)));

        let clamped = stale_high.clone().with_frontier_clamped_to_eof(200);
        assert!(!covered(&clamped, GENERATION, (250, 300)));
        // Only the bytes above the new EOF are withdrawn; the prefix below it
        // still answers.
        assert!(covered(&clamped, GENERATION, (0, 200)));

        // An EOF at or above the frontier end changes nothing.
        assert_eq!(
            stale_high.clone().with_frontier_clamped_to_eof(300),
            stale_high
        );
        assert_eq!(
            stale_high.clone().with_frontier_clamped_to_eof(u64::MAX),
            stale_high
        );

        // A receipt-only index has no frontier to clamp, and its receipts are
        // left alone.
        let receipts_only = index(vec![receipt((250, 300), GENERATION, "turn-a")]);
        assert_eq!(
            receipts_only.clone().with_frontier_clamped_to_eof(0),
            receipts_only
        );
    }

    #[test]
    fn a_frontier_from_another_generation_contributes_nothing() {
        let index = ReceiptIndex::from_record(&record(
            vec![],
            Some(frontier((100, 300), OTHER_GENERATION)),
        ))
        .expect("stale-generation frontier index");
        assert!(!covered(&index, GENERATION, (0, 300)));
        assert!(covered(&index, OTHER_GENERATION, (0, 300)));
    }

    #[test]
    fn a_default_valued_frontier_contributes_nothing() {
        let zero_generation =
            ReceiptIndex::from_record(&record(vec![], Some(frontier((100, 300), 0))))
                .expect("zero-generation frontier index");
        assert!(!zero_generation.covers(&ProviderKind::Claude, SESSION, 0, (0, 300)));

        let zero_end =
            ReceiptIndex::from_record(&record(vec![], Some(frontier((0, 0), GENERATION))))
                .expect("zero-end frontier index");
        assert!(!covered(&zero_end, GENERATION, (0, 1)));
    }

    #[test]
    fn wrong_generation_never_covers() {
        let index = index(vec![receipt((10, 20), GENERATION, "turn-a")]);
        assert!(covered(&index, GENERATION, (10, 20)));
        assert!(!covered(&index, OTHER_GENERATION, (10, 20)));
    }

    #[test]
    fn turn_nonce_is_not_part_of_the_projection() {
        // Distinct nonces, same incarnation: the ranges have to union anyway.
        let index = index(vec![
            receipt((10, 15), GENERATION, "turn-a"),
            receipt((15, 20), GENERATION, "turn-zzz-different"),
        ]);
        assert!(covered(&index, GENERATION, (10, 20)));
        assert_eq!(index.receipt_ranges.len(), 1);
    }

    #[test]
    fn receipts_from_different_sessions_do_not_union() {
        let index = index(vec![
            receipt((10, 15), GENERATION, "turn-a"),
            receipt_for_session("AgentDesk-claude-other", (15, 20), GENERATION, "turn-b"),
        ]);
        assert!(!covered(&index, GENERATION, (10, 20)));
        assert!(covered(&index, GENERATION, (10, 15)));
    }

    #[test]
    fn an_empty_or_inverted_obligation_is_not_covered() {
        let index = index(vec![receipt((10, 20), GENERATION, "turn-a")]);
        assert!(!covered(&index, GENERATION, (12, 12)));
        assert!(!covered(&index, GENERATION, (18, 12)));
    }

    #[test]
    fn a_semantically_incomplete_receipt_makes_the_present_store_unknown() {
        let mut incomplete = receipt((10, 20), GENERATION, "turn-a");
        incomplete.source.turn_nonce.clear();
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("record.json");
        write_record(&path, &record(vec![incomplete], None));

        assert_eq!(
            read_receipt_index_at(&path),
            ReceiptIndexRead::Unknown(ReceiptIndexUnknownReason::ReceiptStoreUnreadable)
        );
    }

    #[test]
    fn a_receipt_without_a_discord_message_id_makes_the_present_store_unknown() {
        // `ExactJsonlSourceIdentity::is_authoritative` does not look at the
        // receipt's own `message_id`, so the missing-proof shape has to be
        // rejected by `project_receipt`'s own conjunct rather than inherited.
        let mut no_message = receipt((10, 20), GENERATION, "turn-a");
        no_message.message_id = 0;
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("record.json");
        write_record(&path, &record(vec![no_message], None));

        assert_eq!(
            read_receipt_index_at(&path),
            ReceiptIndexRead::Unknown(ReceiptIndexUnknownReason::ReceiptStoreUnreadable)
        );

        // The third conjunct: the receipt's channel must agree with the channel
        // recorded inside its own source identity.
        let mut disagreeing_channel = receipt((10, 20), GENERATION, "turn-a");
        disagreeing_channel.delivery_channel_id = 43;
        let disagreeing_path = dir.path().join("disagreeing.json");
        write_record(&disagreeing_path, &record(vec![disagreeing_channel], None));

        assert_eq!(
            read_receipt_index_at(&disagreeing_path),
            ReceiptIndexRead::Unknown(ReceiptIndexUnknownReason::ReceiptStoreUnreadable)
        );
    }

    #[test]
    fn a_malformed_present_store_is_unknown() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("record.json");
        fs::write(&path, "{ this is not a delivery record").expect("write garbage");

        assert_eq!(
            read_receipt_index_at(&path),
            ReceiptIndexRead::Unknown(ReceiptIndexUnknownReason::ReceiptStoreUnreadable)
        );
    }

    #[test]
    fn empty_and_absent_stores_remain_distinct() {
        let dir = tempdir().expect("tempdir");
        let absent = dir.path().join("never-written.json");
        assert_eq!(read_receipt_index_at(&absent), ReceiptIndexRead::Absent);

        let empty = dir.path().join("record.json");
        write_record(&empty, &DeliveryRecord::default());
        assert_eq!(
            read_receipt_index_at(&empty),
            ReceiptIndexRead::Ready(ReceiptIndex::default())
        );
    }

    #[test]
    fn a_readable_store_projects_both_durable_operands() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("record.json");
        write_record(
            &path,
            &record(
                vec![receipt((300, 400), GENERATION, "turn-a")],
                Some(frontier((100, 300), GENERATION)),
            ),
        );

        let ReceiptIndexRead::Ready(index) = read_receipt_index_at(&path) else {
            panic!("a readable record must project");
        };
        assert!(covered(&index, GENERATION, (0, 400)));
    }
}
