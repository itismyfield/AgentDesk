use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

use crate::{
    error::{AppError, AppResult, ErrorCode},
    services as services_layer,
};

use super::AppState;
// #3863: reuse the verdict route's hardened SHA guard so the recovery route
// cannot record a forged `reviewed_commit` marker for an arbitrary commit.
use super::review_verdict::is_valid_commit_sha;

// ── Body types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DecisionItem {
    pub item_id: i64,
    pub decision: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDecisionsBody {
    pub decisions: Vec<DecisionItem>,
}

#[derive(Debug, Deserialize)]
pub struct ReviewTargetRecoveryBody {
    pub dispatch_id: Option<String>,
    pub card_id: Option<String>,
    pub target_commit: Option<String>,
    pub worktree_path: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug)]
struct ReviewRecoveryDispatch {
    id: String,
    card_id: String,
    status: String,
    context: serde_json::Value,
}

fn validate_review_decision(decision: &str) -> bool {
    decision == "accept" || decision == "reject"
}

/// #3863: reject a non-SHA recovery `target_commit` before it is persisted as the
/// `reviewed_commit` deploy-gate marker. Pure (no I/O) so the recovery route can
/// reject as early as possible — before any Postgres transaction or marker
/// write — and so the guard is unit-testable without a live database. Shares the
/// `is_valid_commit_sha` predicate with the verdict route (single source of
/// truth), keeping the same HTTP 400 error shape for an invalid commit.
fn validate_recovery_target_commit(commit: &str) -> Result<(), (StatusCode, String)> {
    if is_valid_commit_sha(commit) {
        Ok(())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            "target_commit must be a lowercase git SHA matching ^[0-9a-f]{7,64}$".to_string(),
        ))
    }
}

fn trimmed_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn json_object_from_text(
    raw: Option<String>,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(serde_json::Map::new());
    };
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| format!("dispatch context is not valid JSON: {error}"))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "dispatch context must be a JSON object".to_string())
}

async fn load_review_recovery_dispatch_pg(
    pool: &sqlx::PgPool,
    dispatch_id: Option<&str>,
    card_id: Option<&str>,
) -> Result<Option<ReviewRecoveryDispatch>, String> {
    let row = if let Some(dispatch_id) = dispatch_id {
        sqlx::query(
            "SELECT id, kanban_card_id, dispatch_type, status, context
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load review dispatch {dispatch_id}: {error}"))?
    } else if let Some(card_id) = card_id {
        sqlx::query(
            "SELECT id, kanban_card_id, dispatch_type, status, context
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status IN ('pending', 'dispatched', 'failed')
             ORDER BY updated_at DESC, created_at DESC, id DESC
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load latest review dispatch for card {card_id}: {error}"))?
    } else {
        return Err("provide dispatch_id or card_id".to_string());
    };

    let Some(row) = row else {
        return Ok(None);
    };

    let loaded_dispatch_id: String = row
        .try_get("id")
        .map_err(|error| format!("decode dispatch id: {error}"))?;
    let loaded_card_id: Option<String> = row
        .try_get("kanban_card_id")
        .map_err(|error| format!("decode dispatch card id: {error}"))?;
    let Some(loaded_card_id) = loaded_card_id.filter(|value| !value.trim().is_empty()) else {
        return Err(format!(
            "dispatch {loaded_dispatch_id} is not attached to a card"
        ));
    };
    if let Some(expected_card_id) = card_id {
        if expected_card_id != loaded_card_id {
            return Err(format!(
                "dispatch {loaded_dispatch_id} belongs to card {loaded_card_id}, not {expected_card_id}"
            ));
        }
    }

    let dispatch_type: Option<String> = row
        .try_get("dispatch_type")
        .map_err(|error| format!("decode dispatch type: {error}"))?;
    if dispatch_type.as_deref() != Some("review") {
        return Err(format!(
            "review recovery only accepts review dispatches, got {}",
            dispatch_type.unwrap_or_else(|| "<none>".to_string())
        ));
    }

    let status: String = row
        .try_get("status")
        .map_err(|error| format!("decode dispatch status: {error}"))?;
    let context = serde_json::Value::Object(json_object_from_text(
        row.try_get("context")
            .map_err(|error| format!("decode dispatch context: {error}"))?,
    )?);

    Ok(Some(ReviewRecoveryDispatch {
        id: loaded_dispatch_id,
        card_id: loaded_card_id,
        status,
        context,
    }))
}

fn worktree_head(path: &str) -> Result<String, String> {
    if !std::path::Path::new(path).is_dir() {
        return Err(format!(
            "worktree_path does not exist or is not a directory: {path}"
        ));
    }
    services_layer::platform::git_head_commit(path)
        .ok_or_else(|| format!("cannot resolve git HEAD for worktree_path: {path}"))
}

fn branch_for_recovery_target(path: Option<&str>, commit: &str) -> Option<String> {
    let path = path?;
    let preferred = services_layer::platform::shell::git_branch_name(path);
    services_layer::platform::shell::git_branch_containing_commit(
        path,
        commit,
        preferred.as_deref(),
        None,
    )
    .or(preferred)
}

fn context_string_field<'a>(context: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    context
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

async fn active_other_review_dispatch_exists_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    dispatch_id: &str,
) -> Result<bool, String> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND id <> $2
               AND dispatch_type = 'review'
               AND status IN ('pending', 'dispatched')
         )",
    )
    .bind(card_id)
    .bind(dispatch_id)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("check active review dispatch conflict: {error}"))
}

