//! Single source of truth for the "an active dispatch already occupies this
//! slot" SQL predicate (#3040).
//!
//! This predicate previously lived, character-for-character, in at least five
//! places (`claim.rs` NOT EXISTS / EXISTS builders, `slots.rs` inline COUNT,
//! and a hand-ported Rust loop in `services/auto_queue/runtime.rs`). Each copy
//! had to be edited in lockstep — a slot double-allocation / permanent-occupancy
//! footgun. Everything now routes through [`active_dispatch_on_slot_predicate`]
//! so the semantics can only diverge here.
//!
//! Semantics: a `task_dispatches` row "actively occupies" `slot_expr` for
//! `agent_expr` when ALL of the following hold:
//! - `status IN ('pending', 'dispatched')`
//! - its `context.slot_index` (see [`dispatch_slot_index_expr`]) equals `slot_expr`
//! - `context.sidecar_dispatch` is not `true`
//! - `context.phase_gate` is absent
//! - it is NOT a quiescent review-class dispatch: i.e. either it is not a
//!   review / review-decision / create-pr dispatch, OR it is still `pending`,
//!   OR a live (non-terminal) session is still attached to it.

/// EXISTS vs NOT EXISTS wrapping for the slot-occupancy predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DispatchSlotPolarity {
    /// `EXISTS (...)` — true when an active dispatch occupies the slot.
    Exists,
    /// `NOT EXISTS (...)` — true when the slot is free of active dispatches.
    NotExists,
}

impl DispatchSlotPolarity {
    fn keyword(self) -> &'static str {
        match self {
            DispatchSlotPolarity::Exists => "EXISTS",
            DispatchSlotPolarity::NotExists => "NOT EXISTS",
        }
    }
}

/// SQL expression that extracts a dispatch row's `context.slot_index` as a
/// `BIGINT`, defaulting to `-1` when the context is empty / null / missing the
/// key. `column` is the (optionally qualified) `context` column reference,
/// e.g. `"d.context"` for an aliased table or `"context"` when unqualified.
pub(crate) fn dispatch_slot_index_expr(column: &str) -> String {
    format!(
        "COALESCE(NULLIF((COALESCE(NULLIF({column}, ''), '{{}}')::jsonb)->>'slot_index', '')::BIGINT, -1)"
    )
}

/// SQL boolean expression that extracts `context.<key>` as a BOOLEAN, defaulting
/// to `FALSE`.
fn dispatch_bool_flag_expr(column: &str, key: &str) -> String {
    format!("COALESCE(((COALESCE(NULLIF({column}, ''), '{{}}')::jsonb)->>'{key}')::BOOLEAN, FALSE)")
}

/// SQL expression accessing `context.<key>` as raw JSONB (for `IS NULL` tests).
fn dispatch_json_member_expr(column: &str, key: &str) -> String {
    format!("(COALESCE(NULLIF({column}, ''), '{{}}')::jsonb)->'{key}'")
}

