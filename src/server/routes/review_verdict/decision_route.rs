use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use sqlite_test::OptionalExtension;

use super::super::AppState;
use super::review_state_repo::update_card_review_state;
use super::tuning_aggregate::{record_decision_tuning, spawn_aggregate_if_needed_with_pg};

/// PG-only wrapper for kanban transitions after #1384.
#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
async fn transition_status_pg_first(
    state: &AppState,
    card_id: &str,
    new_status: &str,
    source: &str,
    force_intent: crate::engine::transition::ForceIntent,
) -> anyhow::Result<crate::kanban::TransitionResult> {
    let pool = state.pg_pool_ref().ok_or_else(|| {
        anyhow::anyhow!("postgres backend required for kanban transition (#1384)")
    })?;
    crate::kanban::transition_status_with_opts_pg_only(
        pool,
        &state.engine,
        card_id,
        new_status,
        source,
        force_intent,
    )
    .await
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
async fn transition_status_pg_first(
    state: &AppState,
    card_id: &str,
    new_status: &str,
    _source: &str,
    _force_intent: crate::engine::transition::ForceIntent,
) -> anyhow::Result<crate::kanban::TransitionResult> {
    if let Some(pool) = state.pg_pool_ref() {
        return crate::kanban::transition_status_with_opts_pg_only(
            pool,
            &state.engine,
            card_id,
            new_status,
            _source,
            _force_intent,
        )
        .await;
    }
    let db = state
        .legacy_db()
        .ok_or_else(|| anyhow::anyhow!("sqlite test backend unavailable for kanban transition"))?;
    let conn = db
        .separate_conn()
        .map_err(|error| anyhow::anyhow!("open sqlite transition connection: {error}"))?;
    let old_status: String = conn.query_row(
        "SELECT status FROM kanban_cards WHERE id = ?1",
        [card_id],
        |row| row.get(0),
    )?;
    let changed = old_status != new_status;
    if changed {
        conn.execute(
            "UPDATE kanban_cards SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
            sqlite_test::params![new_status, card_id],
        )?;
        crate::kanban::fire_transition_hooks_with_backends(
            Some(db),
            None,
            &state.engine,
            card_id,
            &old_status,
            new_status,
        );
    }
    Ok(crate::kanban::TransitionResult {
        changed,
        from: old_status,
        to: new_status.to_string(),
    })
}

fn spawn_review_tuning_aggregate_pg_first(state: &AppState) {
    spawn_aggregate_if_needed_with_pg(state.pg_pool_ref().cloned());
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn review_state_db(state: &AppState) -> Option<&crate::db::Db> {
    state.legacy_db()
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
fn review_state_db(_state: &AppState) -> Option<&crate::db::Db> {
    None
}

// ── Review Decision (agent's response to counter-model review) ──────────────

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn test_worktree_commit_override_slot() -> &'static std::sync::Mutex<Option<Option<String>>> {
    static OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<Option<String>>>> =
        std::sync::OnceLock::new();
    OVERRIDE.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn set_test_worktree_commit_override(commit: Option<String>) {
    if let Ok(mut slot) = test_worktree_commit_override_slot().lock() {
        *slot = Some(commit);
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn clear_test_worktree_commit_override() {
    if let Ok(mut slot) = test_worktree_commit_override_slot().lock() {
        *slot = None;
    }
}

async fn current_issue_worktree_commit(
    pg_pool: Option<&sqlx::PgPool>,
    card_id: &str,
    issue_num: i64,
    context: Option<&serde_json::Value>,
) -> Option<String> {
    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    {
        if let Ok(slot) = test_worktree_commit_override_slot().lock() {
            if let Some(override_commit) = slot.clone() {
                return override_commit;
            }
        }
    }

    let Some(pool) = pg_pool else {
        tracing::warn!(
            "[review-decision] current_issue_worktree_commit: card {} issue #{}: postgres pool unavailable",
            card_id,
            issue_num
        );
        return None;
    };

    match crate::dispatch::resolve_card_worktree(pool, card_id, context).await {
        Ok(Some((_worktree_path, _branch, commit))) => Some(commit),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                "[review-decision] current_issue_worktree_commit: card {} issue #{}: {}",
                card_id,
                issue_num,
                err
            );
            None
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct ActiveAcceptFollowups {
    review: i64,
    rework: i64,
    review_decision: i64,
}

impl ActiveAcceptFollowups {
    fn has_followup(self) -> bool {
        self.review > 0 || self.rework > 0
    }
}

async fn active_accept_followups_pg_first(
    state: &AppState,
    card_id: &str,
) -> ActiveAcceptFollowups {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_as::<_, (i64, i64, i64)>(
                "SELECT \
                     COALESCE(SUM(CASE WHEN dispatch_type = 'review' AND status IN ('pending', 'dispatched') THEN 1 ELSE 0 END), 0)::BIGINT, \
                     COALESCE(SUM(CASE WHEN dispatch_type = 'rework' AND status IN ('pending', 'dispatched') THEN 1 ELSE 0 END), 0)::BIGINT, \
                     COALESCE(SUM(CASE WHEN dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched') THEN 1 ELSE 0 END), 0)::BIGINT \
                 FROM task_dispatches \
                 WHERE kanban_card_id = $1",
            )
            .bind(card_id)
            .fetch_one(pool)
            .await
        {
            Ok((review, rework, review_decision)) => ActiveAcceptFollowups {
                review,
                rework,
                review_decision,
            },
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres accept followups"
                );
                ActiveAcceptFollowups::default()
            }
        };
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(db) = state.legacy_db() {
        if let Ok(conn) = db.separate_conn() {
            if let Ok((review, rework, review_decision)) = conn.query_row(
                "SELECT
                     COALESCE(SUM(CASE WHEN dispatch_type = 'review' AND status IN ('pending', 'dispatched') THEN 1 ELSE 0 END), 0),
                     COALESCE(SUM(CASE WHEN dispatch_type = 'rework' AND status IN ('pending', 'dispatched') THEN 1 ELSE 0 END), 0),
                     COALESCE(SUM(CASE WHEN dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched') THEN 1 ELSE 0 END), 0)
                 FROM task_dispatches
                 WHERE kanban_card_id = ?1",
                [card_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
            ) {
                return ActiveAcceptFollowups {
                    review,
                    rework,
                    review_decision,
                };
            }
        }
    }

    ActiveAcceptFollowups::default()
}

async fn current_card_status_pg_first(state: &AppState, card_id: &str) -> Option<String> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_scalar::<_, String>(
            "SELECT status FROM kanban_cards WHERE id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(status) => status,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres card status"
                );
                None
            }
        };
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(db) = state.legacy_db() {
        return db.separate_conn().ok().and_then(|conn| {
            conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = ?1",
                [card_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
        });
    }

    None
}

#[derive(Debug, Default)]
struct ReviewDecisionCardContext {
    status: Option<String>,
    repo_id: Option<String>,
    agent_id: Option<String>,
    title: Option<String>,
}

async fn load_review_decision_card_context_pg_first(
    state: &AppState,
    card_id: &str,
) -> ReviewDecisionCardContext {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_as::<
            _,
            (
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            ),
        >(
            "SELECT status, repo_id, assigned_agent_id, title
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some((status, repo_id, agent_id, title))) => ReviewDecisionCardContext {
                status,
                repo_id,
                agent_id,
                title,
            },
            Ok(None) => ReviewDecisionCardContext::default(),
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres card context"
                );
                ReviewDecisionCardContext::default()
            }
        };
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(db) = state.legacy_db()
        && let Ok(conn) = db.separate_conn()
        && let Ok(Some((status, repo_id, agent_id, title))) = conn
            .query_row(
                "SELECT status, repo_id, assigned_agent_id, title
                 FROM kanban_cards
                 WHERE id = ?1",
                [card_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
    {
        return ReviewDecisionCardContext {
            status,
            repo_id,
            agent_id,
            title,
        };
    }

    ReviewDecisionCardContext::default()
}

async fn resolve_effective_pipeline_pg_first(
    state: &AppState,
    repo_id: Option<&str>,
    agent_id: Option<&str>,
) -> crate::pipeline::PipelineConfig {
    crate::pipeline::ensure_loaded();

    if let Some(pool) = state.pg_pool_ref() {
        return crate::pipeline::resolve_for_card_pg(pool, repo_id, agent_id).await;
    }

    crate::pipeline::resolve(None, None)
}

async fn card_exists_pg_first(state: &AppState, card_id: &str) -> bool {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM kanban_cards WHERE id = $1)",
        )
        .bind(card_id)
        .fetch_one(pool)
        .await
        {
            Ok(exists) => exists,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to check postgres card existence"
                );
                false
            }
        };
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(db) = state.legacy_db() {
        return db
            .separate_conn()
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT 1 FROM kanban_cards WHERE id = ?1",
                    [card_id],
                    |_| Ok(()),
                )
                .ok()
            })
            .is_some();
    }

    false
}

