//! #3038 decision_route decomposition: review-decision dispatch repository —
//! by-id lookup, pending resolution, finalized-proof loading, and the
//! consume / mark-complete / claim-resume CAS state machine. Function bodies
//! are verbatim moves from the former `decision_route.rs` monolith.

use serde_json::json;

use crate::app_state::AppState;

/// #2200 sub-fix 4 (`stale-dispatch-mismatch`):
/// Outcome of a by-id review-decision dispatch lookup. Used when the caller
/// submits an explicit `dispatch_id` and the canonical "pending" lookup
/// (linked via `card_review_state.pending_dispatch_id` /
/// `kanban_cards.latest_dispatch_id`) misses it because those link rows were
/// cleared while the dispatch row itself is still `dispatched`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReviewDecisionDispatchLookup {
    /// Row exists for (dispatch_id, card_id) with `dispatch_type =
    /// 'review-decision'`, a live status (`pending` / `dispatched`), and is
    /// the most-recent live review-decision dispatch for the card (no newer
    /// live row exists). Safe to treat as the originating dispatch.
    LiveAndCurrent,
    /// Row exists for (dispatch_id, card_id) with `dispatch_type =
    /// 'review-decision'` and a live status, BUT a newer live
    /// review-decision dispatch exists for the same card. Treating the
    /// submitted (older) id as authoritative would consume the wrong
    /// dispatch — reject as stale.
    LiveButSuperseded,
    /// Row exists for (dispatch_id, card_id) with `dispatch_type =
    /// 'review-decision'` but is in a terminal status
    /// (`completed`/`failed`/`cancelled`). This is the proven-finalized
    /// territory that PR #2280 (sub-fix 1) handles via dispatch-result
    /// proof; sub-fix 4 deliberately does NOT short-circuit here. Caller
    /// falls through to the canonical "no pending" 409 (or sub-fix 1's
    /// finalize path once merged).
    Terminal,
    /// No row matches (dispatch_id, card_id) for `dispatch_type =
    /// 'review-decision'`. Either the id is unknown, points at a different
    /// card, or refers to a non-review-decision dispatch. Treated as 404 by
    /// the caller (no cross-card / cross-type confusion).
    NotFound,
}

/// Look up a `review-decision` dispatch by id, restricted to the given card.
///
/// This intentionally bypasses the `card_review_state.pending_dispatch_id` /
/// `kanban_cards.latest_dispatch_id` link tables that
/// [`pending_review_decision_dispatch_id_pg_first`] joins through, because the
/// `stale-dispatch-mismatch` symptom (#2200 sub-fix 4) is exactly that those
/// link rows got cleared (e.g. by a follow-up dispatch) while the originating
/// `review-decision` dispatch row remains alive (`status = 'dispatched'`).
///
/// Authorization layering:
/// 1. `kanban_card_id` must equal the body's `card_id` and `dispatch_type`
///    must be `'review-decision'` — blocks cross-card / cross-type binding
///    via UUID guessing.
/// 2. For a live (`pending`/`dispatched`) row we additionally require that
///    no other live `review-decision` dispatch exists for the same card
///    with a `created_at >= submitted.created_at`. This blocks the "replay
///    an older live id from a previous review round" attack — only the
///    strict-latest live row is honored, with equal-timestamp ties failing
///    closed (Codex round-2 [medium]).
/// 3. Terminal rows are returned as `Terminal` and intentionally NOT
///    short-circuited into a 409 by this routine. Idempotent-finalize of
///    terminal dispatches is sub-fix 1's responsibility (PR #2280), and
///    composing the two without that branch present here would either
///    regress sub-1's 200 path or leak `dispatch_status` on what sub-1
///    keeps as a generic conflict.
pub(super) async fn lookup_review_decision_dispatch_by_id(
    state: &AppState,
    card_id: &str,
    dispatch_id: &str,
) -> ReviewDecisionDispatchLookup {
    if let Some(pool) = state.pg_pool_ref() {
        let row = match sqlx::query_as::<_, (String, chrono::DateTime<chrono::Utc>)>(
            "SELECT status, created_at
             FROM task_dispatches
             WHERE id = $1
               AND kanban_card_id = $2
               AND dispatch_type = 'review-decision'",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(row) => row,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    dispatch_id,
                    %error,
                    "[review-decision] failed to load postgres review-decision dispatch by id"
                );
                return ReviewDecisionDispatchLookup::NotFound;
            }
        };
        let Some((status, created_at)) = row else {
            return ReviewDecisionDispatchLookup::NotFound;
        };
        match status.as_str() {
            "pending" | "dispatched" => {
                // Authorization gate (layer 2): reject if a newer live
                // review-decision dispatch exists for the same card.
                // Codex round-2 [medium]: reject equal-timestamp ties too —
                // require strict uniqueness of "latest live" by treating any
                // other live row with `created_at >= submitted.created_at`
                // as a superseding row. Equal-timestamp duplicates are
                // plausible under the schema-drift / migration-gap state
                // this runtime guard defends against (the canonical
                // partial-unique index blocks them in production but a
                // defensive layer must not assume that).
                match sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*)
                     FROM task_dispatches
                     WHERE kanban_card_id = $1
                       AND dispatch_type = 'review-decision'
                       AND status IN ('pending', 'dispatched')
                       AND id <> $2
                       AND created_at >= $3",
                )
                .bind(card_id)
                .bind(dispatch_id)
                .bind(created_at)
                .fetch_one(pool)
                .await
                {
                    Ok(0) => ReviewDecisionDispatchLookup::LiveAndCurrent,
                    Ok(_) => ReviewDecisionDispatchLookup::LiveButSuperseded,
                    Err(error) => {
                        tracing::warn!(
                            card_id,
                            dispatch_id,
                            %error,
                            "[review-decision] failed to count newer live review-decision dispatches"
                        );
                        ReviewDecisionDispatchLookup::LiveButSuperseded
                    }
                }
            }
            _ => ReviewDecisionDispatchLookup::Terminal,
        }
    } else {
        ReviewDecisionDispatchLookup::NotFound
    }
}