async fn recover_review_target_pg(
    pool: &sqlx::PgPool,
    body: ReviewTargetRecoveryBody,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let dispatch_id = trimmed_optional(body.dispatch_id);
    let card_id = trimmed_optional(body.card_id);
    let worktree_path = trimmed_optional(body.worktree_path);
    let requested_commit = trimmed_optional(body.target_commit);
    let reason = trimmed_optional(body.reason)
        .unwrap_or_else(|| "manual review target recovery".to_string());

    // #3863: `target_commit` is attacker-influenceable (request body) and is
    // persisted as the `reviewed_commit` deploy-gate marker below. Reject any
    // non-SHA value with 400 before any Postgres read/write so a forged
    // "review passed" marker can never be recorded for an arbitrary commit.
    if let Some(commit) = requested_commit.as_deref() {
        validate_recovery_target_commit(commit)?;
    }

    let dispatch =
        load_review_recovery_dispatch_pg(pool, dispatch_id.as_deref(), card_id.as_deref())
            .await
            .map_err(|error| (StatusCode::BAD_REQUEST, error))?
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    "review dispatch not found".to_string(),
                )
            })?;

    let mut context_obj = dispatch.context.as_object().cloned().unwrap_or_default();
    let inferred_worktree_commit = if requested_commit.is_none() {
        worktree_path
            .as_deref()
            .map(worktree_head)
            .transpose()
            .map_err(|error| (StatusCode::BAD_REQUEST, error))?
    } else {
        None
    };
    let existing_commit =
        context_string_field(&dispatch.context, "reviewed_commit").map(str::to_string);
    let target_commit = requested_commit
        .or(inferred_worktree_commit)
        .or(existing_commit)
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "provide target_commit, worktree_path, or recover a dispatch with reviewed_commit in context".to_string(),
            )
        })?;

    // #3863 (codex re-review): validate the RESOLVED `target_commit` from EVERY
    // source — request body, inferred worktree HEAD, or stored dispatch context
    // (`reviewed_commit`) — immediately after resolution and BEFORE any git/shell
    // use. The early body-check above only covers a body-supplied value; when the
    // body omits `target_commit`, a poisoned stored/HEAD value (e.g.
    // `--output=/tmp/x`) would otherwise reach `commit_belongs_to_card_issue_pg`
    // below and be passed to `git log` as an argument (argument injection →
    // filesystem write) before the later re-validation runs. This guard closes
    // the resolved-before-git-log window for all sources.
    validate_recovery_target_commit(&target_commit)?;

    if let Some(path) = worktree_path.as_deref() {
        let head = worktree_head(path).map_err(|error| (StatusCode::BAD_REQUEST, error))?;
        if head != target_commit {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("worktree_path HEAD {head} does not match target_commit {target_commit}"),
            ));
        }
    }

    let validation_repo = worktree_path
        .as_deref()
        .or_else(|| context_string_field(&dispatch.context, "target_repo"))
        .or_else(|| context_string_field(&dispatch.context, "worktree_path"));
    let belongs = crate::dispatch::commit_belongs_to_card_issue_pg(
        pool,
        &dispatch.card_id,
        &target_commit,
        validation_repo,
    )
    .await;
    if !belongs {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "target_commit {target_commit} does not reference or belong to card {}",
                dispatch.card_id
            ),
        ));
    }

    if dispatch.status == "failed"
        && active_other_review_dispatch_exists_pg(pool, &dispatch.card_id, &dispatch.id)
            .await
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?
    {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "cannot requeue failed review dispatch {} while another active review dispatch exists for card {}",
                dispatch.id, dispatch.card_id
            ),
        ));
    }

    // #3863: defense-in-depth — re-validate the resolved `target_commit` right
    // before it is written into the dispatch context as the `reviewed_commit`
    // marker, regardless of source (request body, inferred worktree HEAD, or
    // stored dispatch context). Mirrors the verdict route's belt-and-suspenders
    // check in `stamp_review_passed_marker`.
    validate_recovery_target_commit(&target_commit)?;

    let previous_context = dispatch.context.clone();
    context_obj.insert("reviewed_commit".to_string(), json!(target_commit));
    if let Some(path) = worktree_path.as_deref() {
        context_obj.insert("worktree_path".to_string(), json!(path));
    } else if let Some(existing_path) = context_string_field(&dispatch.context, "worktree_path") {
        match worktree_head(existing_path) {
            Ok(head) if head == target_commit => {}
            _ => {
                context_obj.remove("worktree_path");
            }
        }
    }
    if let Some(branch) = branch_for_recovery_target(worktree_path.as_deref(), &target_commit) {
        context_obj.insert("branch".to_string(), json!(branch));
    }
    let cleared_markers = [
        context_obj.remove("review_target_reject_reason").is_some(),
        context_obj.remove("review_target_warning").is_some(),
    ]
    .into_iter()
    .filter(|removed| *removed)
    .count();

    let new_context = serde_json::Value::Object(context_obj);
    let from_status = dispatch.status.clone();
    let to_status = if dispatch.status == "failed" {
        "pending"
    } else {
        dispatch.status.as_str()
    };
    let payload = json!({
        "reason": reason,
        "previous_context": previous_context,
        "new_context": new_context,
        "cleared_failure_markers": cleared_markers,
        "target_commit": context_string_field(&new_context, "reviewed_commit"),
        "worktree_path": context_string_field(&new_context, "worktree_path"),
    });

    let mut tx = pool.begin().await.map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("begin recovery transaction: {error}"),
        )
    })?;

    sqlx::query(
        "UPDATE task_dispatches
         SET context = $1,
             status = $2,
             result = CASE WHEN status = 'failed' THEN NULL ELSE result END,
             completed_at = CASE WHEN status = 'failed' THEN NULL ELSE completed_at END,
             updated_at = NOW(),
             last_stuck_alert_at = NULL
         WHERE id = $3",
    )
    .bind(new_context.to_string())
    .bind(to_status)
    .bind(&dispatch.id)
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("update review dispatch target: {error}"),
        )
    })?;

    sqlx::query(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
         ) VALUES ($1, $2, 'review', $3, $4, 'manual_review_target_recovery', $5)",
    )
    .bind(&dispatch.id)
    .bind(&dispatch.card_id)
    .bind(&from_status)
    .bind(to_status)
    .bind(payload.clone())
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("insert dispatch recovery event: {error}"),
        )
    })?;

    sqlx::query(
        "INSERT INTO audit_logs (entity_type, entity_id, action, actor)
         VALUES ('task_dispatch', $1, 'review_target_recovered', 'operator')",
    )
    .bind(&dispatch.id)
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("insert recovery audit log: {error}"),
        )
    })?;

    tx.commit().await.map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("commit recovery transaction: {error}"),
        )
    })?;

    Ok(json!({
        "ok": true,
        "dispatch_id": dispatch.id,
        "card_id": dispatch.card_id,
        "from_status": from_status,
        "to_status": to_status,
        "target": {
            "reviewed_commit": context_string_field(&new_context, "reviewed_commit"),
            "worktree_path": context_string_field(&new_context, "worktree_path"),
            "branch": context_string_field(&new_context, "branch"),
        },
        "cleared_failure_markers": cleared_markers,
    }))
}

