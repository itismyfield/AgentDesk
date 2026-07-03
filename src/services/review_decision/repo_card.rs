//! #3038 decision_route decomposition: kanban-card / review-state PG reads,
//! commit normalization, and the scope-mismatch atomic close transaction.
//! Function bodies are verbatim moves from the former `decision_route.rs`
//! monolith (see the module root for the slice map).

use serde::Deserialize;
use serde_json::json;

use crate::app_state::AppState;

use super::worktree_stale::{
    commit_is_on_remote_mainline, context_repo_dir, current_issue_worktree_target,
    latest_completed_review_context_pg_first,
};

/// PG-only wrapper for kanban transitions after #1384.
pub(super) async fn transition_status_pg_first(
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

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct ActiveAcceptFollowups {
    pub(super) review: i64,
    pub(super) rework: i64,
    pub(super) review_decision: i64,
}

impl ActiveAcceptFollowups {
    pub(super) fn has_followup(self) -> bool {
        self.review > 0 || self.rework > 0
    }
}

pub(super) async fn active_accept_followups_pg_first(
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

    ActiveAcceptFollowups::default()
}

pub(super) async fn restamp_latest_active_review_target_pg_first(
    state: &AppState,
    card_id: &str,
    reviewed_commit: &str,
    target_repo: Option<&str>,
) -> Result<bool, String> {
    let Some(pool) = state.pg_pool_ref() else {
        return Ok(false);
    };
    let mut context_patch = json!({
        "reviewed_commit": reviewed_commit,
        "direct_review_target_source": "review_decision_accept",
    });
    if let Some(target_repo) = target_repo
        && !target_repo.trim().is_empty()
    {
        context_patch["target_repo"] = json!(target_repo);
    }
    let rows = sqlx::query(
        "WITH latest AS (
             SELECT id
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status IN ('pending', 'dispatched')
             ORDER BY updated_at DESC NULLS LAST, created_at DESC NULLS LAST
             LIMIT 1
         )
         UPDATE task_dispatches td
         SET context = COALESCE(NULLIF(td.context, ''), '{}')::jsonb || $2::jsonb,
             updated_at = NOW()
         FROM latest
         WHERE td.id = latest.id",
    )
    .bind(card_id)
    .bind(context_patch.to_string())
    .execute(pool)
    .await
    .map_err(|error| {
        format!("restamp active review target for card {card_id} commit {reviewed_commit}: {error}")
    })?
    .rows_affected();
    Ok(rows > 0)
}

pub(super) async fn current_card_status_pg_first(
    state: &AppState,
    card_id: &str,
) -> Option<String> {
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

    None
}

#[derive(Debug, Default)]
pub(super) struct ReviewDecisionCardContext {
    pub(super) status: Option<String>,
    pub(super) repo_id: Option<String>,
    pub(super) agent_id: Option<String>,
    pub(super) title: Option<String>,
}

pub(super) async fn load_review_decision_card_context_pg_first(
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

    ReviewDecisionCardContext::default()
}