/// Builds the `EXISTS (...)` / `NOT EXISTS (...)` predicate that tests whether
/// an active dispatch occupies `slot_expr` for `agent_expr`.
///
/// `agent_expr` and `slot_expr` are inlined SQL expressions (column references
/// such as `"s.agent_id"` / `"s.slot_index"`, or bind placeholders such as
/// `"$1"` / `"$2"`). `extra_clause`, when `Some`, is appended verbatim as an
/// additional `AND (...)` filter inside the subquery (used to exclude a
/// specific dispatch id, e.g. `"d.id != $3"`).
pub(crate) fn active_dispatch_on_slot_predicate(
    agent_expr: &str,
    slot_expr: &str,
    polarity: DispatchSlotPolarity,
    extra_clause: Option<&str>,
) -> String {
    let slot_index = dispatch_slot_index_expr("d.context");
    let sidecar = dispatch_bool_flag_expr("d.context", "sidecar_dispatch");
    let phase_gate = dispatch_json_member_expr("d.context", "phase_gate");
    let extra = match extra_clause {
        Some(clause) => format!("\n               AND ({clause})"),
        None => String::new(),
    };
    format!(
        "{keyword} (
             SELECT 1
             FROM task_dispatches d
             WHERE d.to_agent_id = {agent_expr}
               AND d.status IN ('pending', 'dispatched')
               AND {slot_index} = {slot_expr}
               AND {sidecar} = FALSE
               AND {phase_gate} IS NULL{extra}
               AND (
                   COALESCE(d.dispatch_type, 'implementation') NOT IN ('review', 'review-decision', 'create-pr')
                   OR d.status = 'pending'
                   OR EXISTS (
                       SELECT 1
                       FROM sessions s
                       WHERE s.active_dispatch_id = d.id
                         AND COALESCE(s.status, '') NOT IN ('disconnected', 'completed', 'failed', 'cancelled')
                   )
               )
         )",
        keyword = polarity.keyword(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The original, hand-written predicate body that lived in
    /// `claim.rs::active_dispatch_slot_guard_sql` /
    /// `active_dispatch_slot_exists_sql` before #3040. Kept verbatim here as the
    /// golden expectation so the unified builder can never silently drift from
    /// the semantics every call site relied on.
    fn legacy_claim_predicate(keyword: &str, agent_expr: &str, slot_expr: &str) -> String {
        format!(
            "{keyword} (
             SELECT 1
             FROM task_dispatches d
             WHERE d.to_agent_id = {agent_expr}
               AND d.status IN ('pending', 'dispatched')
               AND COALESCE(NULLIF((COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->>'slot_index', '')::BIGINT, -1) = {slot_expr}
               AND COALESCE(((COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->>'sidecar_dispatch')::BOOLEAN, FALSE) = FALSE
               AND (COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->'phase_gate' IS NULL
               AND (
                   COALESCE(d.dispatch_type, 'implementation') NOT IN ('review', 'review-decision', 'create-pr')
                   OR d.status = 'pending'
                   OR EXISTS (
                       SELECT 1
                       FROM sessions s
                       WHERE s.active_dispatch_id = d.id
                         AND COALESCE(s.status, '') NOT IN ('disconnected', 'completed', 'failed', 'cancelled')
                   )
               )
         )"
        )
    }

    #[test]
    fn polarity_keyword_matches_expected() {
        assert_eq!(DispatchSlotPolarity::Exists.keyword(), "EXISTS");
        assert_eq!(DispatchSlotPolarity::NotExists.keyword(), "NOT EXISTS");
    }

    #[test]
    fn not_exists_form_matches_legacy_claim_guard() {
        // claim.rs `active_dispatch_slot_guard_sql` call sites use column refs.
        for (agent_expr, slot_expr) in [
            ("s.agent_id", "s.slot_index"),
            ("auto_queue_slots.agent_id", "auto_queue_slots.slot_index"),
        ] {
            let generated = active_dispatch_on_slot_predicate(
                agent_expr,
                slot_expr,
                DispatchSlotPolarity::NotExists,
                None,
            );
            let expected = legacy_claim_predicate("NOT EXISTS", agent_expr, slot_expr);
            assert_eq!(generated, expected, "NOT EXISTS drift for {agent_expr}");
        }
    }

    #[test]
    fn exists_form_matches_legacy_claim_exists() {
        // claim.rs `active_dispatch_slot_exists_sql` free-slot-fallback call site.
        let generated = active_dispatch_on_slot_predicate(
            "auto_queue_slots.agent_id",
            "auto_queue_slots.slot_index",
            DispatchSlotPolarity::Exists,
            None,
        );
        let expected = legacy_claim_predicate(
            "EXISTS",
            "auto_queue_slots.agent_id",
            "auto_queue_slots.slot_index",
        );
        assert_eq!(generated, expected);
    }

    #[test]
    fn polarity_only_changes_the_leading_keyword() {
        // The two polarities must differ ONLY by the EXISTS / NOT EXISTS prefix:
        // stripping the keyword leaves byte-identical bodies. This is the exact
        // invariant whose manual upkeep #3040 removes.
        let exists = active_dispatch_on_slot_predicate(
            "s.agent_id",
            "s.slot_index",
            DispatchSlotPolarity::Exists,
            None,
        );
        let not_exists = active_dispatch_on_slot_predicate(
            "s.agent_id",
            "s.slot_index",
            DispatchSlotPolarity::NotExists,
            None,
        );
        let exists_body = exists.strip_prefix("EXISTS").expect("EXISTS prefix");
        let not_exists_body = not_exists
            .strip_prefix("NOT EXISTS")
            .expect("NOT EXISTS prefix");
        assert_eq!(exists_body, not_exists_body);
    }

    #[test]
    fn exists_form_with_bind_params_matches_slots_and_runtime_inline() {
        // slots.rs `slot_has_active_dispatch_excluding_pg` and
        // runtime.rs `slot_has_active_dispatch_excluding_pg` both probe
        // task_dispatches with bind placeholders and an exclude-id clause. They
        // must produce exactly the same predicate the column-ref call sites do,
        // up to the agent/slot expressions and the appended exclude clause.
        let generated = active_dispatch_on_slot_predicate(
            "$1",
            "$2",
            DispatchSlotPolarity::Exists,
            Some("d.id != $3"),
        );
        let expected = legacy_claim_predicate("EXISTS", "$1", "$2")
            .replace(
                "AND (COALESCE(NULLIF(d.context, ''), '{}')::jsonb)->'phase_gate' IS NULL\n",
                "AND (COALESCE(NULLIF(d.context, ''), '{}')::jsonb)->'phase_gate' IS NULL\n               AND (d.id != $3)\n",
            );
        assert_eq!(generated, expected);
        // Exclude clause is injected before the review-class guard, never after.
        let phase_gate_pos = generated.find("phase_gate' IS NULL").unwrap();
        let exclude_pos = generated.find("d.id != $3").unwrap();
        let review_pos = generated.find("NOT IN ('review'").unwrap();
        assert!(phase_gate_pos < exclude_pos && exclude_pos < review_pos);
    }

    #[test]
    fn no_extra_clause_omits_exclude_filter() {
        let generated =
            active_dispatch_on_slot_predicate("$1", "$2", DispatchSlotPolarity::Exists, None);
        assert!(!generated.contains("d.id != "));
    }

    #[test]
    fn slot_index_expr_matches_legacy_extraction() {
        // The shared JSONB slot_index extraction (5x duplicated pre-#3040) must
        // be byte-identical for both the qualified and unqualified context refs.
        assert_eq!(
            dispatch_slot_index_expr("d.context"),
            "COALESCE(NULLIF((COALESCE(NULLIF(d.context, ''), '{}')::jsonb)->>'slot_index', '')::BIGINT, -1)"
        );
        assert_eq!(
            dispatch_slot_index_expr("context"),
            "COALESCE(NULLIF((COALESCE(NULLIF(context, ''), '{}')::jsonb)->>'slot_index', '')::BIGINT, -1)"
        );
    }
}