async fn update_decisions_pg(
    pool: &sqlx::PgPool,
    dispatch_id: &str,
    decisions: &[DecisionItem],
) -> Result<Vec<serde_json::Value>, String> {
    for item in decisions {
        let affected = sqlx::query(
            "UPDATE review_decisions
             SET decision = $1, decided_at = NOW()
             WHERE dispatch_id = $2 AND id = $3",
        )
        .bind(&item.decision)
        .bind(dispatch_id)
        .bind(item.item_id)
        .execute(pool)
        .await
        .map_err(|error| format!("update review_decisions: {error}"))?
        .rows_affected();

        if affected == 0 {
            sqlx::query(
                "INSERT INTO review_decisions (id, dispatch_id, decision, decided_at)
                 VALUES ($1, $2, $3, NOW())
                 ON CONFLICT (id) DO UPDATE SET
                    dispatch_id = EXCLUDED.dispatch_id,
                    decision = EXCLUDED.decision,
                    decided_at = EXCLUDED.decided_at",
            )
            .bind(item.item_id)
            .bind(dispatch_id)
            .bind(&item.decision)
            .execute(pool)
            .await
            .map_err(|error| format!("upsert review_decisions: {error}"))?;
        }
    }

    let rows = sqlx::query(
        "SELECT id, kanban_card_id, dispatch_id, item_index::BIGINT AS item_index, decision, decided_at::text AS decided_at
         FROM review_decisions
         WHERE dispatch_id = $1
         ORDER BY id",
    )
    .bind(dispatch_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load review_decisions: {error}"))?;

    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.try_get::<i64, _>("id").ok(),
                "kanban_card_id": row.try_get::<Option<String>, _>("kanban_card_id").ok().flatten(),
                "dispatch_id": row.try_get::<Option<String>, _>("dispatch_id").ok().flatten(),
                "item_index": row.try_get::<Option<i64>, _>("item_index").ok().flatten(),
                "decision": row.try_get::<Option<String>, _>("decision").ok().flatten(),
                "decided_at": row.try_get::<Option<String>, _>("decided_at").ok().flatten(),
            })
        })
        .collect())
}