/// #2200 sub-fix 4 (`stale-dispatch-mismatch`):
/// Outcome of a by-id review-decision dispatch lookup. Used when the caller
/// submits an explicit `dispatch_id` and the canonical "pending" lookup
/// (linked via `card_review_state.pending_dispatch_id` /
/// `kanban_cards.latest_dispatch_id`) misses it because those link rows were
/// cleared while the dispatch row itself is still `dispatched`.
#[derive(Debug, PartialEq, Eq)]
enum ReviewDecisionDispatchLookup {
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
async fn lookup_review_decision_dispatch_by_id(
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
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        if let Some(db) = state.legacy_db() {
            if let Ok(conn) = db.separate_conn() {
                let row: Option<(String, String)> = conn
                    .query_row(
                        "SELECT status, created_at
                         FROM task_dispatches
                         WHERE id = ?1
                           AND kanban_card_id = ?2
                           AND dispatch_type = 'review-decision'",
                        [dispatch_id, card_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
                    .ok()
                    .flatten();
                let Some((status, created_at)) = row else {
                    return ReviewDecisionDispatchLookup::NotFound;
                };
                return match status.as_str() {
                    "pending" | "dispatched" => {
                        let newer_count: i64 = conn
                            .query_row(
                                "SELECT COUNT(*)
                                 FROM task_dispatches
                                 WHERE kanban_card_id = ?1
                                   AND dispatch_type = 'review-decision'
                                   AND status IN ('pending', 'dispatched')
                                   AND id <> ?2
                                   AND created_at >= ?3",
                                [card_id, dispatch_id, created_at.as_str()],
                                |row| row.get::<_, i64>(0),
                            )
                            .unwrap_or(1);
                        if newer_count == 0 {
                            ReviewDecisionDispatchLookup::LiveAndCurrent
                        } else {
                            ReviewDecisionDispatchLookup::LiveButSuperseded
                        }
                    }
                    _ => ReviewDecisionDispatchLookup::Terminal,
                };
            }
        }
        ReviewDecisionDispatchLookup::NotFound
    }
}

async fn pending_review_decision_dispatch_id_pg_first(
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

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(db) = state.legacy_db() {
        let conn = db.separate_conn().ok()?;
        if let Ok(Some(dispatch_id)) = conn
            .query_row(
                "SELECT td.id
                 FROM task_dispatches td
                 JOIN card_review_state crs ON crs.pending_dispatch_id = td.id
                 WHERE crs.card_id = ?1
                   AND td.dispatch_type = 'review-decision'
                   AND td.status IN ('pending', 'dispatched')",
                [card_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
        {
            return Some(dispatch_id);
        }
        return conn
            .query_row(
                "SELECT td.id
                 FROM task_dispatches td
                 JOIN kanban_cards kc ON kc.latest_dispatch_id = td.id
                 WHERE kc.id = ?1
                   AND td.dispatch_type = 'review-decision'
                   AND td.status IN ('pending', 'dispatched')",
                [card_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten();
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
struct FinalizedReviewDecisionInfo {
    /// Most recent review-decision dispatch row for this card, regardless of
    /// status. `None` means there is no originating dispatch at all.
    latest_dispatch_id: Option<String>,
    /// Status of `latest_dispatch_id` (e.g. completed/cancelled/failed). When
    /// the dispatch is still pending/dispatched the caller should NOT use this
    /// helper — `pending_review_decision_dispatch_id_pg_first` handles the
    /// live case.
    latest_dispatch_status: Option<String>,
    /// `result` JSON column of the latest review-decision dispatch row. Parsed
    /// lazily to discover the recorded decision and finalization source.
    latest_dispatch_result: Option<String>,
    /// `card_review_state.last_decision` — typically one of
    /// `accept` / `dispute` / `dismiss` / `auto_accept`.
    last_decision: Option<String>,
    /// `card_review_state.state` — e.g. `rework_pending`, `reviewing`, `idle`.
    review_state: Option<String>,
}

impl FinalizedReviewDecisionInfo {
    fn has_originating_dispatch(&self) -> bool {
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
    fn proven_finalized_decision(&self) -> Option<&'static str> {
        let result = self.parsed_result()?;
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
async fn finalized_review_decision_info_pg_first(
    state: &AppState,
    card_id: &str,
) -> FinalizedReviewDecisionInfo {
    let mut info = FinalizedReviewDecisionInfo::default();

    if let Some(pool) = state.pg_pool_ref() {
        match sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT id, status, result
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
            Ok(Some((id, status, result))) => {
                info.latest_dispatch_id = Some(id);
                info.latest_dispatch_status = Some(status);
                info.latest_dispatch_result = result;
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

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(db) = state.legacy_db() {
        if let Ok(conn) = db.separate_conn() {
            if let Ok(row) = conn
                .query_row(
                    "SELECT id, status, result
                     FROM task_dispatches
                     WHERE kanban_card_id = ?1
                       AND dispatch_type = 'review-decision'
                     ORDER BY created_at DESC
                     LIMIT 1",
                    [card_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()
            {
                if let Some((id, status, result)) = row {
                    info.latest_dispatch_id = Some(id);
                    info.latest_dispatch_status = Some(status);
                    info.latest_dispatch_result = result;
                }
            }
            if let Ok(row) = conn
                .query_row(
                    "SELECT last_decision, state
                     FROM card_review_state
                     WHERE card_id = ?1",
                    [card_id],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .optional()
            {
                if let Some((last_decision, review_state)) = row {
                    info.last_decision = last_decision;
                    info.review_state = review_state;
                }
            }
        }
    }

    info
}

async fn emit_card_updated(state: &AppState, card_id: &str) {
    if let Some(pool) = state.pg_pool_ref() {
        match super::super::kanban::load_card_json_pg(pool, card_id).await {
            Ok(Some(card)) => {
                crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card);
                return;
            }
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres card for kanban_card_updated emit"
                );
                return;
            }
        }
    }
}

async fn mark_next_review_round_advance_pg_first(
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

async fn dispatch_status_and_result_pg_first(
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

#[derive(Debug, Clone)]
struct ActiveReviewDispatch {
    id: String,
    reviewed_commit: Option<String>,
    target_repo: Option<String>,
}

fn build_active_review_dispatch(id: String, context_raw: Option<String>) -> ActiveReviewDispatch {
    let context = context_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
    let target_repo = context
        .as_ref()
        .and_then(|value| {
            value
                .get("target_repo")
                .and_then(|entry| entry.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            context
                .as_ref()
                .and_then(|value| value.get("worktree_path"))
                .and_then(|entry| entry.as_str())
                .and_then(|path| {
                    crate::services::platform::shell::resolve_repo_dir_for_target(Some(path))
                        .ok()
                        .flatten()
                })
        });
    ActiveReviewDispatch {
        id,
        reviewed_commit: context.as_ref().and_then(|value| {
            value
                .get("reviewed_commit")
                .and_then(|entry| entry.as_str())
                .map(str::to_string)
        }),
        target_repo,
    }
}

async fn latest_active_review_dispatch_pg_first(
    state: &AppState,
    card_id: &str,
) -> Option<ActiveReviewDispatch> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT id, context
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status IN ('pending', 'dispatched')
             ORDER BY updated_at DESC NULLS LAST, created_at DESC NULLS LAST
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(row) => row.map(|(id, context_raw)| build_active_review_dispatch(id, context_raw)),
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres active review dispatch"
                );
                None
            }
        };
    }

    None
}

async fn latest_completed_review_context_pg_first(
    state: &AppState,
    card_id: &str,
) -> Option<serde_json::Value> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_scalar::<_, Option<String>>(
            "SELECT context
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status = 'completed'
             ORDER BY completed_at DESC NULLS LAST, updated_at DESC NULLS LAST
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(context_raw) => context_raw
                .flatten()
                .and_then(|ctx| serde_json::from_str::<serde_json::Value>(&ctx).ok()),
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres completed review context"
                );
                None
            }
        };
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(db) = state.legacy_db() {
        return db.separate_conn().ok().and_then(|conn| {
            conn.query_row(
                "SELECT context
                 FROM task_dispatches
                 WHERE kanban_card_id = ?1
                   AND dispatch_type = 'review'
                   AND status = 'completed'
                 ORDER BY COALESCE(completed_at, updated_at) DESC, updated_at DESC, rowid DESC
                 LIMIT 1",
                [card_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten()
            .and_then(|ctx| serde_json::from_str::<serde_json::Value>(&ctx).ok())
        });
    }

    None
}

/// #2341 / #2200 sub-3 redesign: snapshot of the latest **completed** review
/// dispatch for a card. This is the context that is available in the
/// production flow at `/api/review-decision` time — the review dispatch has
/// already terminated by the time the operator decides to dispute, so the
/// out-of-scope close path MUST bind to a completed (not active) row.
///
/// We surface the dispatch id (so we can record `review_dispatch_id` in the
/// scope_mismatch_closed result for forensic correlation and so the by-id
/// pattern from sub-fix 4 can verify the dispute payload references this
/// completed dispatch), plus the parsed `reviewed_commit` and `target_repo`
/// from its context (so the scope check can re-run against the same commit
/// the reviewer saw).
#[derive(Debug, Clone)]
struct CompletedReviewDispatch {
    id: String,
    reviewed_commit: Option<String>,
    target_repo: Option<String>,
}

async fn latest_completed_review_dispatch_pg_first(
    state: &AppState,
    card_id: &str,
) -> Option<CompletedReviewDispatch> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT id, context
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status = 'completed'
             ORDER BY completed_at DESC NULLS LAST, updated_at DESC NULLS LAST
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(row) => row.map(|(id, context_raw)| {
                let active = build_active_review_dispatch(id, context_raw);
                CompletedReviewDispatch {
                    id: active.id,
                    reviewed_commit: active.reviewed_commit,
                    target_repo: active.target_repo,
                }
            }),
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres completed review dispatch"
                );
                None
            }
        };
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(db) = state.legacy_db() {
        return db.separate_conn().ok().and_then(|conn| {
            conn.query_row(
                "SELECT id, context
                 FROM task_dispatches
                 WHERE kanban_card_id = ?1
                   AND dispatch_type = 'review'
                   AND status = 'completed'
                 ORDER BY COALESCE(completed_at, updated_at) DESC, updated_at DESC, rowid DESC
                 LIMIT 1",
                [card_id],
                |row| {
                    let id: String = row.get(0)?;
                    let context_raw: Option<String> = row.get(1)?;
                    Ok((id, context_raw))
                },
            )
            .optional()
            .ok()
            .flatten()
            .map(|(id, context_raw)| {
                let active = build_active_review_dispatch(id, context_raw);
                CompletedReviewDispatch {
                    id: active.id,
                    reviewed_commit: active.reviewed_commit,
                    target_repo: active.target_repo,
                }
            })
        });
    }

    None
}

/// #2341 / #2200 sub-3: outcome of the source-review-by-id lookup.
#[derive(Debug)]
enum SourceReviewLookup {
    /// Source review id was loaded from the review-decision context and
    /// resolved cleanly to a completed review row.
    ResolvedById(CompletedReviewDispatch),
    /// Review-decision context did NOT include `source_review_dispatch_id`
    /// (legacy row from before the persistence change). Caller may fall
    /// back to the latest completed review.
    LegacyFallback(Option<CompletedReviewDispatch>),
    /// Review-decision context referenced a `source_review_dispatch_id` that
    /// does NOT resolve to a completed review row (missing, uncompleted,
    /// cross-card, or wrong dispatch_type). Codex round-2 [medium]: caller
    /// MUST fail closed — falling back to latest-completed would bind to a
    /// duplicate or unrelated review.
    UnresolvedSourceId(String),
}

/// #2341 / #2200 sub-3 (Codex round-1 [medium] + round-2 [medium]): bind
/// the close path to the source review dispatch that produced THIS
/// review-decision, not to the latest completed review for the card.
/// Loads the review-decision dispatch context to extract
/// `source_review_dispatch_id` (persisted by `discord_delivery::orchestration`
/// when the follow-up was created), then loads that review row by id.
/// Returns:
///   * `ResolvedById` — the source id resolved to a completed review row.
///   * `LegacyFallback(latest)` — context predates the persistence change;
///     the caller may use latest-completed as a defensible fallback.
///   * `UnresolvedSourceId(srid)` — context has a source id but it does not
///     resolve; caller MUST fail closed (no silent fallback that could
///     bind to the wrong review row).
async fn source_review_dispatch_for_decision_pg_first(
    state: &AppState,
    card_id: &str,
    rd_id: &str,
) -> SourceReviewLookup {
    // Load the review-decision dispatch context.
    let rd_context_raw: Option<String> = if let Some(pool) = state.pg_pool_ref() {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT context FROM task_dispatches WHERE id = $1 AND kanban_card_id = $2 AND dispatch_type = 'review-decision'",
        )
        .bind(rd_id)
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
    } else {
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        {
            state
                .legacy_db()
                .and_then(|db| db.separate_conn().ok())
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT context FROM task_dispatches WHERE id = ?1 AND kanban_card_id = ?2 AND dispatch_type = 'review-decision'",
                        [rd_id, card_id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()
                    .ok()
                    .flatten()
                    .flatten()
                })
        }
        #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
        {
            None
        }
    };

    let source_review_dispatch_id: Option<String> = rd_context_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|v| {
            v.get("source_review_dispatch_id")
                .and_then(|s| s.as_str())
                .map(str::to_string)
        });

    if let Some(srid) = source_review_dispatch_id {
        // Load the EXACT review by id — scoped to the same card +
        // dispatch_type so a stale or unrelated id cannot bind.
        let row: Option<(String, Option<String>)> = if let Some(pool) = state.pg_pool_ref() {
            sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT id, context FROM task_dispatches WHERE id = $1 AND kanban_card_id = $2 AND dispatch_type = 'review' AND status = 'completed'",
            )
            .bind(&srid)
            .bind(card_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
        } else {
            #[cfg(all(test, feature = "legacy-sqlite-tests"))]
            {
                state
                    .legacy_db()
                    .and_then(|db| db.separate_conn().ok())
                    .and_then(|conn| {
                        conn.query_row(
                            "SELECT id, context FROM task_dispatches WHERE id = ?1 AND kanban_card_id = ?2 AND dispatch_type = 'review' AND status = 'completed'",
                            [srid.as_str(), card_id],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                        )
                        .optional()
                        .ok()
                        .flatten()
                    })
            }
            #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
            {
                None
            }
        };

        if let Some((id, context_raw)) = row {
            let active = build_active_review_dispatch(id, context_raw);
            return SourceReviewLookup::ResolvedById(CompletedReviewDispatch {
                id: active.id,
                reviewed_commit: active.reviewed_commit,
                target_repo: active.target_repo,
            });
        }
        // Codex round-2 [medium]: do NOT silently fall back to
        // latest-completed when an explicit source id was recorded but does
        // not resolve. That would reintroduce the wrong-row binding the by-id
        // path was meant to prevent.
        tracing::warn!(
            card_id,
            rd_id,
            source_review_dispatch_id = %srid,
            "[review-decision] #2341 source_review_dispatch_id from review-decision context did not resolve to a completed review row; failing closed (no silent latest-completed fallback)"
        );
        return SourceReviewLookup::UnresolvedSourceId(srid);
    }

    // Legacy fallback: review-decision context predates the
    // source_review_dispatch_id persistence change. Latest-completed is
    // defensible here because there was no recorded source id to honor.
    SourceReviewLookup::LegacyFallback(
        latest_completed_review_dispatch_pg_first(state, card_id).await,
    )
}

/// #2341 / #2200 sub-3 redesign: card lifecycle generation marker.
///
/// The close path captures this snapshot before doing any work and re-checks
/// it inside the close transaction. If the card has been re-opened since the
/// review dispatch completed (a fresh review round started, a new pending
/// dispatch was created, or `review_entered_at` advanced), the snapshot
/// changes and the close refuses with 409 stale.
///
/// We bind to three fields:
///   * `kanban_cards.latest_dispatch_id` — flips when any new dispatch is
///     created against the card.
///   * `card_review_state.review_round` — advances on `mark_next_review_round_advance_pg_first`.
///   * `card_review_state.review_entered_at` — set fresh on each new review
///     round entry, cleared on terminal cleanup.
///
/// All three together form the generation marker — equality of all three
/// (or all-None for legacy/missing rows) is required for the close to
/// proceed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct CardLifecycleSnapshot {
    latest_dispatch_id: Option<String>,
    review_round: Option<i32>,
    review_entered_at_iso: Option<String>,
}

async fn card_lifecycle_snapshot_pg_first(
    state: &AppState,
    card_id: &str,
) -> CardLifecycleSnapshot {
    if let Some(pool) = state.pg_pool_ref() {
        let latest_dispatch_id: Option<String> = sqlx::query_scalar::<_, Option<String>>(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten();
        let review_fields: Option<(Option<i32>, Option<chrono::DateTime<chrono::Utc>>)> =
            sqlx::query_as::<_, (Option<i32>, Option<chrono::DateTime<chrono::Utc>>)>(
                "SELECT review_round, review_entered_at FROM card_review_state WHERE card_id = $1",
            )
            .bind(card_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
        let (review_round, review_entered_at) = match review_fields {
            Some((r, t)) => (r, t),
            None => (None, None),
        };
        return CardLifecycleSnapshot {
            latest_dispatch_id,
            review_round,
            review_entered_at_iso: review_entered_at
                .map(|ts| ts.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
        };
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(db) = state.legacy_db() {
        if let Ok(conn) = db.separate_conn() {
            let latest_dispatch_id: Option<String> = conn
                .query_row(
                    "SELECT latest_dispatch_id FROM kanban_cards WHERE id = ?1",
                    [card_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .ok()
                .flatten()
                .flatten();
            let review_fields: Option<(Option<i32>, Option<String>)> = conn
                .query_row(
                    "SELECT review_round, review_entered_at FROM card_review_state WHERE card_id = ?1",
                    [card_id],
                    |row| {
                        Ok((
                            row.get::<_, Option<i32>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .optional()
                .ok()
                .flatten();
            let (review_round, review_entered_at_iso) = match review_fields {
                Some((r, t)) => (r, t),
                None => (None, None),
            };
            return CardLifecycleSnapshot {
                latest_dispatch_id,
                review_round,
                review_entered_at_iso,
            };
        }
    }

    CardLifecycleSnapshot::default()
}

/// #2341 / #2200 sub-3 redesign: PG-tri-state scope check delegating to the
/// dispatch-context helper. Returns `Unknown` when no PG pool is wired in,
/// which the caller must treat as a refusal on the out-of-scope close path.
async fn commit_belongs_to_card_issue_pg_first_tri(
    state: &AppState,
    card_id: &str,
    commit_sha: &str,
    target_repo: Option<&str>,
) -> crate::dispatch::ScopeCheck {
    if let Some(pool) = state.pg_pool_ref() {
        return crate::dispatch::commit_belongs_to_card_issue_pg_tri(
            pool,
            card_id,
            commit_sha,
            target_repo,
        )
        .await;
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(db) = state.legacy_db() {
        return crate::dispatch::commit_belongs_to_card_issue_tri(
            &db,
            card_id,
            commit_sha,
            target_repo,
        );
    }

    crate::dispatch::ScopeCheck::Unknown
}

/// #2341 / #2200 sub-3 redesign: detect a prior `scope_mismatch_closed`
/// finalize for idempotent re-POST handling. Composes with sub-fix 1's
/// proof-of-finalization model — the originating review-decision dispatch
/// carries `result.outcome = scope_mismatch_closed`, which is sufficient
/// proof that a prior call already finalized the close.
#[derive(Debug, Clone)]
struct PriorScopeMismatchClose {
    dispatch_id: String,
    review_dispatch_id: Option<String>,
    reviewed_commit: Option<String>,
    lifecycle_generation: Option<CardLifecycleSnapshot>,
}

async fn recent_scope_mismatch_finalized_pg_first(
    state: &AppState,
    card_id: &str,
) -> Option<PriorScopeMismatchClose> {
    let raw = if let Some(pool) = state.pg_pool_ref() {
        match sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT id, result
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review-decision'
               AND status = 'completed'
               AND result IS NOT NULL
               AND (result::jsonb ->> 'outcome') = 'scope_mismatch_closed'
             ORDER BY completed_at DESC NULLS LAST, updated_at DESC NULLS LAST
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(row) => row,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load recent scope_mismatch_closed dispatch (postgres)"
                );
                None
            }
        }
    } else {
        None
    };

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    let raw = if raw.is_some() {
        raw
    } else if let Some(db) = state.legacy_db() {
        db.separate_conn().ok().and_then(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, result
                     FROM task_dispatches
                     WHERE kanban_card_id = ?1
                       AND dispatch_type = 'review-decision'
                       AND status = 'completed'
                       AND result IS NOT NULL
                     ORDER BY COALESCE(completed_at, updated_at) DESC, rowid DESC
                     LIMIT 8",
                )
                .ok()?;
            let mut rows = stmt
                .query_map([card_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .ok()?;
            while let Some(Ok((id, result_raw))) = rows.next() {
                if let Some(raw) = result_raw.as_deref() {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
                        if value.get("outcome").and_then(|v| v.as_str())
                            == Some("scope_mismatch_closed")
                        {
                            return Some((id, result_raw));
                        }
                    }
                }
            }
            None
        })
    } else {
        None
    };

    let (dispatch_id, result_raw) = raw?;
    let parsed = result_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
    let review_dispatch_id = parsed
        .as_ref()
        .and_then(|v| v.get("review_dispatch_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let reviewed_commit = parsed
        .as_ref()
        .and_then(|v| v.get("reviewed_commit"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let lifecycle_generation = parsed
        .as_ref()
        .and_then(|v| v.get("lifecycle_generation"))
        .and_then(|v| serde_json::from_value::<CardLifecycleSnapshot>(v.clone()).ok());
    Some(PriorScopeMismatchClose {
        dispatch_id,
        review_dispatch_id,
        reviewed_commit,
        lifecycle_generation,
    })
}

/// #2341 / #2200 sub-3 redesign: atomic close transaction.
///
/// In a single Postgres transaction (when a PG pool is available):
///   1. Re-check the card lifecycle generation snapshot — fail with 409
///      stale if the card has been re-opened since the snapshot was taken.
///   2. UPDATE the originating review-decision dispatch to status=completed
///      with `result.outcome = scope_mismatch_closed` — require exactly 1
///      row affected, scoped to the live-statuses `('pending','dispatched')`
///      so a concurrent finalizer cannot be silently overwritten.
///   3. UPDATE `card_review_state.state = dispute_scope_mismatch_closed` and
///      record `last_decision = dispute`.
///
/// On any failure the entire transaction rolls back. Card transition and
/// dismiss-cleanup happen outside the tx but with a final stale-recheck so
/// we still refuse to terminalize a card that was reopened between the tx
/// commit and the transition.
///
/// In sqlite-test mode we fall back to sequential statements (the legacy
/// `set_dispatch_status_with_backends` path) since the test fixture is the
/// only consumer there. This matches the rest of the file.
async fn atomic_finalize_scope_mismatch_close_pg(
    state: &AppState,
    card_id: &str,
    rd_id: &str,
    review_dispatch_id: &str,
    reviewed_commit: &str,
    expected_lifecycle: &CardLifecycleSnapshot,
) -> Result<u64, ScopeMismatchCloseError> {
    let lifecycle_json = serde_json::to_value(expected_lifecycle).unwrap_or(json!({}));
    let result_json = json!({
        "decision": "dispute",
        "outcome": "scope_mismatch_closed",
        "completion_source": "review_decision_api",
        "review_dispatch_id": review_dispatch_id,
        "reviewed_commit": reviewed_commit,
        "lifecycle_generation": lifecycle_json,
    });

    if let Some(pool) = state.pg_pool_ref() {
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| ScopeMismatchCloseError::Internal(format!("begin tx: {e}")))?;

        // Stale re-check inside tx.
        let actual_latest_dispatch_id: Option<String> = sqlx::query_scalar::<_, Option<String>>(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = $1 FOR UPDATE",
        )
        .bind(card_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| ScopeMismatchCloseError::Internal(format!("read card lifecycle: {e}")))?
        .flatten();
        let review_fields: Option<(Option<i32>, Option<chrono::DateTime<chrono::Utc>>)> =
            sqlx::query_as::<_, (Option<i32>, Option<chrono::DateTime<chrono::Utc>>)>(
                "SELECT review_round, review_entered_at FROM card_review_state WHERE card_id = $1 FOR UPDATE",
            )
            .bind(card_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| ScopeMismatchCloseError::Internal(format!("read review state: {e}")))?;
        let actual_review_round = review_fields.as_ref().and_then(|(r, _)| *r);
        let actual_review_entered_at_iso = review_fields
            .as_ref()
            .and_then(|(_, t)| t.as_ref())
            .map(|ts| ts.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
        let actual = CardLifecycleSnapshot {
            latest_dispatch_id: actual_latest_dispatch_id,
            review_round: actual_review_round,
            review_entered_at_iso: actual_review_entered_at_iso,
        };
        if &actual != expected_lifecycle {
            return Err(ScopeMismatchCloseError::LifecycleStale {
                expected: expected_lifecycle.clone(),
                actual,
            });
        }

        // Finalize originating review-decision dispatch. Restricted to live
        // statuses so a concurrent finalizer is detected (rowcount=0).
        let rd_update = sqlx::query(
            "UPDATE task_dispatches
             SET status = 'completed',
                 result = $1,
                 completed_at = COALESCE(completed_at, NOW()),
                 updated_at = NOW()
             WHERE id = $2
               AND kanban_card_id = $3
               AND dispatch_type = 'review-decision'
               AND status IN ('pending', 'dispatched')",
        )
        .bind(result_json.to_string())
        .bind(rd_id)
        .bind(card_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ScopeMismatchCloseError::Internal(format!("finalize rd: {e}")))?;
        if rd_update.rows_affected() != 1 {
            return Err(ScopeMismatchCloseError::DispatchConsumed);
        }

        // Update canonical card_review_state.
        sqlx::query(
            "INSERT INTO card_review_state (card_id, state, last_decision, decided_at, updated_at)
             VALUES ($1, 'dispute_scope_mismatch_closed', 'dispute', NOW(), NOW())
             ON CONFLICT (card_id) DO UPDATE SET
                 state = 'dispute_scope_mismatch_closed',
                 last_decision = 'dispute',
                 decided_at = NOW(),
                 updated_at = NOW()",
        )
        .bind(card_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ScopeMismatchCloseError::Internal(format!("update review state: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| ScopeMismatchCloseError::Internal(format!("commit tx: {e}")))?;
        return Ok(1);
    }

    // No PG pool: fall back to the legacy `set_dispatch_status_with_backends`
    // path so the sqlite test fixture exercises the same close shape. The
    // lifecycle re-check is best-effort here (sqlite has no FOR UPDATE).
    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    {
        let actual = card_lifecycle_snapshot_pg_first(state, card_id).await;
        if &actual != expected_lifecycle {
            return Err(ScopeMismatchCloseError::LifecycleStale {
                expected: expected_lifecycle.clone(),
                actual,
            });
        }
        let status_db = state.legacy_db();
        match crate::dispatch::set_dispatch_status_with_backends(
            status_db,
            state.pg_pool_ref(),
            rd_id,
            "completed",
            Some(&result_json),
            "mark_dispatch_completed",
            Some(&["pending", "dispatched"]),
            true,
        ) {
            Ok(1) => return Ok(1),
            Ok(_) => return Err(ScopeMismatchCloseError::DispatchConsumed),
            Err(e) => return Err(ScopeMismatchCloseError::Internal(e.to_string())),
        }
    }

    #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
    {
        let _ = state;
        let _ = card_id;
        let _ = rd_id;
        let _ = review_dispatch_id;
        let _ = reviewed_commit;
        Err(ScopeMismatchCloseError::Internal(
            "no postgres pool available".to_string(),
        ))
    }
}

#[derive(Debug)]
enum ScopeMismatchCloseError {
    /// Card lifecycle generation changed between snapshot and tx — card was
    /// re-opened.
    LifecycleStale {
        expected: CardLifecycleSnapshot,
        actual: CardLifecycleSnapshot,
    },
    /// Originating review-decision dispatch was already consumed by a
    /// concurrent finalizer (rowcount = 0 on the live-status guard).
    DispatchConsumed,
    /// Anything else.
    Internal(String),
}

// `CardLifecycleSnapshot` is serialized into the dispatch result so a later
// idempotent retry can recover the generation it was closed against.
impl serde::Serialize for CardLifecycleSnapshot {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = ser.serialize_struct("CardLifecycleSnapshot", 3)?;
        s.serialize_field("latest_dispatch_id", &self.latest_dispatch_id)?;
        s.serialize_field("review_round", &self.review_round)?;
        s.serialize_field("review_entered_at_iso", &self.review_entered_at_iso)?;
        s.end()
    }
}

impl<'de> serde::Deserialize<'de> for CardLifecycleSnapshot {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Helper {
            #[serde(default)]
            latest_dispatch_id: Option<String>,
            #[serde(default)]
            review_round: Option<i32>,
            #[serde(default)]
            review_entered_at_iso: Option<String>,
        }
        let helper = Helper::deserialize(de)?;
        Ok(CardLifecycleSnapshot {
            latest_dispatch_id: helper.latest_dispatch_id,
            review_round: helper.review_round,
            review_entered_at_iso: helper.review_entered_at_iso,
        })
    }
}

async fn card_issue_number_pg_first(state: &AppState, card_id: &str) -> Option<i64> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_scalar::<_, Option<i64>>(
            "SELECT github_issue_number::BIGINT FROM kanban_cards WHERE id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(issue_number) => issue_number.flatten(),
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres card issue number"
                );
                None
            }
        };
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(db) = state.legacy_db() {
        return db.separate_conn().ok().and_then(|conn| {
            conn.query_row(
                "SELECT github_issue_number FROM kanban_cards WHERE id = ?1",
                [card_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten()
        });
    }

    None
}

#[derive(Debug, Clone)]
pub(crate) struct SkipReworkDiagnostics {
    pub(crate) skip_rework: bool,
    pub(crate) last_reviewed_commit: Option<String>,
    pub(crate) current_commit: Option<String>,
    pub(crate) current_commit_source: Option<&'static str>,
    pub(crate) issue_number: Option<i64>,
    pub(crate) reason: &'static str,
}

impl SkipReworkDiagnostics {
    fn to_json(&self) -> serde_json::Value {
        json!({
            "skip_rework": self.skip_rework,
            "last_reviewed_commit": self.last_reviewed_commit.as_deref(),
            "current_commit": self.current_commit.as_deref(),
            "current_commit_source": self.current_commit_source,
            "issue_number": self.issue_number,
            "reason": self.reason,
        })
    }
}

fn normalize_optional_commit_sha(commit_sha: Option<&str>) -> Result<Option<String>, String> {
    let Some(commit_sha) = commit_sha else {
        return Ok(None);
    };
    let trimmed = commit_sha.trim();
    if trimmed.is_empty() {
        return Err("commit_sha must not be empty when provided".to_string());
    }
    if !(7..=64).contains(&trimmed.len()) || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("commit_sha must be a 7-64 character hex git commit SHA".to_string());
    }
    Ok(Some(trimmed.to_ascii_lowercase()))
}

fn commit_sha_differs(current: &str, previous: &str) -> bool {
    !current.eq_ignore_ascii_case(previous)
}

pub(crate) async fn evaluate_accept_skip_rework(
    state: &AppState,
    card_id: &str,
    submitted_commit: Option<&str>,
) -> SkipReworkDiagnostics {
    let last_review_context = latest_completed_review_context_pg_first(state, card_id).await;

    let last_reviewed_commit: Option<String> = last_review_context.as_ref().and_then(|v| {
        v.get("reviewed_commit")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
    });

    let issue_number = card_issue_number_pg_first(state, card_id).await;

    let (current_commit, current_commit_source) = if let Some(submitted_commit) = submitted_commit {
        (Some(submitted_commit.to_string()), Some("request"))
    } else if last_reviewed_commit.is_some() {
        if let Some(issue_num) = issue_number {
            (
                current_issue_worktree_commit(
                    state.engine.pg_pool(),
                    card_id,
                    issue_num,
                    last_review_context.as_ref(),
                )
                .await,
                Some("worktree"),
            )
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    let (skip_rework, reason) = match (&last_reviewed_commit, &current_commit) {
        (Some(previous), Some(current)) if commit_sha_differs(current, previous) => {
            (true, "current_commit_differs_from_reviewed_commit")
        }
        (Some(_), Some(_)) => (false, "current_commit_matches_reviewed_commit"),
        (None, _) => (false, "missing_last_reviewed_commit"),
        (_, None) if submitted_commit.is_none() && issue_number.is_none() => {
            (false, "missing_issue_number_for_worktree_inference")
        }
        (_, None) => (false, "missing_current_commit"),
    };

    let diagnostics = SkipReworkDiagnostics {
        skip_rework,
        last_reviewed_commit,
        current_commit,
        current_commit_source,
        issue_number,
        reason,
    };

    tracing::info!(
        card_id = %card_id,
        skip_rework = diagnostics.skip_rework,
        last_reviewed_commit = diagnostics.last_reviewed_commit.as_deref().unwrap_or(""),
        current_commit = diagnostics.current_commit.as_deref().unwrap_or(""),
        current_commit_source = diagnostics.current_commit_source.unwrap_or(""),
        issue_number = ?diagnostics.issue_number,
        reason = diagnostics.reason,
        "[review-decision] #1977 evaluated accept skip_rework"
    );

    diagnostics
}

async fn stale_review_dispatch_ids_pg_first(state: &AppState, card_id: &str) -> Vec<String> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_scalar::<_, String>(
            "SELECT id
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status IN ('pending', 'dispatched')",
        )
        .bind(card_id)
        .fetch_all(pool)
        .await
        {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres stale review dispatches"
                );
                Vec::new()
            }
        };
    }

    Vec::new()
}

async fn prepare_dispute_review_entry_pg_first(
    state: &AppState,
    card_id: &str,
) -> Result<(), String> {
    let pool = state
        .pg_pool_ref()
        .ok_or_else(|| "postgres pool unavailable for dispute review-entry".to_string())?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin dispute review-entry tx for {card_id}: {error}"))?;
    let dispute_intents = [
        crate::engine::transition::TransitionIntent::SetReviewStatus {
            card_id: card_id.to_string(),
            review_status: Some("reviewing".to_string()),
        },
        crate::engine::transition::TransitionIntent::SyncReviewState {
            card_id: card_id.to_string(),
            state: "reviewing".to_string(),
        },
    ];
    for intent in &dispute_intents {
        crate::engine::transition_executor_pg::execute_pg_transition_intent(&mut tx, intent)
            .await?;
    }
    sqlx::query("UPDATE kanban_cards SET review_entered_at = NOW() WHERE id = $1")
        .bind(card_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("set review_entered_at for {card_id}: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| format!("commit dispute review-entry tx for {card_id}: {error}"))?;
    Ok(())
}

async fn finalize_accept_cleanup_pg_first(
    state: &AppState,
    card_id: &str,
    clear_review_status: bool,
) -> Result<(), String> {
    let pool = state
        .pg_pool_ref()
        .ok_or_else(|| "postgres pool unavailable for accept cleanup".to_string())?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin accept cleanup tx for {card_id}: {error}"))?;
    if clear_review_status {
        crate::engine::transition_executor_pg::execute_pg_transition_intent(
            &mut tx,
            &crate::engine::transition::TransitionIntent::SetReviewStatus {
                card_id: card_id.to_string(),
                review_status: None,
            },
        )
        .await?;
    }
    sqlx::query("UPDATE kanban_cards SET suggestion_pending_at = NULL WHERE id = $1")
        .bind(card_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("clear suggestion_pending_at for {card_id}: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| format!("commit accept cleanup tx for {card_id}: {error}"))?;
    Ok(())
}

async fn commit_belongs_to_card_issue_pg_first(
    state: &AppState,
    card_id: &str,
    commit_sha: &str,
    target_repo: Option<&str>,
) -> bool {
    if let Some(pool) = state.pg_pool_ref() {
        return crate::dispatch::commit_belongs_to_card_issue_pg(
            pool,
            card_id,
            commit_sha,
            target_repo,
        )
        .await;
    }

    false
}

async fn cancel_dispatch_pg_first(
    state: &AppState,
    dispatch_id: &str,
    reason: Option<&str>,
) -> Result<usize, String> {
    if let Some(pool) = state.pg_pool_ref() {
        return crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
            pool,
            dispatch_id,
            reason,
        )
        .await;
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    if let Some(db) = state.legacy_db() {
        let conn = db
            .separate_conn()
            .map_err(|error| format!("open sqlite cancel dispatch connection: {error}"))?;
        return crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
            &conn,
            dispatch_id,
            reason,
        )
        .map_err(|error| format!("sqlite cancel dispatch {dispatch_id}: {error}"));
    }

    Err("postgres pool unavailable for cancel dispatch".to_string())
}

async fn dismiss_review_cleanup_pg_first(state: &AppState, card_id: &str) -> Result<(), String> {
    let Some(pool) = state.pg_pool_ref() else {
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        if let Some(db) = state.legacy_db() {
            let conn = db
                .separate_conn()
                .map_err(|error| format!("open sqlite dismiss cleanup connection: {error}"))?;
            let dispatch_ids = conn
                .prepare(
                    "SELECT id FROM task_dispatches
                     WHERE kanban_card_id = ?1
                       AND status IN ('pending', 'dispatched')
                       AND dispatch_type IN ('review', 'review-decision')",
                )
                .and_then(|mut stmt| {
                    let rows = stmt.query_map([card_id], |row| row.get::<_, String>(0))?;
                    rows.collect::<sqlite_test::Result<Vec<_>>>()
                })
                .map_err(|error| {
                    format!("load sqlite dismiss cleanup dispatches for {card_id}: {error}")
                })?;

            for dispatch_id in &dispatch_ids {
                crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
                    &conn,
                    dispatch_id,
                    None,
                )
                .map_err(|error| {
                    format!("sqlite dismiss cleanup cancel dispatch {dispatch_id}: {error}")
                })?;
            }
            conn.execute(
                "UPDATE kanban_cards
                 SET review_status = NULL,
                     channel_thread_map = NULL,
                     active_thread_id = NULL,
                     updated_at = datetime('now')
                 WHERE id = ?1",
                [card_id],
            )
            .map_err(|error| format!("clear sqlite dismiss review state for {card_id}: {error}"))?;
            return Ok(());
        }

        return Err("postgres pool unavailable for dismiss cleanup".to_string());
    };
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin dismiss cleanup tx for {card_id}: {error}"))?;

    let dispatch_ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')
           AND dispatch_type IN ('review', 'review-decision')",
    )
    .bind(card_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("load dismiss cleanup dispatches for {card_id}: {error}"))?;

    for dispatch_id in &dispatch_ids {
        crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg_tx(&mut tx, dispatch_id, None)
            .await?;
    }

    let clear_review_status = crate::engine::transition::TransitionIntent::SetReviewStatus {
        card_id: card_id.to_string(),
        review_status: None,
    };
    crate::engine::transition_executor_pg::execute_pg_transition_intent(
        &mut tx,
        &clear_review_status,
    )
    .await?;

    sqlx::query(
        "UPDATE kanban_cards
         SET channel_thread_map = NULL,
             active_thread_id = NULL
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("clear dismiss thread mappings for {card_id}: {error}"))?;

    tx.commit()
        .await
        .map_err(|error| format!("commit dismiss cleanup tx for {card_id}: {error}"))?;
    Ok(())
}

#[derive(Debug, Deserialize, Serialize, Default)]
#[allow(dead_code)]
pub struct ReviewDecisionBody {
    pub card_id: String,
    pub decision: String, // "accept", "dispute", "dismiss"
    pub comment: Option<String>,
    /// Optional current implementation commit. When accept is submitted after
    /// the agent has already committed fixes during review-decision, this takes
    /// precedence over worktree inference for #246 skip_rework detection.
    pub commit_sha: Option<String>,
    /// #109: dispatch-scoped targeting — when provided, the server validates
    /// that this dispatch_id matches the pending review-decision dispatch for
    /// the card. Prevents replayed/stale decisions from consuming the wrong
    /// dispatch.
    pub dispatch_id: Option<String>,
    /// #2341 / #2200 sub-3: when the agent disputes a review because the
    /// finding lies outside the current card's scope (e.g. a stacked-branch
    /// leftover), set this to true. The server closes the pending
    /// review-decision dispatch with outcome `scope_mismatch_closed` and
    /// routes the card to terminal state instead of requiring an in-issue
    /// re-review target. Only meaningful when `decision == "dispute"`.
    ///
    /// The close path binds to the latest **completed** review dispatch
    /// (which is what is available at decision time in production flow), and
    /// fail-closes on Unknown scope verification (transient PG/git failure)
    /// or a card lifecycle generation mismatch (card re-opened since the
    /// review completed).
    #[serde(default)]
    pub out_of_scope: Option<bool>,
}

/// POST /api/reviews/decision
///
/// Agent's decision on counter-model review feedback.
/// - accept: agent will rework based on review → card to in_progress
/// - dispute: agent disagrees, sends back for re-review → new review dispatch
/// - dismiss: agent ignores review → card to done
pub async fn submit_review_decision(
    State(state): State<AppState>,
    Json(body): Json<ReviewDecisionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let valid = ["accept", "dispute", "dismiss"];
    if !valid.contains(&body.decision.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("decision must be one of: {}", valid.join(", "))})),
        );
    }

    let submitted_commit = match normalize_optional_commit_sha(body.commit_sha.as_deref()) {
        Ok(commit) => commit,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": error, "field": "commit_sha"})),
            );
        }
    };

    if !card_exists_pg_first(&state, &body.card_id).await {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "card not found"})),
        );
    }