pub(super) async fn pending_review_decision_dispatch_id_pg_first(
    state: &AppState,
    card_id: &str,
) -> Option<String> {
    if let Some(pool) = state.pg_pool_ref() {
        match sqlx::query_scalar::<_, String>(
            "SELECT td.id
             FROM task_dispatches td
             JOIN card_review_state crs ON crs.pending_dispatch_id = td.id
             WHERE crs.card_id = $1
               AND td.dispatch_type = 'review-decision'
               AND td.status IN ('pending', 'dispatched')",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(dispatch_id)) => return Some(dispatch_id),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres pending review-decision by review state"
                );
                return None;
            }
        }

        return match sqlx::query_scalar::<_, String>(
            "SELECT td.id
             FROM task_dispatches td
             JOIN kanban_cards kc ON kc.latest_dispatch_id = td.id
             WHERE kc.id = $1
               AND td.dispatch_type = 'review-decision'
               AND td.status IN ('pending', 'dispatched')",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(dispatch_id) => dispatch_id,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres pending review-decision by latest dispatch"
                );
                None
            }
        };
    }

    None
}

/// #2200 sub-fix 1 (`stale-state`): Snapshot of the most recent originating
/// review-decision dispatch for a card together with the canonical
/// `card_review_state.last_decision`. Used to decide whether a late
/// `/api/review-decision` call should idempotently short-circuit because the
/// originating dispatch was already consumed by a follow-up (rework/review) or
/// by the auto-accept policy.
#[derive(Debug, Clone, Default)]
pub(super) struct FinalizedReviewDecisionInfo {
    /// Most recent review-decision dispatch row for this card, regardless of
    /// status. `None` means there is no originating dispatch at all.
    pub(super) latest_dispatch_id: Option<String>,
    /// Status of `latest_dispatch_id` (e.g. completed/cancelled/failed). When
    /// the dispatch is still pending/dispatched the caller should NOT use this
    /// helper — `pending_review_decision_dispatch_id_pg_first` handles the
    /// live case.
    pub(super) latest_dispatch_status: Option<String>,
    /// `result` JSON column of the latest review-decision dispatch row. Parsed
    /// lazily to discover the recorded decision and finalization source.
    pub(super) latest_dispatch_result: Option<String>,
    /// Updated timestamp of `latest_dispatch_id`. Used to avoid immediately
    /// double-running side effects while a first request is still active.
    pub(super) latest_dispatch_updated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `card_review_state.last_decision` — typically one of
    /// `accept` / `dispute` / `dismiss` / `auto_accept`.
    pub(super) last_decision: Option<String>,
    /// `card_review_state.state` — e.g. `rework_pending`, `reviewing`, `idle`.
    pub(super) review_state: Option<String>,
}