async fn resolve_review_card_id_pg(
    pool: &sqlx::PgPool,
    dispatch_id: &str,
) -> Result<Option<String>, String> {
    if let Some(row) = sqlx::query(
        "SELECT kanban_card_id
         FROM review_decisions
         WHERE dispatch_id = $1
         LIMIT 1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load review decision card: {error}"))?
    {
        let card_id = row
            .try_get::<Option<String>, _>("kanban_card_id")
            .map_err(|error| format!("decode review decision card: {error}"))?;
        if card_id.is_some() {
            return Ok(card_id);
        }
    }

    let row = sqlx::query(
        "SELECT kanban_card_id
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load dispatch card: {error}"))?;

    match row {
        Some(row) => row
            .try_get::<Option<String>, _>("kanban_card_id")
            .map_err(|error| format!("decode dispatch card: {error}")),
        None => Ok(None),
    }
}

// ── Handlers ───────────────────────────────────────────────────

/// PATCH /api/kanban-reviews/:id/decisions
pub async fn update_decisions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateDecisionsBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    for item in &body.decisions {
        if !validate_review_decision(&item.decision) {
            return Err(AppError::bad_request(format!(
                "invalid decision '{}', must be 'accept' or 'reject'",
                item.decision
            )));
        }
    }

    let Some(pg_pool) = state.pg_pool_ref() else {
        return Err(AppError::internal("postgres pool unavailable").with_code(ErrorCode::Database));
    };

    let decisions = update_decisions_pg(pg_pool, &id, &body.decisions)
        .await
        .map_err(|error| AppError::internal(error).with_code(ErrorCode::Database))?;
    Ok((
        StatusCode::OK,
        Json(json!({"review": {"dispatch_id": id, "decisions": decisions}})),
    ))
}

/// POST /api/reviews/recovery
pub async fn recover_review_target(
    State(state): State<AppState>,
    Json(body): Json<ReviewTargetRecoveryBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pg_pool) = state.pg_pool_ref() else {
        return Err(AppError::internal("postgres pool unavailable").with_code(ErrorCode::Database));
    };

    let value = recover_review_target_pg(pg_pool, body)
        .await
        .map_err(|(status, error)| AppError::new(status, ErrorCode::Validation, error))?;
    Ok((StatusCode::OK, Json(value)))
}

/// POST /api/kanban-reviews/:id/trigger-rework
pub async fn trigger_rework(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pg_pool) = state.pg_pool_ref() else {
        return Err(AppError::internal("postgres pool unavailable").with_code(ErrorCode::Database));
    };

    let card_id = resolve_review_card_id_pg(pg_pool, &id)
        .await
        .map_err(|error| AppError::internal(error).with_code(ErrorCode::Database))?
        .ok_or_else(|| AppError::not_found("review or dispatch not found"))?;

    crate::kanban::transition_status_with_opts_pg_only(
        pg_pool,
        &state.engine,
        &card_id,
        "in_progress",
        "trigger-rework",
        crate::engine::transition::ForceIntent::OperatorOverride,
    )
    .await
    .map_err(|error| AppError::internal(format!("{error}")))?;
    Ok((StatusCode::OK, Json(json!({"ok": true}))))
}