    let mut pending_rd_id =
        pending_review_decision_dispatch_id_pg_first(&state, &body.card_id).await;

    // #2200 sub-fix 4 (`stale-dispatch-mismatch`):
    // If the caller submitted an explicit `dispatch_id` and the canonical
    // pending lookup missed it, fall back to a by-id lookup scoped to the
    // same card and `dispatch_type = 'review-decision'`. This recovers the
    // case where the originating dispatch row is still `dispatched` but the
    // `card_review_state.pending_dispatch_id` / `kanban_cards.latest_dispatch_id`
    // links were cleared (e.g. by a follow-up dispatch that did not finalize
    // the predecessor).
    //
    // Authorization layering (see `lookup_review_decision_dispatch_by_id`):
    //   - Cross-card / cross-type ids return `NotFound` → 404.
    //   - Older live rows superseded by a newer live row return
    //     `LiveButSuperseded` → 409 (blocks replay of stale same-card ids).
    //   - Only the most-recent live row is honored (`LiveAndCurrent`).
    //   - Terminal rows fall through to the canonical "no pending" 409,
    //     leaving room for PR #2280 sub-fix 1's proven-finalized idempotent
    //     path to compose without short-circuit.
    if pending_rd_id.is_none() {
        if let Some(ref submitted_did) = body.dispatch_id {
            match lookup_review_decision_dispatch_by_id(&state, &body.card_id, submitted_did).await
            {
                ReviewDecisionDispatchLookup::LiveAndCurrent => {
                    tracing::info!(
                        card_id = %body.card_id,
                        dispatch_id = %submitted_did,
                        "[review-decision] #2200 sub-fix 4: honoring submitted dispatch_id whose link rows were cleared but dispatch is still live and current"
                    );
                    pending_rd_id = Some(submitted_did.clone());
                }
                ReviewDecisionDispatchLookup::LiveButSuperseded => {
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({
                            "error": "review-decision dispatch is superseded by a newer live dispatch for this card",
                            "card_id": body.card_id,
                            "dispatch_id": submitted_did,
                        })),
                    );
                }
                ReviewDecisionDispatchLookup::Terminal => {
                    // Intentional fall-through: the row is terminal, which is
                    // sub-fix 1's territory (PR #2280 proven-finalized).
                    // Returning the canonical 409 here keeps the response
                    // shape compatible with sub-1 and lets that branch
                    // promote to 200 already_finalized once merged.
                }
                ReviewDecisionDispatchLookup::NotFound => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({
                            "error": "review-decision dispatch not found for this card",
                            "card_id": body.card_id,
                            "dispatch_id": submitted_did,
                        })),
                    );
                }
            }
        }
    }

    if pending_rd_id.is_none() {
        // #2341 / #2200 sub-3 idempotent resume: a prior dispute+out_of_scope
        // call already finalized the originating review-decision dispatch
        // with `result.outcome = scope_mismatch_closed`. The pending lookup
        // correctly returns None (the dispatch is no longer pending), but we
        // must NOT reject the retry — that would mislead the operator into
        // thinking the close failed. Instead, detect the prior finalize and
        // return 200 already_finalized.
        //
        // Compose with sub-fix 1 (PR #2280): sub-1's `proven_finalized_decision`
        // path only fires for `accept`/`dispute`/`dismiss` decisions emitted
        // by `review_decision_api` or `review_auto_accept_policy`. Our
        // scope_mismatch_closed close path emits the same shape with
        // `completion_source = review_decision_api` and
        // `decision = dispute`, but with the additional `outcome` field. To
        // avoid sub-1 incorrectly classifying this as a normal dispute and
        // accepting an `accept` retry against it, we detect the outcome
        // first and short-circuit before sub-1's branch runs.
        if body.decision == "dispute" && body.out_of_scope == Some(true) {
            if let Some(prior) =
                recent_scope_mismatch_finalized_pg_first(&state, &body.card_id).await
            {
                // dispatch_id must match the finalized dispatch — closes the
                // probing oracle that would let a caller learn which
                // dispatch_id terminalized this card.
                if let Some(submitted) = body.dispatch_id.as_deref() {
                    if submitted != prior.dispatch_id {
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "error": format!(
                                    "out_of_scope retry dispatch_id mismatch: submitted {submitted} but prior finalized scope_mismatch_closed is {}",
                                    prior.dispatch_id
                                ),
                                "card_id": body.card_id,
                            })),
                        );
                    }
                } else {
                    // No dispatch_id supplied — refuse with the generic 409
                    // rather than disclosing the prior close.
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({
                            "error": "no pending review-decision dispatch for this card",
                            "card_id": body.card_id,
                        })),
                    );
                }

                // Determine whether the card already reached terminal in a
                // prior successful call. Terminal cleanup clears
                // `kanban_cards.latest_dispatch_id` (Codex round-2 [medium]),
                // which would otherwise make the stored lifecycle generation
                // diverge from the current snapshot for a fully successful
                // close — leading to a spurious 409 on retry. So:
                //   - If the card IS terminal: the prior close completed;
                //     skip the strict generation comparison, return
                //     already_finalized.
                //   - If the card is NOT terminal: the prior close was
                //     partial (tx committed but transition / cleanup did
                //     not run). Strict generation comparison is then the
                //     correct guard against terminalizing a re-opened card.
                let card_ctx =
                    load_review_decision_card_context_pg_first(&state, &body.card_id).await;
                let effective_pipeline = resolve_effective_pipeline_pg_first(
                    &state,
                    card_ctx.repo_id.as_deref(),
                    card_ctx.agent_id.as_deref(),
                )
                .await;
                let current_status = card_ctx.status.clone().unwrap_or_default();
                let terminal_state = effective_pipeline
                    .states
                    .iter()
                    .find(|s| s.terminal)
                    .map(|s| s.id.clone())
                    .unwrap_or_else(|| "done".to_string());
                let card_is_terminal = effective_pipeline.is_terminal(&current_status);

                if !card_is_terminal {
                    // Generation marker: enforce only on the non-terminal
                    // resume path. Terminalizing a re-opened card from a
                    // stale closure is the failure mode HIGH 2 warned
                    // about. The dispatch_id match above already proved
                    // dispatch-scope authorization; lifecycle proves the
                    // card is the same generation we closed against.
                    if let Some(expected) = prior.lifecycle_generation.clone() {
                        let actual = card_lifecycle_snapshot_pg_first(&state, &body.card_id).await;
                        if actual != expected {
                            tracing::warn!(
                                card_id = %body.card_id,
                                ?expected,
                                ?actual,
                                "[review-decision] #2341 idempotent resume refused: card lifecycle advanced since prior scope_mismatch_closed (non-terminal card)"
                            );
                            return (
                                StatusCode::CONFLICT,
                                Json(json!({
                                    "error": "card lifecycle has advanced since the prior scope_mismatch_closed; refusing idempotent close on a re-opened card",
                                    "card_id": body.card_id,
                                    "pending_dispatch_id": prior.dispatch_id,
                                    "reason": "lifecycle_generation_mismatch",
                                })),
                            );
                        }
                    }
                }

                let mut resumed_steps: Vec<&'static str> = Vec::new();
                if !card_is_terminal {
                    // Resume: cancel stale + transition + cleanup. We
                    // already verified lifecycle generation above, so the
                    // card is still the same generation we closed against.
                    tracing::warn!(
                        card_id = %body.card_id,
                        pending_rd_id = %prior.dispatch_id,
                        current_status = %current_status,
                        terminal_state = %terminal_state,
                        "[review-decision] #2341 resuming partial-close: dispatch was scope_mismatch_closed but card never reached terminal"
                    );

                    let stale_ids = stale_review_dispatch_ids_pg_first(&state, &body.card_id).await;
                    let mut cancelled_stale = 0usize;
                    for stale_id in &stale_ids {
                        if cancel_dispatch_pg_first(
                            &state,
                            stale_id,
                            Some("scope_mismatch_closed_resume"),
                        )
                        .await
                        .unwrap_or(0)
                            > 0
                        {
                            cancelled_stale += 1;
                        }
                    }
                    if cancelled_stale > 0 {
                        resumed_steps.push("cancelled_stale");
                    }

                    match transition_status_pg_first(
                        &state,
                        &body.card_id,
                        &terminal_state,
                        "dispute_scope_mismatch_closed_resume",
                        crate::engine::transition::ForceIntent::SystemRecovery,
                    )
                    .await
                    {
                        Ok(_) => {
                            resumed_steps.push("transition_terminal");
                        }
                        Err(e) => {
                            tracing::error!(
                                card_id = %body.card_id,
                                pending_rd_id = %prior.dispatch_id,
                                terminal_state = %terminal_state,
                                error = %e,
                                "[review-decision] #2341 resume failed to transition card to terminal"
                            );
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({
                                    "error": format!(
                                        "scope_mismatch_closed resume: card transition to {terminal_state} failed: {e}"
                                    ),
                                    "card_id": body.card_id,
                                    "pending_dispatch_id": prior.dispatch_id,
                                    "resumed_steps": resumed_steps,
                                })),
                            );
                        }
                    }

                    if let Err(error) = dismiss_review_cleanup_pg_first(&state, &body.card_id).await
                    {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({
                                "error": error,
                                "card_id": body.card_id,
                                "pending_dispatch_id": prior.dispatch_id,
                                "resumed_steps": resumed_steps,
                            })),
                        );
                    }
                    resumed_steps.push("dismiss_cleanup");

                    update_card_review_state(
                        review_state_db(&state),
                        state.pg_pool_ref(),
                        &body.card_id,
                        "dispute_scope_mismatch_closed",
                        Some(&prior.dispatch_id),
                    );

                    emit_card_updated(&state, &body.card_id).await;
                }

                tracing::info!(
                    card_id = %body.card_id,
                    pending_rd_id = %prior.dispatch_id,
                    card_was_terminal = card_is_terminal,
                    resumed_steps = ?resumed_steps,
                    "[review-decision] #2341 idempotent: returning 200 already_finalized for retried scope_mismatch_closed"
                );
                return (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "card_id": body.card_id,
                        "decision": "dispute",
                        "outcome": "scope_mismatch_closed",
                        "pending_dispatch_id": prior.dispatch_id,
                        "review_dispatch_id": prior.review_dispatch_id,
                        "reviewed_commit": prior.reviewed_commit,
                        "resumed": !card_is_terminal,
                        "resumed_steps": resumed_steps,
                        "message": if card_is_terminal {
                            "scope_mismatch_closed already finalized; idempotent no-op"
                        } else {
                            "scope_mismatch_closed resumed: card transitioned to terminal after prior partial close"
                        },
                    })),
                );
            }
        }
        // No pending review-decision dispatch → stale or duplicate call.
        // No dispatch_id to disambiguate either.
        //
        // #2200 sub-fix 1 (`stale-state`): when the originating review-decision
        // dispatch is missing because a follow-up (rework/review) or the
        // auto-accept policy already consumed it, idempotently short-circuit
        // instead of rejecting with 409 — but ONLY when:
        //   1. The caller supplied a `dispatch_id` that names the most-recent
        //      originating review-decision dispatch for this card (dispatch-
        //      scoped — closes the probing oracle described in Codex review).
        //      Callers without dispatch_id continue to see the legacy 409.
        //   2. The latest dispatch carries dispatch-scoped proof of the
        //      finalized decision (status + recognized completion_source +
        //      recorded decision). We never trust unscoped card-level
        //      `last_decision` alone (it can be stale from a prior round).
        //   3. The submitted decision matches the proven prior decision (a
        //      caller cannot flip a finalized decision by re-POSTing a
        //      different verdict — preserves legacy 409 for that case).

        // Without dispatch_id, return the generic legacy 409 — no card-
        // history-specific body shapes, no probing oracle.
        let Some(submitted_did) = body.dispatch_id.as_deref() else {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "no pending review-decision dispatch for this card",
                    "card_id": body.card_id,
                })),
            );
        };

        let finalized = finalized_review_decision_info_pg_first(&state, &body.card_id).await;

        // dispatch_id must match the latest originating review-decision
        // dispatch on file. Mismatch or no originating dispatch at all →
        // return the generic legacy 409 (no history disclosure).
        let matches_latest = finalized
            .latest_dispatch_id
            .as_deref()
            .is_some_and(|id| id == submitted_did);
        if !matches_latest {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "no pending review-decision dispatch for this card",
                    "card_id": body.card_id,
                })),
            );
        }

        if let Some(proven) = finalized.proven_finalized_decision() {
            if proven == body.decision.as_str() {
                tracing::info!(
                    card_id = %body.card_id,
                    submitted_decision = %body.decision,
                    latest_dispatch_id = ?finalized.latest_dispatch_id,
                    latest_dispatch_status = ?finalized.latest_dispatch_status,
                    review_state = ?finalized.review_state,
                    "[review-decision] #2200 stale-state: returning already_finalized for idempotent re-POST"
                );
                return (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "card_id": body.card_id,
                        "decision": body.decision,
                        "outcome": "already_finalized",
                        "message": "review-decision was already finalized; idempotent no-op",
                    })),
                );
            }
            tracing::warn!(
                card_id = %body.card_id,
                submitted_decision = %body.decision,
                proven_decision = %proven,
                "[review-decision] #2200 stale-state: rejecting decision-mismatch replay against finalized dispatch"
            );
        }

        // Originating dispatch matches but proof of finalization is missing
        // (e.g. status=failed, missing completion_source, or recorded decision
        // does not match). Return the legacy 409.
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "no pending review-decision dispatch for this card",
                "card_id": body.card_id,
            })),
        );
    }

    // #109: When dispatch_id is provided, validate it matches the pending
    // review-decision dispatch. This prevents replayed or stale decisions from
    // consuming a different dispatch than the one they were issued for.
    //
    // After #2200 sub-fix 4: if we just recovered `pending_rd_id` from the
    // submitted `dispatch_id` via `lookup_review_decision_dispatch_by_id`,
    // they are guaranteed equal — this branch is a no-op in that case but is
    // kept for the canonical "pending lookup populated it" path.
    if let Some(ref submitted_did) = body.dispatch_id {
        if pending_rd_id.as_deref() != Some(submitted_did.as_str()) {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "dispatch_id mismatch: submitted {} but pending is {}",
                        submitted_did,
                        pending_rd_id.as_deref().unwrap_or("(none)")
                    ),
                    "card_id": body.card_id,
                })),
            );
        }
    }
    match body.decision.as_str() {
        "accept" => {
            // #195: Agent accepts review feedback — create a rework dispatch so the
            // agent can address the findings. When the rework dispatch completes,
            // OnDispatchCompleted (kanban-rules.js) transitions to review for re-review.
            let card_ctx = load_review_decision_card_context_pg_first(&state, &body.card_id).await;
            let card_status_now = card_ctx.status.clone().unwrap_or_default();
            let card_repo_id = card_ctx.repo_id.clone();
            let card_agent_id = card_ctx.agent_id.clone();
            let card_title = card_ctx.title.clone();
            let effective_pipeline = resolve_effective_pipeline_pg_first(
                &state,
                card_repo_id.as_deref(),
                card_agent_id.as_deref(),
            )
            .await;

            // Guard: terminal card
            if effective_pipeline.is_terminal(&card_status_now) {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "card is terminal, cannot accept review feedback",
                        "card_id": body.card_id,
                    })),
                );
            }

            // Find rework target via review_rework gate (same logic as timeouts.js section E)
            let rework_target = effective_pipeline
                .transitions
                .iter()
                .find(|t| {
                    t.from == card_status_now
                        && t.transition_type == crate::pipeline::TransitionType::Gated
                        && t.gates.iter().any(|g| g == "review_rework")
                })
                .map(|t| t.to.clone())
                .unwrap_or_else(|| {
                    effective_pipeline
                        .dispatchable_states()
                        .first()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| effective_pipeline.initial_state().to_string())
                });

            // #246: Check if the agent already committed new work during the
            // review-decision turn. If the worktree HEAD differs from the
            // reviewed_commit of the last review, skip rework and go straight
            // to review (the agent already addressed the feedback).
            let skip_rework_diagnostics =
                evaluate_accept_skip_rework(&state, &body.card_id, submitted_commit.as_deref())
                    .await;
            let skip_rework = skip_rework_diagnostics.skip_rework;

            let mut accept_failures = Vec::new();
            let mut direct_review_auto_approved = false;

            // #246: If agent already committed new work, skip rework and re-enter
            // review via a two-step transition (rework_target → review) so that
            // OnReviewEnter fires naturally (increments review_round, sets
            // review_status, creates review dispatch via review-automation.js).
            let direct_review_attempted = skip_rework;
            let mut direct_review_created = if skip_rework {
                // Find the review state from the pipeline (gated transition from rework_target)
                let review_state = effective_pipeline
                    .transitions
                    .iter()
                    .find(|t| {
                        t.from == rework_target
                            && t.transition_type == crate::pipeline::TransitionType::Gated
                    })
                    .map(|t| t.to.clone());

                if let Some(ref review_st) = review_state {
                    if let Err(error) =
                        mark_next_review_round_advance_pg_first(&state, &body.card_id).await
                    {
                        accept_failures.push(format!(
                            "failed to mark review round advance before direct review: {error}"
                        ));
                        tracing::warn!(
                            "[review-decision] failed to mark direct-review round advance for card {}: {}",
                            body.card_id,
                            error
                        );
                    }
                    // Step 1: Transition to rework_target (e.g., in_progress)
                    match transition_status_pg_first(
                        &state,
                        &body.card_id,
                        &rework_target,
                        "review_decision_accept_skip_rework_step1",
                        crate::engine::transition::ForceIntent::SystemRecovery,
                    )
                    .await
                    {
                        Ok(_) => {
                            // Step 2: Transition to review — fires OnReviewEnter
                            match transition_status_pg_first(
                                &state,
                                &body.card_id,
                                review_st,
                                "review_decision_accept_skip_rework_step2",
                                crate::engine::transition::ForceIntent::SystemRecovery,
                            )
                            .await
                            {
                                Ok(_) => {
                                    // Materialize any follow-up transitions queued by
                                    // OnReviewEnter (for example, single-provider
                                    // auto-approval to terminal) before checking
                                    // whether a live review dispatch exists.
                                    crate::kanban::drain_hook_side_effects_with_backends(
                                        None,
                                        &state.engine,
                                    );
                                    let followups =
                                        active_accept_followups_pg_first(&state, &body.card_id)
                                            .await;
                                    if followups.review > 0 {
                                        tracing::info!(
                                            "[review-decision] #246 Direct review re-entry for card {}: {} → {} → {} (rework skipped)",
                                            body.card_id,
                                            card_status_now,
                                            rework_target,
                                            review_st
                                        );
                                        true
                                    } else if current_card_status_pg_first(&state, &body.card_id)
                                        .await
                                        .as_deref()
                                        .map(|status| effective_pipeline.is_terminal(status))
                                        .unwrap_or(false)
                                    {
                                        direct_review_auto_approved = true;
                                        tracing::info!(
                                            "[review-decision] #483 Direct review re-entry for card {} auto-approved without review dispatch (no alternate reviewer)",
                                            body.card_id
                                        );
                                        false
                                    } else {
                                        accept_failures.push(format!(
                                        "direct review transition reached {} but no active review dispatch was created",
                                        review_st
                                    ));
                                        tracing::warn!(
                                            "[review-decision] #339 Direct review re-entry for card {} reached {} but no active review dispatch exists",
                                            body.card_id,
                                            review_st
                                        );
                                        false
                                    }
                                }
                                Err(e) => {
                                    accept_failures.push(format!(
                                        "direct review step2 transition to {} failed: {e}",
                                        review_st
                                    ));
                                    tracing::warn!(
                                        "[review-decision] #246 Step 2 transition to {} failed for card {}: {e}",
                                        review_st,
                                        body.card_id
                                    );
                                    false
                                }
                            }
                        }
                        Err(e) => {
                            accept_failures.push(format!(
                                "direct review step1 transition to {} failed: {e}",
                                rework_target
                            ));
                            tracing::warn!(
                                "[review-decision] #339 Step 1 transition to {} failed for card {} during direct review: {e}",
                                rework_target,
                                body.card_id
                            );
                            false
                        }
                    }
                } else {
                    accept_failures.push(format!(
                        "skip_rework requested but no review state could be resolved from rework target {}",
                        rework_target
                    ));
                    false
                }
            } else {
                false
            };

            // Create rework dispatch on the normal accept path, or as a fallback when
            // direct review re-entry fails / produces no active review dispatch.
            if !direct_review_created && !direct_review_auto_approved {
                let card_status_before_rework =
                    current_card_status_pg_first(&state, &body.card_id).await;
                let rework_transition_ready = card_status_before_rework.as_deref()
                    == Some(rework_target.as_str())
                    || match transition_status_pg_first(
                        &state,
                        &body.card_id,
                        &rework_target,
                        "review_decision_accept",
                        crate::engine::transition::ForceIntent::SystemRecovery,
                    )
                    .await
                    {
                        Ok(_) => true,
                        Err(e) => {
                            accept_failures.push(format!(
                                "transition to rework target {} failed: {e}",
                                rework_target
                            ));
                            tracing::warn!(
                                "[review-decision] #195 Transition to rework target failed for card {}: {e}",
                                body.card_id
                            );
                            false
                        }
                    };

                if rework_transition_ready {
                    if let Some(ref agent_id) = card_agent_id {
                        let rework_title = format!(
                            "[Rework] {}",
                            card_title.as_deref().unwrap_or(&body.card_id)
                        );
                        let rework_dispatch_result = if let Some(pool) = state.pg_pool_ref() {
                            crate::dispatch::create_dispatch_with_options_pg_only(
                                pool,
                                &state.engine,
                                &body.card_id,
                                agent_id,
                                "rework",
                                &rework_title,
                                &json!({}),
                                crate::dispatch::DispatchCreateOptions::default(),
                            )
                        } else {
                            #[cfg(all(test, feature = "legacy-sqlite-tests"))]
                            {
                                state.legacy_db().map_or_else(
                                    || {
                                        Err(anyhow::anyhow!(
                                            "sqlite test backend unavailable for rework dispatch"
                                        ))
                                    },
                                    |db| {
                                        crate::dispatch::create_dispatch(
                                            db,
                                            &state.engine,
                                            &body.card_id,
                                            agent_id,
                                            "rework",
                                            &rework_title,
                                            &json!({}),
                                        )
                                    },
                                )
                            }
                            #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
                            {
                                Err(anyhow::anyhow!(
                                    "postgres pool unavailable for rework dispatch"
                                ))
                            }
                        };
                        match rework_dispatch_result {
                            Ok(dispatch) => {
                                let dispatch_id = dispatch
                                    .get("id")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("(unknown)");
                                tracing::info!(
                                    "[review-decision] #195 Rework dispatch created: card={} dispatch={}",
                                    body.card_id,
                                    dispatch_id
                                );
                            }
                            Err(e) => {
                                accept_failures
                                    .push(format!("rework dispatch creation failed: {e}"));
                                tracing::warn!(
                                    "[review-decision] #195 Rework dispatch creation failed for card {}: {e}",
                                    body.card_id
                                );
                            }
                        }
                    } else {
                        accept_failures.push(format!(
                            "no assigned agent for rework dispatch on card {}",
                            body.card_id
                        ));
                        tracing::warn!(
                            "[review-decision] #195 No agent assigned to card {} — cannot create rework dispatch",
                            body.card_id
                        );
                    }
                }
            }

            let followups = active_accept_followups_pg_first(&state, &body.card_id).await;
            direct_review_created = followups.review > 0;
            let rework_dispatch_created = followups.rework > 0;
            let terminal_auto_approved = direct_review_attempted
                && (direct_review_auto_approved
                    || (!direct_review_created
                        && !rework_dispatch_created
                        && current_card_status_pg_first(&state, &body.card_id)
                            .await
                            .as_deref()
                            .map(|status| effective_pipeline.is_terminal(status))
                            .unwrap_or(false)));

            if !followups.has_followup() && !terminal_auto_approved {
                let card_status_after = current_card_status_pg_first(&state, &body.card_id).await;
                tracing::error!(
                    card_id = %body.card_id,
                    pending_rd_id = pending_rd_id.as_deref().unwrap_or(""),
                    card_status_before = %card_status_now,
                    card_status_after = card_status_after.as_deref().unwrap_or("(unknown)"),
                    rework_target = %rework_target,
                    skip_rework,
                    direct_review_attempted,
                    direct_review_created,
                    rework_dispatch_created,
                    active_review = followups.review,
                    active_rework = followups.rework,
                    active_review_decision = followups.review_decision,
                    failures = ?accept_failures,
                    "[review-decision] #339 accept failed closed: no follow-up dispatch created"
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "review-decision accept failed: no follow-up dispatch created",
                        "card_id": body.card_id,
                        "pending_dispatch_id": pending_rd_id,
                        "skip_rework": skip_rework,
                        "skip_rework_diagnostics": skip_rework_diagnostics.to_json(),
                        "card_status_before": card_status_now,
                        "card_status_after": card_status_after,
                        "rework_target": rework_target,
                        "followups": {
                            "review": followups.review,
                            "rework": followups.rework,
                            "review_decision": followups.review_decision,
                        },
                        "failures": accept_failures,
                    })),
                );
            }

            if let Some(ref rd_id) = pending_rd_id {
                #[cfg(all(test, feature = "legacy-sqlite-tests"))]
                let status_db = state.legacy_db();
                #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
                let status_db = None;
                match crate::dispatch::set_dispatch_status_with_backends(
                    status_db,
                    state.pg_pool_ref(),
                    rd_id,
                    "completed",
                    Some(
                        &json!({"decision": "accept", "completion_source": "review_decision_api"}),
                    ),
                    "mark_dispatch_completed",
                    Some(&["pending", "dispatched"]),
                    true,
                ) {
                    Ok(1) => {}
                    Ok(_) => {
                        let dispatch_consumed_by_terminal_cleanup = terminal_auto_approved
                            && dispatch_status_and_result_pg_first(&state, rd_id)
                                .await
                                .map(|(status, result)| {
                                    if status == "completed" {
                                        return true;
                                    }
                                    if status != "cancelled" {
                                        return false;
                                    }
                                    result
                                        .as_deref()
                                        .and_then(|raw| {
                                            serde_json::from_str::<serde_json::Value>(raw).ok()
                                        })
                                        .and_then(|value| {
                                            value
                                                .get("reason")
                                                .and_then(|reason| reason.as_str())
                                                .map(str::to_string)
                                        })
                                        .as_deref()
                                        .is_some_and(|reason| {
                                            reason == "auto_cancelled_on_terminal_card"
                                                || reason == "js_terminal_cleanup"
                                        })
                                })
                                .unwrap_or(false);
                        let dispatch_no_longer_active = terminal_auto_approved
                            && active_accept_followups_pg_first(&state, &body.card_id)
                                .await
                                .review_decision
                                == 0;
                        if dispatch_consumed_by_terminal_cleanup || dispatch_no_longer_active {
                            tracing::info!(
                                "[review-decision] #483 pending review-decision {} for card {} was already consumed by terminal auto-approval",
                                rd_id,
                                body.card_id
                            );
                        } else {
                            let live_dispatches =
                                active_accept_followups_pg_first(&state, &body.card_id).await;
                            tracing::error!(
                                card_id = %body.card_id,
                                pending_rd_id = %rd_id,
                                active_review = live_dispatches.review,
                                active_rework = live_dispatches.rework,
                                active_review_decision = live_dispatches.review_decision,
                                "[review-decision] #339 accept created a follow-up dispatch but failed to finalize the pending review-decision"
                            );
                            return (
                                StatusCode::CONFLICT,
                                Json(json!({
                                    "error": "failed to finalize pending review-decision after follow-up dispatch creation",
                                    "card_id": body.card_id,
                                    "pending_dispatch_id": rd_id,
                                })),
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            card_id = %body.card_id,
                            pending_rd_id = %rd_id,
                            active_review = followups.review,
                            active_rework = followups.rework,
                            error = %e,
                            "[review-decision] #339 accept created a follow-up dispatch but mark_dispatch_completed errored"
                        );
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({
                                "error": format!("failed to finalize pending review-decision: {e}"),
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                            })),
                        );
                    }
                }
            };

            // Clear suggestion_pending_at (always) and review_status (rework path only).
            // #266: review_status was left as "suggestion_pending" because the
            // review→in_progress rework transition is non-terminal and
            // ClearTerminalFields never fires.
            // Guard: when direct_review_created, OnReviewEnter already set
            // review_status='reviewing' — clearing it would break the live review.
            finalize_accept_cleanup_pg_first(
                &state,
                &body.card_id,
                !direct_review_created && !terminal_auto_approved,
            )
            .await
            .ok();

            // #119: Record tuning outcome
            record_decision_tuning(
                state.pg_pool_ref(),
                &body.card_id,
                "accept",
                pending_rd_id.as_deref(),
            )
            .await;
            spawn_review_tuning_aggregate_pg_first(&state);

            // #117: Update canonical review state.
            // For direct review: OnReviewEnter already set the state, so skip the
            // rework_pending override that would conflict with the live review dispatch.
            if !direct_review_created && !terminal_auto_approved {
                update_card_review_state(
                    review_state_db(&state),
                    state.pg_pool_ref(),
                    &body.card_id,
                    "accept",
                    pending_rd_id.as_deref(),
                );
            }

            emit_card_updated(&state, &body.card_id).await;
            let message = if terminal_auto_approved {
                "Review-decision accepted, review auto-approved (no alternate reviewer)"
            } else if direct_review_created {
                "Review-decision accepted, direct review dispatch created (rework skipped)"
            } else {
                "Review-decision accepted, rework dispatch created"
            };
            return (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "card_id": body.card_id,
                    "decision": "accept",
                    "rework_dispatch_created": rework_dispatch_created,
                    "direct_review_created": direct_review_created,
                    "review_auto_approved": terminal_auto_approved,
                    "skip_rework": skip_rework,
                    "skip_rework_diagnostics": skip_rework_diagnostics.to_json(),
                    "message": message,
                })),
            );
        }
        "dispute" => {
            // #2341 / #2200 sub-3 redesign: out-of-scope dispute close path.
            //
            // Production reality (per #2341 Codex round-3): at /api/review-decision
            // time the review dispatch is **completed** (not active/pending).
            // PR #2336's close path bound to `latest_active_review_dispatch`
            // and therefore never fired in production. This redesign binds
            // to the latest **completed** review dispatch, verifies its
            // `reviewed_commit` is proven out-of-scope (fail-closed on
            // Unknown — carried forward from PR #2336 HIGH 1), captures a
            // card-lifecycle generation marker (HIGH 2 reworked to bind to
            // card lifecycle, not just dispatch existence), and runs the
            // finalize + cancel-stale + transition + cleanup sequence
            // atomically with a stale re-check inside the close
            // transaction.
            if body.out_of_scope == Some(true) {
                // 1. Caller must prove ownership of the pending review-decision
                //    dispatch via `dispatch_id` matching `pending_rd_id`.
                let rd_id = match (body.dispatch_id.as_deref(), pending_rd_id.as_deref()) {
                    (Some(submitted), Some(pending)) if submitted == pending => {
                        submitted.to_string()
                    }
                    (Some(submitted), Some(pending)) => {
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "error": format!(
                                    "dispatch_id mismatch: submitted {submitted} but pending is {pending}"
                                ),
                                "card_id": body.card_id,
                            })),
                        );
                    }
                    (None, _) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({
                                "error": "out_of_scope dispute requires dispatch_id to prove ownership of the pending review-decision",
                                "card_id": body.card_id,
                            })),
                        );
                    }
                    (Some(_), None) => {
                        // Already guarded above; defensive only.
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "error": "no pending review-decision dispatch for this card",
                                "card_id": body.card_id,
                            })),
                        );
                    }
                };

                // 2. Bind to the **source** review dispatch that produced THIS
                //    review-decision (loaded by id from the review-decision's
                //    `context.source_review_dispatch_id`), not to the latest
                //    completed review for the card. This closes Codex r1 [medium]:
                //    a duplicate or delayed completed review row could otherwise
                //    bind the close to the wrong reviewed_commit.
                //    Codex r2 [medium]: if the source id is present but does
                //    not resolve, fail closed — no silent latest-completed
                //    fallback.
                let completed_review = match source_review_dispatch_for_decision_pg_first(
                    &state,
                    &body.card_id,
                    &rd_id,
                )
                .await
                {
                    SourceReviewLookup::ResolvedById(d) => d,
                    SourceReviewLookup::LegacyFallback(Some(d)) => d,
                    SourceReviewLookup::LegacyFallback(None) => {
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "error": "out_of_scope dispute requires a completed review dispatch whose reviewed_commit can be verified against the card issue",
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                            })),
                        );
                    }
                    SourceReviewLookup::UnresolvedSourceId(srid) => {
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "error": "out_of_scope dispute refused: review-decision context references a source review that does not resolve to a completed review row; cannot verify scope",
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                                "source_review_dispatch_id": srid,
                                "reason": "source_review_unresolved",
                            })),
                        );
                    }
                };
                let reviewed_commit = match completed_review.reviewed_commit.clone() {
                    Some(c) if !c.trim().is_empty() => c,
                    _ => {
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "error": "out_of_scope dispute requires the completed review to expose reviewed_commit for scope verification",
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                                "review_dispatch_id": completed_review.id,
                            })),
                        );
                    }
                };

                // 3. HIGH 1 fail-closed: tri-state scope verification. Only a
                //    proven OutOfScope is allowed to take the close shortcut.
                //    Unknown — transient PG/git failure — refuses with 503.
                match commit_belongs_to_card_issue_pg_first_tri(
                    &state,
                    &body.card_id,
                    &reviewed_commit,
                    completed_review.target_repo.as_deref(),
                )
                .await
                {
                    crate::dispatch::ScopeCheck::OutOfScope => {}
                    crate::dispatch::ScopeCheck::InScope => {
                        tracing::warn!(
                            card_id = %body.card_id,
                            pending_rd_id = %rd_id,
                            review_dispatch_id = %completed_review.id,
                            reviewed_commit = %reviewed_commit,
                            "[review-decision] #2341 rejected out_of_scope claim: reviewed_commit belongs to the card issue"
                        );
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "error": "out_of_scope dispute refused: reviewed_commit belongs to this card's issue; submit a regular dispute instead",
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                                "review_dispatch_id": completed_review.id,
                                "reviewed_commit": reviewed_commit,
                            })),
                        );
                    }
                    crate::dispatch::ScopeCheck::Unknown => {
                        tracing::warn!(
                            card_id = %body.card_id,
                            pending_rd_id = %rd_id,
                            review_dispatch_id = %completed_review.id,
                            reviewed_commit = %reviewed_commit,
                            target_repo = %completed_review.target_repo.as_deref().unwrap_or(""),
                            "[review-decision] #2341 refused out_of_scope: scope verification inconclusive (repo/git transient failure); fail-closed"
                        );
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({
                                "error": "scope verification inconclusive; cannot close as out-of-scope. Retry once the repo is reachable, or submit a regular dispute.",
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                                "review_dispatch_id": completed_review.id,
                                "reviewed_commit": reviewed_commit,
                                "reason": "scope_check_unknown",
                            })),
                        );
                    }
                }

                // 4. Capture the card lifecycle generation snapshot. The
                //    atomic close re-reads this inside its transaction and
                //    rolls back if it has changed (= card re-opened between
                //    snapshot and tx).
                let lifecycle_snapshot =
                    card_lifecycle_snapshot_pg_first(&state, &body.card_id).await;

                // 5. Atomic finalize: in one tx, re-check lifecycle + flip
                //    the review-decision dispatch to completed +
                //    scope_mismatch_closed + update card_review_state.
                match atomic_finalize_scope_mismatch_close_pg(
                    &state,
                    &body.card_id,
                    &rd_id,
                    &completed_review.id,
                    &reviewed_commit,
                    &lifecycle_snapshot,
                )
                .await
                {
                    Ok(_) => {}
                    Err(ScopeMismatchCloseError::LifecycleStale { expected, actual }) => {
                        tracing::warn!(
                            card_id = %body.card_id,
                            pending_rd_id = %rd_id,
                            review_dispatch_id = %completed_review.id,
                            ?expected,
                            ?actual,
                            "[review-decision] #2341 refused out_of_scope close: card lifecycle generation changed (card re-opened)"
                        );
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "error": "card lifecycle has advanced since the completed review; refusing to close as out-of-scope",
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                                "review_dispatch_id": completed_review.id,
                                "reason": "lifecycle_generation_mismatch",
                            })),
                        );
                    }
                    Err(ScopeMismatchCloseError::DispatchConsumed) => {
                        tracing::warn!(
                            card_id = %body.card_id,
                            pending_rd_id = %rd_id,
                            "[review-decision] #2341 race: pending review-decision dispatch was already consumed before scope_mismatch_closed could finalize it"
                        );
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "error": "race: pending review-decision dispatch was already consumed",
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                            })),
                        );
                    }
                    Err(ScopeMismatchCloseError::Internal(e)) => {
                        tracing::error!(
                            card_id = %body.card_id,
                            pending_rd_id = %rd_id,
                            error = %e,
                            "[review-decision] #2341 atomic finalize failed for scope_mismatch_closed"
                        );
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({
                                "error": format!(
                                    "failed to atomically finalize scope_mismatch_closed: {e}"
                                ),
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                            })),
                        );
                    }
                }

                // 6. Re-check lifecycle BEFORE any destructive action.
                //    Codex round-2 [high]: cancel-stale must NOT run before
                //    the lifecycle re-check, otherwise a re-open that
                //    happened between the tx commit and this point would
                //    have its fresh review dispatch cancelled by
                //    stale_review_dispatch_ids_pg_first before we discover
                //    the re-open and refuse. Re-checking first means the
                //    409 refusal triggers before any side effects.
                let post_tx_lifecycle =
                    card_lifecycle_snapshot_pg_first(&state, &body.card_id).await;
                if post_tx_lifecycle != lifecycle_snapshot {
                    tracing::warn!(
                        card_id = %body.card_id,
                        pending_rd_id = %rd_id,
                        ?lifecycle_snapshot,
                        ?post_tx_lifecycle,
                        "[review-decision] #2341 lifecycle changed after tx commit but before cleanup; leaving dispatch finalized for idempotent resume, no destructive actions taken"
                    );
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({
                            "error": "card lifecycle advanced after scope_mismatch_closed finalize; transition refused",
                            "card_id": body.card_id,
                            "pending_dispatch_id": rd_id,
                            "review_dispatch_id": completed_review.id,
                            "reason": "lifecycle_generation_mismatch_post_tx",
                        })),
                    );
                }

                // 7. Cancel stale review/review-decision dispatches so the
                //    dedup guard doesn't strand them. Outside the tx because
                //    cancel touches multiple rows + may dispatch outbox
                //    messages. Safe to run now: the post-tx lifecycle
                //    re-check above guaranteed no fresh generation exists.
                let stale_ids = stale_review_dispatch_ids_pg_first(&state, &body.card_id).await;
                let mut cancelled_stale = 0usize;
                for stale_id in &stale_ids {
                    if cancel_dispatch_pg_first(&state, stale_id, Some("scope_mismatch_closed"))
                        .await
                        .unwrap_or(0)
                        > 0
                    {
                        cancelled_stale += 1;
                    }
                }

                let card_ctx =
                    load_review_decision_card_context_pg_first(&state, &body.card_id).await;
                let effective_pipeline = resolve_effective_pipeline_pg_first(
                    &state,
                    card_ctx.repo_id.as_deref(),
                    card_ctx.agent_id.as_deref(),
                )
                .await;
                let terminal_state = effective_pipeline
                    .states
                    .iter()
                    .find(|state| state.terminal)
                    .map(|state| state.id.clone())
                    .unwrap_or_else(|| "done".to_string());
                match transition_status_pg_first(
                    &state,
                    &body.card_id,
                    &terminal_state,
                    "dispute_scope_mismatch_closed",
                    crate::engine::transition::ForceIntent::SystemRecovery,
                )
                .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!(
                            card_id = %body.card_id,
                            pending_rd_id = %rd_id,
                            terminal_state = %terminal_state,
                            error = %e,
                            "[review-decision] #2341 finalized review-decision but failed to transition card to terminal"
                        );
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({
                                "error": format!(
                                    "scope_mismatch_closed: review-decision finalized but card transition to {terminal_state} failed: {e}"
                                ),
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                            })),
                        );
                    }
                }

                // 8. Reuse dismiss cleanup to clear any leftover pending
                //    review dispatches and review_status.
                if let Err(error) = dismiss_review_cleanup_pg_first(&state, &body.card_id).await {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": error})),
                    );
                }

                record_decision_tuning(
                    state.pg_pool_ref(),
                    &body.card_id,
                    "dispute_scope_mismatch_closed",
                    Some(&rd_id),
                )
                .await;
                spawn_review_tuning_aggregate_pg_first(&state);

                emit_card_updated(&state, &body.card_id).await;
                tracing::info!(
                    card_id = %body.card_id,
                    pending_rd_id = %rd_id,
                    review_dispatch_id = %completed_review.id,
                    reviewed_commit = %reviewed_commit,
                    cancelled_stale,
                    "[review-decision] #2341 closed dispute as scope_mismatch_closed (completed-review-context binding)"
                );
                return (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "card_id": body.card_id,
                        "decision": "dispute",
                        "outcome": "scope_mismatch_closed",
                        "pending_dispatch_id": rd_id,
                        "review_dispatch_id": completed_review.id,
                        "reviewed_commit": reviewed_commit,
                        "cancelled_stale_dispatches": cancelled_stale,
                        "message": "Dispute closed: completed review verified as out-of-scope for this card",
                    })),
                );
            }

            if let Err(error) = prepare_dispute_review_entry_pg_first(&state, &body.card_id).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }

            // #119: Record tuning outcome BEFORE OnReviewEnter (which increments review_round)
            record_decision_tuning(
                state.pg_pool_ref(),
                &body.card_id,
                "dispute",
                pending_rd_id.as_deref(),
            )
            .await;
            spawn_review_tuning_aggregate_pg_first(&state);

            // #229: Cancel stale pending/dispatched review dispatches for this card.
            // Without this, the dispatch-core dedup guard blocks
            // OnReviewEnter from creating a fresh review dispatch after dispute.
            let stale_ids = stale_review_dispatch_ids_pg_first(&state, &body.card_id).await;
            let mut cancelled = 0usize;
            for stale_id in &stale_ids {
                if cancel_dispatch_pg_first(
                    &state,
                    stale_id,
                    Some("superseded_by_dispute_re_review"),
                )
                .await
                .unwrap_or(0)
                    > 0
                {
                    cancelled += 1;
                }
            }
            if cancelled > 0 {
                tracing::info!(
                    "[review-decision] #229 Cancelled {} stale review dispatch(es) for card {} before dispute re-review",
                    cancelled,
                    body.card_id
                );
            }

            // Fire on_enter hooks for current state (should be a review-like state with OnReviewEnter)
            let dispute_status = current_card_status_pg_first(&state, &body.card_id)
                .await
                .unwrap_or_else(|| "review".to_string());
            crate::kanban::fire_enter_hooks_with_backends(
                None,
                &state.engine,
                &body.card_id,
                &dispute_status,
            );

            // #108: Drain all pending intents and transitions from OnReviewEnter hooks.
            // drain_hook_side_effects handles both transition processing (e.g. setStatus
            // for review/manual-intervention follow-up on max rounds) and Discord notifications for any
            // dispatches created by the hooks, eliminating the previous manual drain loop
            // that only handled transitions and missed dispatch notifications.
            crate::kanban::drain_hook_side_effects_with_backends(None, &state.engine);

            // #229: Safety net — if card is still in a review-like state but no
            // pending review dispatch exists (OnReviewEnter hook may have failed
            // due to lock contention or JS error), re-fire with blocking lock.
            {
                let card_ctx =
                    load_review_decision_card_context_pg_first(&state, &body.card_id).await;
                let has_review_dispatch = if let Some(pool) = state.pg_pool_ref() {
                    sqlx::query_scalar::<_, bool>(
                        "SELECT COUNT(*) > 0
                         FROM task_dispatches
                         WHERE kanban_card_id = $1
                           AND dispatch_type IN ('review', 'review-decision')
                           AND status IN ('pending', 'dispatched')",
                    )
                    .bind(&body.card_id)
                    .fetch_one(pool)
                    .await
                    .unwrap_or(false)
                } else {
                    false
                };
                let effective_pipeline = resolve_effective_pipeline_pg_first(
                    &state,
                    card_ctx.repo_id.as_deref(),
                    card_ctx.agent_id.as_deref(),
                )
                .await;
                let needs_review = card_ctx.status.as_deref().is_some_and(|status| {
                    effective_pipeline
                        .hooks_for_state(status)
                        .is_some_and(|hooks| {
                            hooks.on_enter.iter().any(|name| name == "OnReviewEnter")
                        })
                }) && !has_review_dispatch;

                if needs_review {
                    tracing::warn!(
                        "[review-decision] Card {} in review state but no review dispatch after dispute — re-firing OnReviewEnter (#229)",
                        body.card_id
                    );
                    let _ = state.engine.fire_hook_by_name_blocking(
                        "OnReviewEnter",
                        json!({ "card_id": body.card_id }),
                    );
                    crate::kanban::drain_hook_side_effects_with_backends(None, &state.engine);
                }
            }

            let live_review = match latest_active_review_dispatch_pg_first(&state, &body.card_id)
                .await
            {
                Some(dispatch) => dispatch,
                None => {
                    tracing::error!(
                        card_id = %body.card_id,
                        pending_rd_id = pending_rd_id.as_deref().unwrap_or(""),
                        "[review-decision] #491 dispute failed closed: no live review dispatch after re-review entry"
                    );
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({
                            "error": "review-decision dispute failed: no follow-up review dispatch created",
                            "card_id": body.card_id,
                            "pending_dispatch_id": pending_rd_id,
                        })),
                    );
                }
            };

            if let Some(ref reviewed_commit) = live_review.reviewed_commit {
                if !commit_belongs_to_card_issue_pg_first(
                    &state,
                    &body.card_id,
                    reviewed_commit,
                    live_review.target_repo.as_deref(),
                )
                .await
                {
                    let _ = cancel_dispatch_pg_first(
                        &state,
                        &live_review.id,
                        Some("invalid_dispute_rereview_target"),
                    )
                    .await;
                    tracing::error!(
                        card_id = %body.card_id,
                        pending_rd_id = pending_rd_id.as_deref().unwrap_or(""),
                        review_dispatch_id = %live_review.id,
                        reviewed_commit = %reviewed_commit,
                        "[review-decision] #491 dispute failed closed: re-review target does not belong to the card issue"
                    );
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({
                            "error": "review-decision dispute failed: re-review target is stale or unrelated to the card issue",
                            "card_id": body.card_id,
                            "pending_dispatch_id": pending_rd_id,
                            "review_dispatch_id": live_review.id,
                            "reviewed_commit": reviewed_commit,
                        })),
                    );
                }
            }

            if let Some(ref rd_id) = pending_rd_id {
                #[cfg(all(test, feature = "legacy-sqlite-tests"))]
                let status_db = state.legacy_db();
                #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
                let status_db = None;
                match crate::dispatch::set_dispatch_status_with_backends(
                    status_db,
                    state.pg_pool_ref(),
                    rd_id,
                    "completed",
                    Some(
                        &json!({"decision": "dispute", "completion_source": "review_decision_api"}),
                    ),
                    "mark_dispatch_completed",
                    Some(&["pending", "dispatched"]),
                    true,
                ) {
                    Ok(1) => {}
                    Ok(_) => {
                        tracing::error!(
                            card_id = %body.card_id,
                            pending_rd_id = %rd_id,
                            review_dispatch_id = %live_review.id,
                            "[review-decision] #491 dispute created a follow-up review dispatch but failed to finalize the pending review-decision"
                        );
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "error": "failed to finalize pending review-decision after re-review dispatch creation",
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                                "review_dispatch_id": live_review.id,
                            })),
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            card_id = %body.card_id,
                            pending_rd_id = %rd_id,
                            review_dispatch_id = %live_review.id,
                            error = %e,
                            "[review-decision] #491 dispute created a follow-up review dispatch but mark_dispatch_completed errored"
                        );
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({
                                "error": format!("failed to finalize pending review-decision: {e}"),
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                                "review_dispatch_id": live_review.id,
                            })),
                        );
                    }
                }
            }

            // #117: Update canonical review state before returning
            update_card_review_state(
                review_state_db(&state),
                state.pg_pool_ref(),
                &body.card_id,
                "dispute",
                pending_rd_id.as_deref(),
            );

            emit_card_updated(&state, &body.card_id).await;
            return (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "card_id": body.card_id,
                    "decision": "dispute",
                    "review_dispatch_id": live_review.id,
                    "reviewed_commit": live_review.reviewed_commit,
                    "message": "Re-review dispatched to counter-model",
                })),
            );
        }
        "dismiss" => {
            // Agent dismisses review → transition to terminal state, then clean up stale state.
            // Order matters: transition_status requires an active dispatch, so we must
            // transition BEFORE cancelling pending dispatches.
            let card_ctx = load_review_decision_card_context_pg_first(&state, &body.card_id).await;
            let effective_pipeline = resolve_effective_pipeline_pg_first(
                &state,
                card_ctx.repo_id.as_deref(),
                card_ctx.agent_id.as_deref(),
            )
            .await;
            let terminal_state = effective_pipeline
                .states
                .iter()
                .find(|state| state.terminal)
                .map(|state| state.id.clone())
                .unwrap_or_else(|| "done".to_string());
            let _ = transition_status_pg_first(
                &state,
                &body.card_id,
                &terminal_state,
                "dismiss",
                crate::engine::transition::ForceIntent::SystemRecovery, // dismiss bypasses review_passed gate
            )
            .await;

            // Post-transition cleanup: cancel remaining pending review dispatches to prevent
            // stale dispatches from re-triggering review loops after dismiss.
            if let Err(error) = dismiss_review_cleanup_pg_first(&state, &body.card_id).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }
        _ => {}
    }

    // #117: Update canonical review state for all decision paths
    update_card_review_state(
        review_state_db(&state),
        state.pg_pool_ref(),
        &body.card_id,
        &body.decision,
        pending_rd_id.as_deref(),
    );
    // #119: Record tuning outcome (dismiss falls through here; accept/dispute call helper before returning)
    record_decision_tuning(
        state.pg_pool_ref(),
        &body.card_id,
        &body.decision,
        pending_rd_id.as_deref(),
    )
    .await;
    spawn_review_tuning_aggregate_pg_first(&state);

    emit_card_updated(&state, &body.card_id).await;

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "card_id": body.card_id,
            "decision": body.decision,
        })),
    )
}