impl FinalizedReviewDecisionInfo {
    pub(super) fn has_originating_dispatch(&self) -> bool {
        self.latest_dispatch_id.is_some()
    }

    fn parsed_result(&self) -> Option<serde_json::Value> {
        self.latest_dispatch_result
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
    }

    /// `completion_source` from the dispatch `result`, if any. Used to gate
    /// the idempotent-finalize path to dispatch rows that were finalized by a
    /// trusted route-owned path. An attacker (or another finalizer) writing
    /// `{"decision":"accept"}` into `result` is NOT sufficient on its own —
    /// the row must also carry a recognized `completion_source`.
    fn completion_source(&self) -> Option<String> {
        self.parsed_result()?
            .get("completion_source")
            .and_then(|value| value.as_str())
            .map(str::to_string)
    }

    /// Cancellation reason from the dispatch `result`, if any.
    fn cancellation_reason(&self) -> Option<String> {
        self.parsed_result()?
            .get("reason")
            .and_then(|value| value.as_str())
            .map(str::to_string)
    }

    pub(super) fn pending_side_effects_decision(&self) -> Option<&'static str> {
        if self.latest_dispatch_status.as_deref() != Some("completed") {
            return None;
        }
        if self.completion_source().as_deref() != Some("review_decision_api_in_progress") {
            return None;
        }
        let result = self.parsed_result()?;
        if !matches!(
            result.get("completion_state").and_then(|v| v.as_str()),
            Some("side_effects_pending" | "side_effects_resuming")
        ) {
            return None;
        }
        match result.get("decision").and_then(|v| v.as_str())? {
            "accept" => Some("accept"),
            "dispute" => Some("dispute"),
            "dismiss" => Some("dismiss"),
            _ => None,
        }
    }

    pub(super) fn side_effects_resume_is_stale_enough(&self) -> bool {
        const RESUME_AFTER_SECONDS: i64 = 30;
        self.latest_dispatch_updated_at
            .map(|updated_at| {
                chrono::Utc::now()
                    .signed_duration_since(updated_at)
                    .num_seconds()
                    >= RESUME_AFTER_SECONDS
            })
            .unwrap_or(true)
    }

    /// Returns the canonical decision that the originating review-decision
    /// dispatch was finalized with, if and only if we can prove it from the
    /// dispatch row's own `status`+`result` AND a recognized
    /// `completion_source`. Dispatch-scoped: we never derive the decision
    /// from unscoped `card_review_state.last_decision` (which can be stale
    /// from a prior round).
    ///
    /// Recognized proofs:
    /// - status=completed, completion_source=review_decision_api, result.decision is one of accept/dispute/dismiss
    /// - status=completed, completion_source=review_auto_accept_policy, result.decision=auto_accept (maps to accept)
    /// - status=cancelled, completion_source=force_transition, result.reason=auto_cancelled_on_terminal_card
    ///   AND result.decision present (auto-cleanup path that records the consumed decision)
    pub(super) fn proven_finalized_decision(&self) -> Option<&'static str> {
        let result = self.parsed_result()?;
        if result.get("outcome").and_then(|v| v.as_str()) == Some("scope_mismatch_closed") {
            return None;
        }
        let recorded_decision = result.get("decision").and_then(|v| v.as_str());
        let source = self.completion_source();
        match self.latest_dispatch_status.as_deref() {
            Some("completed") => match source.as_deref() {
                Some("review_decision_api") => match recorded_decision? {
                    "accept" => Some("accept"),
                    "dispute" => Some("dispute"),
                    "dismiss" => Some("dismiss"),
                    _ => None,
                },
                Some("review_auto_accept_policy") => match recorded_decision? {
                    "auto_accept" | "accept" => Some("accept"),
                    _ => None,
                },
                // Any other completion_source (e.g. orphan_recovery, unknown,
                // or absent) does NOT prove a decision — fall through to 409
                // so the caller can investigate.
                _ => None,
            },
            Some("cancelled") => {
                // Cleanup-cancelled rows: we require BOTH the cleanup-source
                // marker AND a recorded decision on this dispatch row. We do
                // NOT consult card-level `last_decision` because that field
                // is not dispatch-scoped — a fresh dispatch could be cancelled
                // by terminal cleanup while an older `last_decision` remains
                // from a previous round.
                let reason = self.cancellation_reason();
                let is_cleanup = matches!(
                    source.as_deref(),
                    Some("force_transition") | Some("js_terminal_cleanup")
                ) && matches!(
                    reason.as_deref(),
                    Some("auto_cancelled_on_terminal_card") | Some("js_terminal_cleanup")
                );
                if !is_cleanup {
                    return None;
                }
                match recorded_decision? {
                    "accept" | "auto_accept" => Some("accept"),
                    "dispute" => Some("dispute"),
                    "dismiss" => Some("dismiss"),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// #2200 sub-fix 1 (`stale-state`): load the most recent review-decision
/// dispatch for the card and the canonical `card_review_state` snapshot. This
/// is intentionally narrow — it does NOT pick up unrelated dispatch types and
/// does NOT mutate state.
pub(super) async fn finalized_review_decision_info_pg_first(
    state: &AppState,
    card_id: &str,
) -> FinalizedReviewDecisionInfo {
    let mut info = FinalizedReviewDecisionInfo::default();

    if let Some(pool) = state.pg_pool_ref() {
        match sqlx::query_as::<
            _,
            (
                String,
                String,
                Option<String>,
                chrono::DateTime<chrono::Utc>,
            ),
        >(
            "SELECT id, status, result, updated_at
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review-decision'
             ORDER BY created_at DESC
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some((id, status, result, updated_at))) => {
                info.latest_dispatch_id = Some(id);
                info.latest_dispatch_status = Some(status);
                info.latest_dispatch_result = result;
                info.latest_dispatch_updated_at = Some(updated_at);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load latest review-decision dispatch for idempotent finalize check"
                );
            }
        }

        match sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT last_decision, state
             FROM card_review_state
             WHERE card_id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some((last_decision, review_state))) => {
                info.last_decision = last_decision;
                info.review_state = review_state;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load card_review_state for idempotent finalize check"
                );
            }
        }

        return info;
    }

    info
}