#[cfg(test)]
mod recovery_commit_validation_tests {
    //! #3863: the /api/reviews/recovery route must apply the same SHA guard the
    //! verdict route uses, so a forged "review passed" marker cannot be recorded
    //! for an arbitrary `target_commit`.
    use super::{is_valid_commit_sha, validate_recovery_target_commit};
    use axum::http::StatusCode;

    #[test]
    fn accepts_valid_lowercase_sha() {
        // Abbreviated (7), full SHA-1 (40), and SHA-256 (64) bounds are accepted.
        assert!(validate_recovery_target_commit("0123abc").is_ok());
        assert!(
            validate_recovery_target_commit("1234567890abcdef1234567890abcdef12345678").is_ok()
        );
        assert!(validate_recovery_target_commit(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn rejects_invalid_target_commit_with_400_and_no_marker() {
        // Uppercase, too-short, non-hex, and path-traversal inputs must all be
        // rejected with HTTP 400. The guard returns `Err` before the route
        // reaches `pool.begin()`, so no `reviewed_commit` marker is ever
        // persisted (no db/fs write happens on the rejection path).
        for bad in [
            "ABCDEF0",                 // uppercase hex
            "abc123",                  // below the 7-char floor
            "zzzzzzz",                 // non-hex letters
            "../../etc",               // path traversal
            "../review_passed/forged", // marker-forgery attempt
            "/etc/passwd",             // absolute path
        ] {
            let err = validate_recovery_target_commit(bad)
                .expect_err("non-SHA target_commit must be rejected");
            assert_eq!(err.0, StatusCode::BAD_REQUEST, "input: {bad}");
            assert!(
                err.1.contains("^[0-9a-f]{7,64}$"),
                "input: {bad}, msg: {}",
                err.1
            );
        }
    }

    #[test]
    fn shares_predicate_with_verdict_route() {
        // The recovery guard is backed by the same `is_valid_commit_sha`
        // predicate the verdict route hardened in #3863 (single source of truth).
        assert!(is_valid_commit_sha("0123abc"));
        assert!(!is_valid_commit_sha("../../etc"));
    }

    #[test]
    fn rejects_resolved_commit_from_stored_context_or_head_before_git() {
        // #3863 (codex re-review): when `body.target_commit` is ABSENT, the
        // recovery handler resolves `target_commit` from the inferred worktree
        // HEAD or the stored dispatch context (`reviewed_commit`) — sources the
        // early body-check does NOT cover. `recover_review_target_pg` applies
        // `validate_recovery_target_commit(&target_commit)?` to that RESOLVED
        // value immediately after the
        // `requested_commit.or(inferred_worktree_commit).or(existing_commit)`
        // binding and BEFORE the first git/shell use,
        // `commit_belongs_to_card_issue_pg`, which runs `git log <commit>`
        // (commit passed as a positional argument with no `--` separator).
        //
        // Structural guarantee: because the guard returns `Err((400, _))` first,
        // a poisoned stored/HEAD value never reaches that `git log` call, so the
        // argument-injection vector (e.g. `git log … --output=/tmp/x` writing an
        // arbitrary file) cannot fire. The full handler needs a live PgPool, so
        // — like the existing recovery tests — we assert the guard predicate on
        // the exact poisoned values an attacker could plant in a stored context
        // or have a worktree HEAD resolve to.
        for poisoned in [
            "--output=/tmp/x",            // git log --output= → arbitrary file write
            "--upload-pack=/tmp/evil.sh", // option/command injection
            "--no-such-flag",             // any leading-dash option is rejected
            "../../etc",                  // path traversal
            "../review_passed/forged",    // marker-forgery attempt
        ] {
            let err = validate_recovery_target_commit(poisoned)
                .expect_err("poisoned resolved target_commit must be rejected pre-git");
            assert_eq!(err.0, StatusCode::BAD_REQUEST, "input: {poisoned}");
            assert!(
                err.1.contains("^[0-9a-f]{7,64}$"),
                "input: {poisoned}, msg: {}",
                err.1
            );
        }
    }
}