pub(super) async fn resolve_effective_pipeline_pg_first(
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

pub(super) async fn card_exists_pg_first(state: &AppState, card_id: &str) -> bool {
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

    false
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
pub(super) struct CardLifecycleSnapshot {
    pub(super) latest_dispatch_id: Option<String>,
    pub(super) review_round: Option<i64>,
    pub(super) review_entered_at_iso: Option<String>,
}

pub(super) async fn card_lifecycle_snapshot_pg_first(
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
        let review_fields: Option<(Option<i64>, Option<chrono::DateTime<chrono::Utc>>)> =
            sqlx::query_as::<_, (Option<i64>, Option<chrono::DateTime<chrono::Utc>>)>(
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

    CardLifecycleSnapshot::default()
}

/// #2341 / #2200 sub-3 redesign: PG-tri-state scope check delegating to the
/// dispatch-context helper. Returns `Unknown` when no PG pool is wired in,
/// which the caller must treat as a refusal on the out-of-scope close path.
pub(super) async fn commit_belongs_to_card_issue_pg_first_tri(
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

    crate::dispatch::ScopeCheck::Unknown
}

/// #2341 / #2200 sub-3 redesign: detect a prior `scope_mismatch_closed`
/// finalize for idempotent re-POST handling. Composes with sub-fix 1's
/// proof-of-finalization model — the originating review-decision dispatch
/// carries `result.outcome = scope_mismatch_closed`, which is sufficient
/// proof that a prior call already finalized the close.
#[derive(Debug, Clone)]
pub(super) struct PriorScopeMismatchClose {
    pub(super) dispatch_id: String,
    pub(super) review_dispatch_id: Option<String>,
    pub(super) reviewed_commit: Option<String>,
    pub(super) lifecycle_generation: Option<CardLifecycleSnapshot>,
}

pub(super) async fn recent_scope_mismatch_finalized_pg_first(
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
/// If PostgreSQL is unavailable this operation fails closed; the retired
/// SQLite-era fallback is not a supported close path.
pub(super) async fn atomic_finalize_scope_mismatch_close_pg(
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
        let review_fields: Option<(Option<i64>, Option<chrono::DateTime<chrono::Utc>>)> =
            sqlx::query_as::<_, (Option<i64>, Option<chrono::DateTime<chrono::Utc>>)>(
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

    // No PG pool: fail closed. This path needs row locks and the PG
    // transaction is the only supported implementation.

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
pub(super) enum ScopeMismatchCloseError {
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
            review_round: Option<i64>,
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

pub(super) async fn card_issue_number_pg_first(state: &AppState, card_id: &str) -> Option<i64> {
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

    None
}

#[derive(Debug, Clone)]
pub(super) struct SkipReworkDiagnostics {
    pub(super) skip_rework: bool,
    pub(super) last_reviewed_commit: Option<String>,
    pub(super) current_commit: Option<String>,
    pub(super) current_commit_source: Option<&'static str>,
    pub(super) current_commit_remote_visible: Option<bool>,
    pub(super) current_commit_repo: Option<String>,
    pub(super) issue_number: Option<i64>,
    pub(super) reason: &'static str,
}

impl SkipReworkDiagnostics {
    pub(super) fn to_json(&self) -> serde_json::Value {
        json!({
            "skip_rework": self.skip_rework,
            "last_reviewed_commit": self.last_reviewed_commit.as_deref(),
            "current_commit": self.current_commit.as_deref(),
            "current_commit_source": self.current_commit_source,
            "current_commit_remote_visible": self.current_commit_remote_visible,
            "current_commit_repo": self.current_commit_repo.as_deref(),
            "issue_number": self.issue_number,
            "reason": self.reason,
        })
    }
}

pub(super) fn normalize_optional_commit_sha(
    commit_sha: Option<&str>,
) -> Result<Option<String>, String> {
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

pub(super) async fn evaluate_accept_skip_rework(
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

    let mut current_commit_repo = context_repo_dir(last_review_context.as_ref());
    let (current_commit, current_commit_source) = if let Some(submitted_commit) = submitted_commit {
        (Some(submitted_commit.to_string()), Some("request"))
    } else if last_reviewed_commit.is_some() {
        if let Some(issue_num) = issue_number {
            let target = current_issue_worktree_target(
                state.engine.pg_pool(),
                card_id,
                issue_num,
                last_review_context.as_ref(),
            )
            .await;
            if current_commit_repo.is_none() {
                current_commit_repo = target.as_ref().map(|target| target.worktree_path.clone());
            }
            (target.map(|target| target.commit), Some("worktree"))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };
    let current_commit_remote_visible = current_commit.as_deref().and_then(|commit| {
        current_commit_repo
            .as_deref()
            .and_then(|repo| commit_is_on_remote_mainline(repo, commit))
    });

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
        current_commit_remote_visible,
        current_commit_repo,
        issue_number,
        reason,
    };

    tracing::info!(
        card_id = %card_id,
        skip_rework = diagnostics.skip_rework,
        last_reviewed_commit = diagnostics.last_reviewed_commit.as_deref().unwrap_or(""),
        current_commit = diagnostics.current_commit.as_deref().unwrap_or(""),
        current_commit_source = diagnostics.current_commit_source.unwrap_or(""),
        current_commit_remote_visible = ?diagnostics.current_commit_remote_visible,
        issue_number = ?diagnostics.issue_number,
        reason = diagnostics.reason,
        "[review-decision] #1977 evaluated accept skip_rework"
    );

    diagnostics
}

// #3038 characterization tests (moved with their functions from the monolith).
#[cfg(test)]
mod tests {
    use super::*;

    // ── normalize_optional_commit_sha ────────────────────────────────────

    #[test]
    fn normalize_commit_sha_absent_is_ok_none() {
        assert_eq!(normalize_optional_commit_sha(None), Ok(None));
    }

    #[test]
    fn normalize_commit_sha_empty_is_rejected() {
        let error = normalize_optional_commit_sha(Some("")).unwrap_err();
        assert_eq!(error, "commit_sha must not be empty when provided");
    }

    #[test]
    fn normalize_commit_sha_whitespace_only_is_rejected() {
        let error = normalize_optional_commit_sha(Some("   \t ")).unwrap_err();
        assert_eq!(error, "commit_sha must not be empty when provided");
    }

    #[test]
    fn normalize_commit_sha_too_short_is_rejected() {
        let error = normalize_optional_commit_sha(Some("abc123")).unwrap_err();
        assert_eq!(
            error,
            "commit_sha must be a 7-64 character hex git commit SHA"
        );
    }

    #[test]
    fn normalize_commit_sha_minimum_seven_hex_is_accepted() {
        assert_eq!(
            normalize_optional_commit_sha(Some("abc1234")),
            Ok(Some("abc1234".to_string()))
        );
    }

    #[test]
    fn normalize_commit_sha_full_forty_hex_is_lowercased() {
        let upper = "A".repeat(40);
        assert_eq!(
            normalize_optional_commit_sha(Some(upper.as_str())),
            Ok(Some("a".repeat(40)))
        );
    }

    #[test]
    fn normalize_commit_sha_sixty_four_hex_is_accepted() {
        let sha = "f".repeat(64);
        assert_eq!(
            normalize_optional_commit_sha(Some(sha.as_str())),
            Ok(Some(sha.clone()))
        );
    }

    #[test]
    fn normalize_commit_sha_sixty_five_hex_is_rejected() {
        let sha = "f".repeat(65);
        let error = normalize_optional_commit_sha(Some(sha.as_str())).unwrap_err();
        assert_eq!(
            error,
            "commit_sha must be a 7-64 character hex git commit SHA"
        );
    }

    #[test]
    fn normalize_commit_sha_non_hex_is_rejected() {
        let error = normalize_optional_commit_sha(Some("xyz1234")).unwrap_err();
        assert_eq!(
            error,
            "commit_sha must be a 7-64 character hex git commit SHA"
        );
    }

    #[test]
    fn normalize_commit_sha_is_trimmed_before_validation() {
        assert_eq!(
            normalize_optional_commit_sha(Some("  ABC1234  ")),
            Ok(Some("abc1234".to_string()))
        );
    }

    // ── commit_sha_differs ───────────────────────────────────────────────

    #[test]
    fn commit_sha_differs_identical_is_false() {
        assert!(!commit_sha_differs("abc1234", "abc1234"));
    }

    #[test]
    fn commit_sha_differs_is_case_insensitive() {
        assert!(!commit_sha_differs("ABC1234", "abc1234"));
        assert!(!commit_sha_differs("abc1234", "ABC1234"));
    }

    #[test]
    fn commit_sha_differs_distinct_is_true() {
        assert!(commit_sha_differs("abc1234", "def5678"));
    }

    #[test]
    fn commit_sha_differs_prefix_is_true() {
        // Full equality only — a short/long prefix pair counts as different.
        assert!(commit_sha_differs("abc1234", "abc1234def5678"));
    }
}