pub(super) async fn mark_next_review_round_advance_pg_first(
    state: &AppState,
    card_id: &str,
) -> Result<bool, String> {
    let pool = state
        .pg_pool_ref()
        .ok_or_else(|| "postgres pool unavailable for review round advance".to_string())?;
    let rows = sqlx::query(
        "UPDATE kanban_cards
         SET metadata = COALESCE(metadata, '{}'::jsonb) || jsonb_build_object($1::text, true),
             updated_at = NOW()
         WHERE id = $2
           AND COALESCE((metadata ->> $1)::boolean, false) = false",
    )
    .bind(crate::engine::ops::ADVANCE_REVIEW_ROUND_HINT_KEY)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("mark postgres review round advance for {card_id}: {error}"))?
    .rows_affected();
    Ok(rows > 0)
}

pub(super) async fn dispatch_status_and_result_pg_first(
    state: &AppState,
    dispatch_id: &str,
) -> Option<(String, Option<String>)> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT status, result FROM task_dispatches WHERE id = $1",
        )
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await
        {
            Ok(row) => row,
            Err(error) => {
                tracing::warn!(
                    dispatch_id,
                    %error,
                    "[review-decision] failed to load postgres dispatch status"
                );
                None
            }
        };
    }

    None
}

pub(super) async fn consume_review_decision_dispatch_pg_first(
    state: &AppState,
    card_id: &str,
    dispatch_id: &str,
    decision: &str,
) -> Result<bool, String> {
    let Some(pool) = state.pg_pool_ref() else {
        return Ok(false);
    };

    let matches_requested_dispatch = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1
             FROM task_dispatches
             WHERE id = $1
               AND kanban_card_id = $2
               AND dispatch_type = 'review-decision'
               AND status IN ('pending', 'dispatched')
         )",
    )
    .bind(dispatch_id)
    .bind(card_id)
    .fetch_one(pool)
    .await
    .map_err(|error| {
        format!("check postgres review-decision dispatch before consume {dispatch_id}: {error}")
    })?;
    if !matches_requested_dispatch {
        return Ok(false);
    }

    let result = json!({
        "decision": decision,
        "completion_source": "review_decision_api_in_progress",
        "completion_state": "side_effects_pending",
    });
    let rows = crate::dispatch::set_dispatch_status_on_pg_async(
        pool,
        dispatch_id,
        "completed",
        Some(&result),
        "mark_dispatch_completed",
        Some(&["pending", "dispatched"]),
        true,
    )
    .await
    .map_err(|error| format!("consume postgres review-decision dispatch {dispatch_id}: {error}"))?;

    Ok(rows == 1)
}

pub(super) async fn mark_review_decision_side_effects_complete_pg_first(
    state: &AppState,
    card_id: &str,
    dispatch_id: &str,
    decision: &str,
    expected_completion_state: &str,
) -> Result<bool, String> {
    let Some(pool) = state.pg_pool_ref() else {
        return Ok(false);
    };

    let result = json!({
        "decision": decision,
        "completion_source": "review_decision_api",
        "completion_state": "side_effects_complete",
    });
    let rows = sqlx::query(
        "UPDATE task_dispatches
         SET result = $1,
             updated_at = NOW()
         WHERE id = $2
           AND kanban_card_id = $3
	           AND dispatch_type = 'review-decision'
	           AND status = 'completed'
	           AND (result::jsonb ->> 'decision') = $4
	           AND (result::jsonb ->> 'completion_source') = 'review_decision_api_in_progress'
	           AND (result::jsonb ->> 'completion_state') = $5",
    )
    .bind(result.to_string())
    .bind(dispatch_id)
    .bind(card_id)
    .bind(decision)
    .bind(expected_completion_state)
    .execute(pool)
    .await
    .map_err(|error| {
        format!("mark postgres review-decision side effects complete {dispatch_id}: {error}")
    })?
    .rows_affected();

    Ok(rows == 1)
}

pub(super) async fn claim_review_decision_side_effects_resume_pg_first(
    state: &AppState,
    card_id: &str,
    dispatch_id: &str,
    decision: &str,
) -> Result<bool, String> {
    let Some(pool) = state.pg_pool_ref() else {
        return Ok(true);
    };

    let result = json!({
        "decision": decision,
        "completion_source": "review_decision_api_in_progress",
        "completion_state": "side_effects_resuming",
    });
    let rows = sqlx::query(
        "UPDATE task_dispatches
         SET result = $1,
             updated_at = NOW()
         WHERE id = $2
           AND kanban_card_id = $3
           AND dispatch_type = 'review-decision'
           AND status = 'completed'
           AND (result::jsonb ->> 'decision') = $4
           AND (result::jsonb ->> 'completion_source') = 'review_decision_api_in_progress'
           AND (result::jsonb ->> 'completion_state') = 'side_effects_pending'
           AND updated_at <= NOW() - INTERVAL '30 seconds'",
    )
    .bind(result.to_string())
    .bind(dispatch_id)
    .bind(card_id)
    .bind(decision)
    .execute(pool)
    .await
    .map_err(|error| {
        format!("claim postgres review-decision side-effects resume {dispatch_id}: {error}")
    })?
    .rows_affected();

    Ok(rows == 1)
}

/// #3038 route_srp: named repo lookup extracted verbatim from the #229
/// safety-net block in `decision_route_dispute_re_review`. True when the card
/// has any live (`pending`/`dispatched`) `review` or `review-decision`
/// dispatch; `false` on query failure or when no postgres pool is wired in —
/// identical to the original inline expression.
pub(super) async fn has_pending_reviewish_dispatch_pg_first(
    state: &AppState,
    card_id: &str,
) -> bool {
    if let Some(pool) = state.pg_pool_ref() {
        sqlx::query_scalar::<_, bool>(
            "SELECT COUNT(*) > 0
                         FROM task_dispatches
                         WHERE kanban_card_id = $1
                           AND dispatch_type IN ('review', 'review-decision')
                           AND status IN ('pending', 'dispatched')",
        )
        .bind(card_id)
        .fetch_one(pool)
        .await
        .unwrap_or(false)
    } else {
        false
    }
}
